//! Deterministic std.mysql ABI tests. A fake mysql.h/libmysqlclient exercises
//! MYSQL_STMT prepare/bind/execute/fetch without requiring a live database.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::fs;
use std::process::Command;

use native_harness::{compile_harness, emit_c, run_bounded, OutputLimits, TempDir, RUN_TIMEOUT};

const RUN_LIMITS: OutputLimits = OutputLimits::new(1024 * 1024, 2 * 1024 * 1024);

fn fixture() -> &'static str {
    r#"import mysql from "std.mysql"
fn main(): null! {
    client = mysql.client({
        host: "127.0.0.1",
        port: 3306,
        user: "tester",
        password: "secret",
        database: "fixture",
        max_connections: 2,
        max_waiters: 1,
        connect_timeout_ms: 1000,
        acquire_timeout_ms: 1000,
        query_timeout_ms: 1000
    })?
    payload = "x'; DROP TABLE users; --"
    result = client.query("SELECT ?, ?, ?", [payload.clone(), "", "你好"])?
    if (result.rows() != 2) panic("mysql row count")
    if (result.cols() != 3) panic("mysql column count")
    if (result.value(0, 0)? != payload) panic("mysql prepared payload")
    if (result.value(0, 1)? != "") panic("mysql empty value")
    if (result.value(0, 2)? != "你好") panic("mysql utf8 value")
    if (!result.is_null(1, 0)?) panic("mysql null flag")
    long_value = result.value(1, 1)?
    if (long_value.byte_len() != 300) panic("mysql truncated fetch")

    try {
        null_value = result.value(1, 0)?
        panic("mysql null was accepted as text")
    } catch(err) {
        if (err.domain != "mysql" || err.code != "null_value") panic("mysql null error")
    } finally {}
    try {
        out = result.value(9, 0)?
        panic("mysql out of bounds was accepted")
    } catch(err) {
        if (err.code != "index_out_of_bounds") panic("mysql bounds error")
    } finally {}
    try {
        mismatch = client.query("SELECT ?", [])?
        panic("mysql parameter mismatch was accepted")
    } catch(err) {
        if (err.code != "parameter_count") panic("mysql parameter count error")
    } finally {}
    try {
        broken = client.query("BROKEN ?", ["x"])?
        panic("mysql broken connection was accepted")
    } catch(err) {
        if (err.code != "execution_unknown") panic("mysql broken connection error")
    } finally {}
    try {
        failed = client.execute("SERVER_ERROR", [])?
        panic("mysql server error was accepted")
    } catch(err) {
        if (err.code != "query_error") panic("mysql server error classification")
    } finally {}
    recovered = client.query("SELECT ?, ?, ?", [payload.clone(), "", "你好"])?
    if (recovered.rows() != 2) panic("mysql pool did not replace broken connection")
    try {
        binary = client.query("SELECT BINARY ?, ?, ?", [payload.clone(), "", "你好"])?
        panic("mysql binary result was accepted")
    } catch(err) {
        if (err.code != "execution_unknown") panic("mysql binary error")
    } finally {}
    changed = client.execute("UPDATE fixture SET value = ?", ["safe"])?
    if (changed != 3) panic("mysql affected rows")
    client.close()
    println("mysql-fake-ok")
    return ok(null)
}
"#
}

#[test]
fn native_mysql_uses_only_server_prepared_statements_and_one_public_path() {
    let directory = TempDir::new("mysql-source");
    let generated = emit_c(directory.path(), fixture());
    for required in [
        "MYSQL_STMT* statement = mysql_stmt_init(connection)",
        "mysql_stmt_prepare(",
        "mysql_stmt_bind_param(statement, bindings)",
        "mysql_stmt_execute(statement)",
        "mysql_stmt_fetch_column(statement, &column, index, 0)",
        "static KuResult_mysql_client ku_mysql_client_new(KuObject* config)",
        "static KuResult_mysql_result ku_mysql_client_query(",
        "static KuResult_int ku_mysql_client_execute(",
        "KU_MYSQL_MAX_CONFIG_BYTES 65536ULL",
        "MYSQL_OPT_LOCAL_INFILE, &local_infile",
        "mysql_reset_connection(",
        "pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC)",
        "pthread_cond_timedwait_relative_np(",
        "SleepConditionVariableCS(",
        "mariadb_get_infov(",
        "MARIADB_CLIENT_VERSION_ID",
        "MARIADB_CLIENT_VERSION",
        "mysql_get_client_version()",
        "mysql_get_client_info()",
        "KU_MYSQL_LIBRARY_ABI_MISMATCH",
        "client_abi_mismatch",
        "#if defined(KU_MYSQL_SELECTED_HEADER)",
        "# include KU_MYSQL_SELECTED_HEADER",
        "KU_MYSQL_EXPECT_HEADER_FAMILY != KU_MYSQL_HEADER_FAMILY_MARIADB",
    ] {
        assert!(
            generated.contains(required),
            "missing prepared ABI: {required}"
        );
    }
    for forbidden in [
        "mysql_real_escape_string",
        "mysql_query(",
        "ku_mysql_query_params",
        "ku_mysql_connect(",
        "mysql_options(connection, MYSQL_OPT_RECONNECT",
    ] {
        assert!(
            !generated.contains(forbidden),
            "legacy interpolation/raw connection path survived: {forbidden}"
        );
    }
    for foreign_lexer in [
        "ku_pg_sql_next_top_token(",
        "ku_pg_sql_has_explicit_session_control(",
    ] {
        assert!(
            !generated.contains(foreign_lexer),
            "MySQL artifact must not depend on PostgreSQL SQL policy code: {foreign_lexer}"
        );
    }

    for (name, start, end) in [
        (
            "query",
            "static KuResult_mysql_result ku_mysql_client_query(",
            "static KuResult_int ku_mysql_client_execute(",
        ),
        (
            "execute",
            "static KuResult_int ku_mysql_client_execute(",
            "static int64_t ku_mysql_result_rows(",
        ),
    ] {
        let body = generated
            .split_once(start)
            .map(|(_, suffix)| suffix)
            .and_then(|suffix| suffix.split_once(end))
            .map(|(body, _)| body)
            .unwrap_or_else(|| panic!("generated MySQL {name} implementation"));
        let validation = body
            .find("ku_mysql_validate_statement_input")
            .unwrap_or_else(|| panic!("MySQL {name} input validation"));
        let thread_enter = body
            .find("ku_mysql_thread_enter()")
            .unwrap_or_else(|| panic!("MySQL {name} thread entry"));
        let acquire = body
            .find("ku_mysql_acquire(")
            .unwrap_or_else(|| panic!("MySQL {name} pool acquisition"));
        assert!(
            validation < thread_enter && thread_enter < acquire,
            "MySQL {name} must reject hostile input before thread or pool side effects"
        );
    }

    let acquire = generated
        .split_once("static MYSQL* ku_mysql_acquire(")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| suffix.split_once("static void ku_mysql_release("))
        .map(|(body, _)| body)
        .expect("generated MySQL acquire implementation");
    let acquire_loop = acquire
        .find("for (;;) {")
        .expect("MySQL acquire has a bounded retry loop");
    let deadline_check = acquire[acquire_loop..]
        .find("if (__ku_handler_now_ms() >= deadline)")
        .expect("MySQL acquire checks its absolute deadline every retry")
        + acquire_loop;
    let first_slot_scan = acquire[acquire_loop..]
        .find("for (size_t index = 0; index < client->max_connections; index++)")
        .expect("MySQL acquire scans bounded connection slots")
        + acquire_loop;
    assert!(
        deadline_check < first_slot_scan,
        "MySQL acquire must reject an expired waiter before handing out a slot"
    );
    assert!(acquire.contains("ku_mysql_release_backoff_timer_locked(client, owns_backoff_timer)"));
    assert!(acquire.contains("bool acquire_limited_connect = deadline <= connect_budget_deadline;"));

    let initialize = generated
        .split_once("static void ku_mysql_library_initialize_once(void) {")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| suffix.split_once("static bool ku_mysql_library_ready(void)"))
        .map(|(body, _)| body)
        .expect("generated POSIX MySQL library initialization");
    let abi_gate = initialize
        .find("ku_mysql_client_abi_compatible()")
        .expect("MySQL ABI gate runs during one-time initialization");
    let library_init = initialize
        .find("mysql_library_init(0, NULL, NULL)")
        .expect("MySQL library init call");
    assert!(
        library_init < abi_gate,
        "MySQL client library must initialize before any ABI probe"
    );
    assert_eq!(
        generated
            .matches(
                "} else if (!ku_mysql_client_abi_compatible()) {\n    mysql_library_end();\n    ku_mysql_library_status = KU_MYSQL_LIBRARY_ABI_MISMATCH;",
            )
            .count(),
        2,
        "both Windows and POSIX ABI mismatches must tear down the initialized library"
    );
}

#[test]
fn native_mysql_posix_sync_destroy_failures_stop_before_client_free() {
    let directory = TempDir::new("mysql-sync-destroy");
    let generated = emit_c(directory.path(), fixture());
    let signature = "static void ku_mysql_sync_destroy(KuMysqlClient* client) {";
    let destroy_start = generated
        .find(signature)
        .expect("generated MySQL synchronization destructor");
    let destroy_suffix = &generated[destroy_start..];
    let destroy_end = destroy_suffix
        .find("\n}\n")
        .expect("generated MySQL synchronization destructor end")
        + 3;
    let destroy = &destroy_suffix[..destroy_end];

    let condition_call = destroy
        .find("pthread_cond_destroy(&client->condition)")
        .expect("MySQL condition destructor call");
    let condition_failure = destroy
        .find("mysql client condition destroy failed")
        .expect("MySQL condition destruction failure guard");
    let mutex_call = destroy
        .find("pthread_mutex_destroy(&client->mutex)")
        .expect("MySQL mutex destructor call");
    let mutex_failure = destroy
        .find("mysql client mutex destroy failed")
        .expect("MySQL mutex destruction failure guard");
    assert!(
        condition_call < condition_failure
            && condition_failure < mutex_call
            && mutex_call < mutex_failure,
        "MySQL must validate condition destruction before attempting mutex destruction"
    );
    let client_destroy = generated
        .split_once("static void ku_mysql_client_destroy(KuMysqlClient* client) {")
        .expect("generated MySQL client destructor")
        .1
        .split_once("/* Called with the mutex held.")
        .expect("generated MySQL client destructor end")
        .0;
    let sync_destroy = client_destroy
        .find("ku_mysql_sync_destroy(client)")
        .expect("MySQL synchronization destruction before client free");
    let fields_free = client_destroy
        .find("ku_mysql_client_free_fields(client)")
        .expect("MySQL client field free");
    let owner_free = client_destroy
        .find("ku_mysql_free(client)")
        .expect("MySQL client owner free");
    assert!(
        sync_destroy < fields_free && fields_free < owner_free,
        "MySQL synchronization must be destroyed before freeing enclosing client allocations"
    );

    let harness = directory.path().join("mysql_sync_destroy_harness.c");
    fs::write(
        &harness,
        format!(
            r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#undef _WIN32
#endif
typedef struct KuMysqlClient {{ int mutex; int condition; }} KuMysqlClient;
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
  KuMysqlClient client = {{0}};
  ku_mysql_sync_destroy(&client);
  fputs("free\n", stdout);
  return 0;
}}
"#
        ),
    )
    .expect("write MySQL synchronization destruction harness");

    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-sync-destroy") else {
        eprintln!("skip: no C compiler available for MySQL synchronization destruction test");
        return;
    };
    for (case, success, stdout, stderr) in [
        (
            "condition",
            false,
            "condition\n",
            "mysql client condition destroy failed\n",
        ),
        (
            "mutex",
            false,
            "condition\nmutex\n",
            "mysql client mutex destroy failed\n",
        ),
        ("ok", true, "condition\nmutex\nfree\n", ""),
    ] {
        let mut command = Command::new(&executable);
        command.current_dir(directory.path()).arg(case);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS).unwrap_or_else(|error| {
            panic!("MySQL synchronization destruction case {case} was not bounded: {error}")
        });
        assert_eq!(
            output.status.success(),
            success,
            "unexpected MySQL synchronization destruction status for {case}"
        );
        let actual_stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
        let actual_stderr = String::from_utf8_lossy(&output.stderr).replace('\r', "");
        assert_eq!(actual_stdout, stdout);
        assert_eq!(actual_stderr, stderr);
        if !success {
            assert_eq!(output.status.code(), Some(1));
            assert!(
                !actual_stdout.contains("free\n"),
                "MySQL synchronization destruction failure continued to client free"
            );
        }
    }
}

#[test]
fn native_mysql_fake_stmt_roundtrip_and_recovery() {
    let directory = TempDir::new("mysql-fake");
    let generated = emit_c(directory.path(), fixture());
    fs::write(directory.path().join("mysql.h"), fake_mysql_header()).expect("write fake mysql.h");
    let harness = directory.path().join("mysql_harness.c");
    fs::write(
        &harness,
        format!(
            "#define KU_MYSQL_FAKE_CLIENT 1\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
            fake_mysql_source(),
            fake_mysql_roundtrip_harness()
        ),
    )
    .expect("write fake libmysqlclient harness");

    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-fake") else {
        eprintln!("skip: no C compiler available for MySQL fake ABI test");
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL fake ABI executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "MySQL fake ABI executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        "mysql-fake-ok"
    );
}

#[test]
fn native_mysql_pool_prioritizes_waiters_and_close_does_not_wait_for_borrowers() {
    let directory = TempDir::new("mysql-pool-close");
    let generated = emit_c(directory.path(), fixture())
        .replacen(
            "static void ku_mysql_wake_one(KuMysqlClient* client) {",
            "static int ku_test_mysql_wake_one_calls = 0;\nstatic void ku_mysql_wake_one(KuMysqlClient* client) { ku_test_mysql_wake_one_calls++;",
            1,
        )
        .replacen(
            "static int ku_mysql_wait_until(KuMysqlClient* client, unsigned long long deadline) {",
            "static int ku_test_mysql_wait_failure = 0;\nstatic int ku_mysql_wait_until(KuMysqlClient* client, unsigned long long deadline) { if (ku_test_mysql_wait_failure) { ku_test_mysql_wait_failure = 0; return -1; }",
            1,
        )
        .replacen(
            "static unsigned long long __ku_handler_now_ms(void) {",
            "static int ku_test_mysql_clock_enabled = 0;\nstatic unsigned long long ku_test_mysql_clock_ms = 0;\nstatic unsigned long long ku_test_mysql_connect_elapsed_ms = 0;\nstatic unsigned long long __ku_handler_now_ms(void) { if (ku_test_mysql_clock_enabled) return ku_test_mysql_clock_ms;",
            1,
        )
        .replacen(
            "MYSQL* connected = mysql_real_connect(",
            "if (ku_test_mysql_clock_enabled) ku_test_mysql_clock_ms += ku_test_mysql_connect_elapsed_ms;\n  MYSQL* connected = mysql_real_connect(",
            1,
        );
    fs::write(directory.path().join("mysql.h"), fake_mysql_header()).expect("write fake mysql.h");
    let harness = directory.path().join("mysql_pool_harness.c");
    fs::write(
        &harness,
        format!(
            "#define KU_MYSQL_FAKE_CLIENT 1\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
            fake_mysql_source(),
            fake_mysql_pool_harness()
        ),
    )
    .expect("write MySQL pool-close harness");
    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-pool-close") else {
        eprintln!("skip: no C compiler available for MySQL pool-close test");
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL pool-close executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "MySQL pool-close executable failed with {:?}:\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        "mysql-pool-close-ok"
    );
}

#[test]
fn native_mysql_library_once_and_oom_paths_are_recoverable() {
    let directory = TempDir::new("mysql-oom");
    let generated = emit_c(directory.path(), fixture());
    fs::write(directory.path().join("mysql.h"), fake_mysql_header()).expect("write fake mysql.h");
    let harness = directory.path().join("mysql_oom_harness.c");
    fs::write(
        &harness,
        format!(
            "#define KU_MYSQL_FAKE_CLIENT 1\n#define KU_MYSQL_TEST_ALLOCATOR 1\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
            fake_mysql_source(),
            fake_mysql_oom_and_library_harness()
        ),
    )
    .expect("write MySQL OOM harness");
    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-oom") else {
        eprintln!("skip: no C compiler available for MySQL OOM test");
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL OOM executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "MySQL OOM executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        "mysql-oom-ok"
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostics.is_empty(),
        "unexpected MySQL diagnostic: {diagnostics}"
    );
    for secret in ["secret", "DROP TABLE"] {
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(secret)
                && !diagnostics.contains(secret),
            "MySQL diagnostic leaked sensitive input: {secret}"
        );
    }
}

#[test]
fn native_mysql_library_init_failure_is_a_structured_sync_error() {
    let directory = TempDir::new("mysql-library-init-error");
    let generated = emit_c(directory.path(), fixture());
    assert!(generated.contains(
        "ku_mysql_error(\"sync_error\", \"MySQL client library initialization failed\")"
    ));
    assert!(!generated.contains("library_init_error"));
    fs::write(directory.path().join("mysql.h"), fake_mysql_header()).expect("write fake mysql.h");
    let harness = directory.path().join("mysql_library_error_harness.c");
    fs::write(
        &harness,
        format!(
            "#define KU_MYSQL_FAKE_CLIENT 1\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
            fake_mysql_source(),
            fake_mysql_library_failure_harness()
        ),
    )
    .expect("write MySQL library-init failure harness");
    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-library-error")
    else {
        eprintln!("skip: no C compiler available for MySQL library-init failure test");
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL library-init failure test was not bounded: {error}"));
    assert!(
        output.status.success(),
        "MySQL library-init failure test failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        "mysql-library-error-ok"
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected MySQL library-init diagnostic"
    );
}

#[test]
fn native_mysql_vendor_compatibility_gates_accept_matching_headers_and_runtimes() {
    let generated_directory = TempDir::new("mysql-mariadb-abi-match");
    let generated = emit_c(generated_directory.path(), fixture());
    fs::write(
        generated_directory.path().join("mysql.h"),
        fake_mysql_header(),
    )
    .expect("write fake mysql.h");

    for (
        name,
        definitions,
        expected_mariadb_version_calls,
        expected_mariadb_info_calls,
        expected_mysql_version_calls,
        expected_mysql_info_calls,
    ) in [
        (
            "mariadb-10-package-3",
            "#define KU_FAKE_MARIADB_HEADER_VERSION 101106\n#define KU_FAKE_MARIADB_RUNTIME_VERSION 101106L\n#define KU_FAKE_MARIADB_RUNTIME_INFO \"10.11.6\"",
            1,
            1,
            0,
            0,
        ),
        (
            "mariadb-11-package-3",
            "#define KU_FAKE_MARIADB_HEADER_VERSION 110602\n#define KU_FAKE_MARIADB_RUNTIME_VERSION 110602L\n#define KU_FAKE_MARIADB_RUNTIME_INFO \"11.6.2\"",
            1,
            1,
            0,
            0,
        ),
        (
            "oracle-8",
            "#define KU_FAKE_ORACLE_HEADER 1\n#define KU_FAKE_ORACLE_HEADER_VERSION 80029\n#define KU_FAKE_MYSQL_RUNTIME_VERSION 80029L\n#define KU_FAKE_MYSQL_RUNTIME_INFO \"8.0.29\"",
            0,
            0,
            1,
            1,
        ),
    ] {
        let harness = generated_directory
            .path()
            .join(format!("mysql_abi_{name}_harness.c"));
        fs::write(
            &harness,
            format!(
                "#define KU_MYSQL_FAKE_CLIENT 1\n{definitions}\n#define KU_FAKE_EXPECT_MARIADB_VERSION_CALLS {expected_mariadb_version_calls}L\n#define KU_FAKE_EXPECT_MARIADB_INFO_CALLS {expected_mariadb_info_calls}L\n#define KU_FAKE_EXPECT_MYSQL_VERSION_CALLS {expected_mysql_version_calls}L\n#define KU_FAKE_EXPECT_MYSQL_INFO_CALLS {expected_mysql_info_calls}L\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
                fake_mysql_source(),
                fake_mysql_abi_match_harness()
            ),
        )
        .expect("write fake MySQL compatibility-match harness");
        let Some(executable) = compile_harness(
            generated_directory.path(),
            &harness,
            &format!("mysql-abi-{name}"),
        ) else {
            eprintln!("skip: no C compiler available for MySQL compatibility-match test");
            return;
        };
        let mut command = Command::new(&executable);
        command.current_dir(generated_directory.path());
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .unwrap_or_else(|error| panic!("MySQL compatibility-match test was not bounded: {error}"));
        assert!(
            output.status.success(),
            "MySQL {name} compatibility-match test failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim_end(),
            "mysql-abi-match-ok"
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected MySQL compatibility-match diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn native_mysql_runtime_header_mismatches_fail_before_thread_or_connection_use() {
    let generated_directory = TempDir::new("mysql-abi-mismatch");
    let generated = emit_c(generated_directory.path(), fixture());
    fs::write(
        generated_directory.path().join("mysql.h"),
        fake_mysql_header(),
    )
    .expect("write fake mysql.h");

    for (
        name,
        definitions,
        expected_mariadb_version_calls,
        expected_mariadb_info_calls,
        expected_mysql_version_calls,
        expected_mysql_info_calls,
    ) in [
        (
            "mariadb-header-major",
            "#define KU_FAKE_MARIADB_HEADER_VERSION 101106\n#define KU_FAKE_MARIADB_RUNTIME_VERSION 110602L\n#define KU_FAKE_MARIADB_RUNTIME_INFO \"11.6.2\"",
            1,
            1,
            0,
            0,
        ),
        (
            "mariadb-numeric-info",
            "#define KU_FAKE_MARIADB_RUNTIME_VERSION 101106L\n#define KU_FAKE_MARIADB_RUNTIME_INFO \"11.1.6\"",
            1,
            1,
            0,
            0,
        ),
        (
            "mariadb-version-api-error",
            "#define KU_FAKE_MARIADB_FAIL_VERSION 1",
            1,
            0,
            0,
            0,
        ),
        (
            "mariadb-info-api-error",
            "#define KU_FAKE_MARIADB_FAIL_INFO 1",
            1,
            1,
            0,
            0,
        ),
        (
            "oracle-header-major",
            "#define KU_FAKE_ORACLE_HEADER 1\n#define KU_FAKE_ORACLE_HEADER_VERSION 80029\n#define KU_FAKE_MYSQL_RUNTIME_VERSION 90000L\n#define KU_FAKE_MYSQL_RUNTIME_INFO \"9.0.0\"",
            0,
            0,
            1,
            1,
        ),
        (
            "oracle-numeric-info",
            "#define KU_FAKE_ORACLE_HEADER 1\n#define KU_FAKE_ORACLE_HEADER_VERSION 80029\n#define KU_FAKE_MYSQL_RUNTIME_VERSION 80029L\n#define KU_FAKE_MYSQL_RUNTIME_INFO \"9.0.0\"",
            0,
            0,
            1,
            1,
        ),
        (
            "oracle-malformed-info",
            "#define KU_FAKE_ORACLE_HEADER 1\n#define KU_FAKE_ORACLE_HEADER_VERSION 80029\n#define KU_FAKE_MYSQL_RUNTIME_VERSION 80029L\n#define KU_FAKE_MYSQL_RUNTIME_INFO \"8\"",
            0,
            0,
            1,
            1,
        ),
    ] {
        let harness = generated_directory
            .path()
            .join(format!("mysql_abi_{name}_harness.c"));
        fs::write(
            &harness,
            format!(
                "#define KU_MYSQL_FAKE_CLIENT 1\n{definitions}\n#define KU_FAKE_EXPECT_MARIADB_VERSION_CALLS {expected_mariadb_version_calls}L\n#define KU_FAKE_EXPECT_MARIADB_INFO_CALLS {expected_mariadb_info_calls}L\n#define KU_FAKE_EXPECT_MYSQL_VERSION_CALLS {expected_mysql_version_calls}L\n#define KU_FAKE_EXPECT_MYSQL_INFO_CALLS {expected_mysql_info_calls}L\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
                fake_mysql_source(),
                fake_mysql_abi_mismatch_harness()
            ),
        )
        .expect("write fake MySQL ABI-mismatch harness");
        let Some(executable) = compile_harness(
            generated_directory.path(),
            &harness,
            &format!("mysql-abi-{name}"),
        ) else {
            eprintln!("skip: no C compiler available for MySQL ABI-mismatch test");
            return;
        };
        let mut command = Command::new(&executable);
        command.current_dir(generated_directory.path());
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .unwrap_or_else(|error| panic!("MySQL ABI-mismatch test was not bounded: {error}"));
        assert!(
            output.status.success(),
            "MySQL {name} ABI-mismatch test failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim_end(),
            "mysql-abi-mismatch-ok"
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected MySQL ABI-mismatch diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn native_mysql_cleanup_failures_discard_connection_slots() {
    let directory = TempDir::new("mysql-cleanup-failures");
    let generated = emit_c(directory.path(), fixture());
    fs::write(directory.path().join("mysql.h"), fake_mysql_header()).expect("write fake mysql.h");
    let harness = directory.path().join("mysql_cleanup_harness.c");
    fs::write(
        &harness,
        format!(
            "#define KU_MYSQL_FAKE_CLIENT 1\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
            fake_mysql_source(),
            fake_mysql_cleanup_harness()
        ),
    )
    .expect("write MySQL cleanup-failure harness");
    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-cleanup") else {
        eprintln!("skip: no C compiler available for MySQL cleanup-failure test");
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL cleanup executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "MySQL cleanup executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        "mysql-cleanup-ok"
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected MySQL cleanup diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_mysql_execution_outcomes_are_not_reported_as_retryable() {
    let directory = TempDir::new("mysql-outcomes");
    let generated = emit_c(directory.path(), fixture())
        .replacen(
            "static void ku_mysql_result_free(KuMysqlResult* result) {\n  if (!result) return;",
            "static size_t ku_test_mysql_result_free_calls = 0;\nstatic void ku_mysql_result_free(KuMysqlResult* result) {\n  if (!result) return;\n  ku_test_mysql_result_free_calls++;",
            1,
        )
        .replacen(
            "static unsigned long long __ku_handler_now_ms(void) {",
            "static int ku_test_mysql_clock_enabled = 0;\nstatic unsigned long long ku_test_mysql_clock_ms = 0;\nstatic unsigned long long ku_test_mysql_clock_calls = 0;\nstatic unsigned long long ku_test_mysql_clock_expire_after = 0;\nstatic unsigned long long __ku_handler_now_ms(void) { if (ku_test_mysql_clock_enabled) { ku_test_mysql_clock_calls++; return ku_test_mysql_clock_calls >= ku_test_mysql_clock_expire_after ? ku_test_mysql_clock_ms + 2 : ku_test_mysql_clock_ms; }",
            1,
        )
        .replacen(
            "static MYSQL* ku_mysql_acquire(\n    KuMysqlClient* client, size_t* slot_index, KuError* error,\n    unsigned long long operation_deadline) {",
            "static size_t ku_test_mysql_acquire_calls = 0;\nstatic MYSQL* ku_mysql_acquire(\n    KuMysqlClient* client, size_t* slot_index, KuError* error,\n    unsigned long long operation_deadline) {\n  ku_test_mysql_acquire_calls++;",
            1,
        );
    assert!(
        generated.contains("ku_test_mysql_result_free_calls++"),
        "MySQL outcome harness failed to instrument result destruction"
    );
    assert!(
        generated.contains("ku_test_mysql_clock_expire_after"),
        "MySQL outcome harness failed to instrument the monotonic clock"
    );
    assert!(
        generated.contains("ku_test_mysql_acquire_calls++"),
        "MySQL outcome harness failed to instrument pool acquisition"
    );
    fs::write(directory.path().join("mysql.h"), fake_mysql_header()).expect("write fake mysql.h");
    let harness = directory.path().join("mysql_outcomes_harness.c");
    fs::write(
        &harness,
        format!(
            "#define KU_MYSQL_FAKE_CLIENT 1\n#define main ku_fixture_main\n{generated}\n#undef main\n{}\n{}",
            fake_mysql_source(),
            fake_mysql_outcome_harness()
        ),
    )
    .expect("write fake MySQL outcome harness");

    let Some(executable) = compile_harness(directory.path(), &harness, "mysql-outcomes") else {
        eprintln!("skip: no C compiler available for MySQL outcome test");
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL outcome executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "MySQL outcome executable failed with {}:\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        "mysql-outcomes-ok"
    );
}

fn fake_mysql_roundtrip_harness() -> &'static str {
    r#"
static void roundtrip_verify_library_shutdown(void) {
  if (fake_atomic_load(&ku_fake_library_init_calls) == 1
      && fake_atomic_load(&ku_fake_library_end_calls) == 1
      && fake_atomic_load(&ku_fake_thread_active) == 0
      && fake_atomic_load(&ku_fake_thread_init_calls)
          == fake_atomic_load(&ku_fake_thread_end_calls)
      && fake_atomic_load(&ku_fake_connections_live) == 0
      && fake_atomic_load(&ku_fake_local_infile_disabled)
          == fake_atomic_load(&ku_fake_connections_opened)
      && fake_atomic_load(&ku_fake_statements_live) == 0) return;
  fputs("mysql roundtrip lifecycle imbalance\n", stderr);
  fflush(stderr);
#if defined(_WIN32)
  ExitProcess(91);
#else
  _Exit(91);
#endif
}

int main(void) {
  if (atexit(roundtrip_verify_library_shutdown) != 0) return 90;
  int status = ku_fixture_main();
  if (status != 0) return status;
  if (fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(&ku_fake_local_infile_disabled)
          != fake_atomic_load(&ku_fake_connections_opened)
      || fake_atomic_load(&ku_fake_thread_active) != 0
      || fake_atomic_load(&ku_fake_thread_init_calls)
          != fake_atomic_load(&ku_fake_thread_end_calls)) return 92;
  return 0;
}
"#
}

fn fake_mysql_cleanup_harness() -> &'static str {
    r#"
static KuString cleanup_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void cleanup_config_str(KuObject* config, const char* key, const char* value) {
  ku_object_set(config, cleanup_text(key), ku_v_str(cleanup_text(value)));
}
static void cleanup_config_int(KuObject* config, const char* key, int64_t value) {
  ku_object_set(config, cleanup_text(key), ku_v_int(value));
}
static KuObject* cleanup_config(void) {
  KuObject* config = ku_object_new(16);
  cleanup_config_str(config, "host", "127.0.0.1");
  cleanup_config_str(config, "user", "tester");
  cleanup_config_str(config, "password", "secret");
  cleanup_config_str(config, "database", "fixture");
  cleanup_config_int(config, "max_connections", 1);
  cleanup_config_int(config, "max_waiters", 1);
  cleanup_config_int(config, "connect_timeout_ms", 1000);
  cleanup_config_int(config, "acquire_timeout_ms", 1000);
  cleanup_config_int(config, "query_timeout_ms", 1000);
  return config;
}
static KuArray_str cleanup_params(void) {
  KuString values[3] = {
    cleanup_text("x'; DROP TABLE users; --"), cleanup_text(""), cleanup_text("你好")
  };
  return ku_array_make_str(3, values);
}
static bool cleanup_query_ok(KuMysqlClient* client) {
  KuArray_str params = cleanup_params();
  KuResult_mysql_result result = ku_mysql_client_query(
      client, cleanup_text("SELECT ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (!result.ok) {
    ku_error_drop(&result.error);
    return false;
  }
  bool valid = result.value && result.value->rows == 2 && result.value->cols == 3;
  ku_drop_mysql_result(&result.value);
  return valid;
}
static bool cleanup_pool_state(KuMysqlClient* client, bool has_connection) {
  bool matches;
  ku_mysql_lock(client);
  matches = client->active == 0 && client->waiters == 0
      && !client->slots[0].busy
      && (client->slots[0].connection != NULL) == has_connection;
  ku_mysql_unlock(client);
  return matches;
}
static int cleanup_failure_case(
    KuMysqlClient* client, KuFakeAtomicLong* injection, int code) {
  long opened_before = fake_atomic_load(&ku_fake_connections_opened);
  fake_atomic_store(injection, 1);
  if (!cleanup_query_ok(client)) return code;
  if (!cleanup_pool_state(client, false)
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(injection) != 0) return code + 1;
  if (fake_atomic_load(&ku_fake_connections_opened) != opened_before) return code + 2;

  if (!cleanup_query_ok(client)) return code + 3;
  if (!cleanup_pool_state(client, true)
      || fake_atomic_load(&ku_fake_connections_live) != 1
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(&ku_fake_connections_opened) != opened_before + 1) {
    return code + 4;
  }
  return 0;
}
static void cleanup_verify_library_shutdown(void) {
  if (fake_atomic_load(&ku_fake_library_init_calls) == 1
      && fake_atomic_load(&ku_fake_library_end_calls) == 1
      && fake_atomic_load(&ku_fake_thread_active) == 0
      && fake_atomic_load(&ku_fake_thread_init_calls)
          == fake_atomic_load(&ku_fake_thread_end_calls)
      && fake_atomic_load(&ku_fake_connections_live) == 0
      && fake_atomic_load(&ku_fake_statements_live) == 0) return;
  fputs("mysql cleanup lifecycle imbalance\n", stderr);
  fflush(stderr);
#if defined(_WIN32)
  ExitProcess(91);
#else
  _Exit(91);
#endif
}

int main(void) {
  if (atexit(cleanup_verify_library_shutdown) != 0) return 10;
  KuObject* config = cleanup_config();
  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (!opened.ok) return 11;
  KuMysqlClient* client = opened.value;
  if (!cleanup_pool_state(client, true)
      || fake_atomic_load(&ku_fake_connections_opened) != 1
      || fake_atomic_load(&ku_fake_connections_live) != 1) return 12;

  int failure = cleanup_failure_case(client, &ku_fake_fail_stmt_free_result, 20);
  if (failure) return failure;
  failure = cleanup_failure_case(client, &ku_fake_fail_stmt_close, 30);
  if (failure) return failure;
  failure = cleanup_failure_case(client, &ku_fake_fail_reset_connection, 40);
  if (failure) return failure;
  if (fake_atomic_load(&ku_fake_reset_connection_calls) != 4) return 50;

  ku_mysql_client_close(client);
  ku_mysql_thread_shutdown();
  if (fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(&ku_fake_thread_active) != 0
      || fake_atomic_load(&ku_fake_thread_init_calls)
          != fake_atomic_load(&ku_fake_thread_end_calls)) return 51;
  puts("mysql-cleanup-ok");
  return 0;
}
"#
}

fn fake_mysql_outcome_harness() -> &'static str {
    r#"
static KuString outcome_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void outcome_config_str(KuObject* config, const char* key, const char* value) {
  ku_object_set(config, outcome_text(key), ku_v_str(outcome_text(value)));
}
static void outcome_config_int(KuObject* config, const char* key, int64_t value) {
  ku_object_set(config, outcome_text(key), ku_v_int(value));
}
static KuObject* outcome_config(void) {
  KuObject* config = ku_object_new(16);
  outcome_config_str(config, "host", "127.0.0.1");
  outcome_config_str(config, "user", "tester");
  outcome_config_str(config, "password", "secret");
  outcome_config_str(config, "database", "fixture");
  outcome_config_int(config, "max_connections", 1);
  outcome_config_int(config, "max_waiters", 1);
  outcome_config_int(config, "connect_timeout_ms", 1000);
  outcome_config_int(config, "acquire_timeout_ms", 1000);
  outcome_config_int(config, "query_timeout_ms", 1000);
  return config;
}
static bool outcome_code(KuError error, const char* code) {
  return ku_string_equal(error.domain, outcome_text("mysql"))
      && ku_string_equal(error.code, outcome_text(code));
}
static bool outcome_message_is_non_retryable(KuError error) {
  const char* needle = "never retry automatically";
  size_t length = strlen(needle);
  if (error.message.len < length || (error.message.len && !error.message.ptr)) return false;
  for (size_t i = 0; i <= error.message.len - length; i++) {
    if (memcmp(error.message.ptr + i, needle, length) == 0) return true;
  }
  return false;
}
static bool outcome_pool_has_connection(KuMysqlClient* client, bool expected) {
  bool matches;
  ku_mysql_lock(client);
  matches = client->active == 0 && client->waiters == 0
      && !client->slots[0].busy
      && (client->slots[0].connection != NULL) == expected;
  ku_mysql_unlock(client);
  return matches;
}
static bool outcome_invalid_config_rejected_without_database(KuObject* config) {
  long mariadb_version_calls = fake_atomic_load(&ku_fake_mariadb_version_calls);
  long mariadb_info_calls = fake_atomic_load(&ku_fake_mariadb_info_calls);
  long client_version_calls = fake_atomic_load(&ku_fake_client_version_calls);
  long client_info_calls = fake_atomic_load(&ku_fake_client_info_calls);
  long probe_before_init_calls =
      fake_atomic_load(&ku_fake_probe_before_library_init_calls);
  long library_ready_for_probe =
      fake_atomic_load(&ku_fake_library_ready_for_probe);
  long library_init_calls = fake_atomic_load(&ku_fake_library_init_calls);
  long library_end_calls = fake_atomic_load(&ku_fake_library_end_calls);
  long thread_init_calls = fake_atomic_load(&ku_fake_thread_init_calls);
  long thread_end_calls = fake_atomic_load(&ku_fake_thread_end_calls);
  long thread_active = fake_atomic_load(&ku_fake_thread_active);
  long mysql_init_calls = fake_atomic_load(&ku_fake_mysql_init_calls);
  long real_connect_calls = fake_atomic_load(&ku_fake_real_connect_calls);
  long connections_opened = fake_atomic_load(&ku_fake_connections_opened);
  long connections_live = fake_atomic_load(&ku_fake_connections_live);
  long local_infile_disabled = fake_atomic_load(&ku_fake_local_infile_disabled);
  long reconnect_option_calls = fake_atomic_load(&ku_fake_reconnect_option_calls);
  long statements_live = fake_atomic_load(&ku_fake_statements_live);
  long statement_init_calls = fake_atomic_load(&ku_fake_stmt_init_calls);
  long statement_prepare_calls = fake_atomic_load(&ku_fake_stmt_prepare_calls);
  long execute_calls = fake_atomic_load(&ku_fake_execute_calls);
  long reset_calls = fake_atomic_load(&ku_fake_reset_connection_calls);
  size_t acquire_calls = ku_test_mysql_acquire_calls;
  size_t result_frees = ku_test_mysql_result_free_calls;
  int library_status = ku_mysql_library_status;
  unsigned int thread_depth = ku_mysql_thread_depth;
  bool thread_initialized = ku_mysql_thread_initialized;
  KuResult_mysql_client rejected = ku_mysql_client_new(config);
  bool expected_error = !rejected.ok && !rejected.value
      && outcome_code(rejected.error, "invalid_config");
  if (rejected.value) ku_drop_mysql_client(&rejected.value);
  if (!rejected.ok) ku_error_drop(&rejected.error);
  return expected_error
      && fake_atomic_load(&ku_fake_mariadb_version_calls) == mariadb_version_calls
      && fake_atomic_load(&ku_fake_mariadb_info_calls) == mariadb_info_calls
      && fake_atomic_load(&ku_fake_client_version_calls) == client_version_calls
      && fake_atomic_load(&ku_fake_client_info_calls) == client_info_calls
      && fake_atomic_load(&ku_fake_probe_before_library_init_calls)
          == probe_before_init_calls
      && fake_atomic_load(&ku_fake_library_ready_for_probe)
          == library_ready_for_probe
      && fake_atomic_load(&ku_fake_library_init_calls) == library_init_calls
      && fake_atomic_load(&ku_fake_library_end_calls) == library_end_calls
      && fake_atomic_load(&ku_fake_thread_init_calls) == thread_init_calls
      && fake_atomic_load(&ku_fake_thread_end_calls) == thread_end_calls
      && fake_atomic_load(&ku_fake_thread_active) == thread_active
      && fake_atomic_load(&ku_fake_mysql_init_calls) == mysql_init_calls
      && fake_atomic_load(&ku_fake_real_connect_calls) == real_connect_calls
      && fake_atomic_load(&ku_fake_connections_opened) == connections_opened
      && fake_atomic_load(&ku_fake_connections_live) == connections_live
      && fake_atomic_load(&ku_fake_local_infile_disabled) == local_infile_disabled
      && fake_atomic_load(&ku_fake_reconnect_option_calls) == reconnect_option_calls
      && fake_atomic_load(&ku_fake_statements_live) == statements_live
      && fake_atomic_load(&ku_fake_stmt_init_calls) == statement_init_calls
      && fake_atomic_load(&ku_fake_stmt_prepare_calls) == statement_prepare_calls
      && fake_atomic_load(&ku_fake_execute_calls) == execute_calls
      && fake_atomic_load(&ku_fake_reset_connection_calls) == reset_calls
      && ku_test_mysql_acquire_calls == acquire_calls
      && ku_test_mysql_result_free_calls == result_frees
      && ku_mysql_library_status == library_status
      && ku_mysql_thread_depth == thread_depth
      && ku_mysql_thread_initialized == thread_initialized;
}
static bool outcome_input_rejected_before_database(
    KuMysqlClient* client, KuString sql, KuArray_str params,
    const char* expected_code) {
  size_t acquire_calls = ku_test_mysql_acquire_calls;
  long execute_calls = fake_atomic_load(&ku_fake_execute_calls);
  long statement_init_calls = fake_atomic_load(&ku_fake_stmt_init_calls);
  long statement_prepare_calls = fake_atomic_load(&ku_fake_stmt_prepare_calls);
  long reset_calls = fake_atomic_load(&ku_fake_reset_connection_calls);
  long opened = fake_atomic_load(&ku_fake_connections_opened);
  long live = fake_atomic_load(&ku_fake_connections_live);
  long statements = fake_atomic_load(&ku_fake_statements_live);
  long thread_init_calls = fake_atomic_load(&ku_fake_thread_init_calls);
  long thread_end_calls = fake_atomic_load(&ku_fake_thread_end_calls);
  long thread_active = fake_atomic_load(&ku_fake_thread_active);
  unsigned int thread_depth = ku_mysql_thread_depth;
  bool thread_initialized = ku_mysql_thread_initialized;
  size_t result_frees = ku_test_mysql_result_free_calls;
  KuResult_int rejected_execute = ku_mysql_client_execute(client, sql, params);
  bool execute_error = !rejected_execute.ok
      && outcome_code(rejected_execute.error, expected_code);
  if (!rejected_execute.ok) ku_error_drop(&rejected_execute.error);
  unsigned long long execute_clock_calls = ku_test_mysql_clock_calls;
  if (ku_test_mysql_clock_enabled) ku_test_mysql_clock_calls = 0;
  KuResult_mysql_result rejected_query = ku_mysql_client_query(client, sql, params);
  bool query_error = !rejected_query.ok && !rejected_query.value
      && outcome_code(rejected_query.error, expected_code);
  if (rejected_query.value) ku_drop_mysql_result(&rejected_query.value);
  if (!rejected_query.ok) ku_error_drop(&rejected_query.error);
  bool both_deadlines_observed = !ku_test_mysql_clock_enabled
      || (execute_clock_calls >= ku_test_mysql_clock_expire_after
          && ku_test_mysql_clock_calls >= ku_test_mysql_clock_expire_after);
  return execute_error && query_error && both_deadlines_observed
      && ku_test_mysql_acquire_calls == acquire_calls
      && fake_atomic_load(&ku_fake_execute_calls) == execute_calls
      && fake_atomic_load(&ku_fake_stmt_init_calls) == statement_init_calls
      && fake_atomic_load(&ku_fake_stmt_prepare_calls) == statement_prepare_calls
      && fake_atomic_load(&ku_fake_reset_connection_calls) == reset_calls
      && fake_atomic_load(&ku_fake_connections_opened) == opened
      && fake_atomic_load(&ku_fake_connections_live) == live
      && fake_atomic_load(&ku_fake_statements_live) == statements
      && fake_atomic_load(&ku_fake_thread_init_calls) == thread_init_calls
      && fake_atomic_load(&ku_fake_thread_end_calls) == thread_end_calls
      && fake_atomic_load(&ku_fake_thread_active) == thread_active
      && ku_mysql_thread_depth == thread_depth
      && ku_mysql_thread_initialized == thread_initialized
      && ku_test_mysql_result_free_calls == result_frees
      && outcome_pool_has_connection(client, true);
}
static bool outcome_rejected_before_database(
    KuMysqlClient* client, const char* sql) {
  return outcome_input_rejected_before_database(
      client, outcome_text(sql), (KuArray_str){0},
      "session_state_unsupported");
}
static void outcome_test_clock_start(unsigned long long expire_after) {
  ku_test_mysql_clock_ms = 1000;
  ku_test_mysql_clock_calls = 0;
  ku_test_mysql_clock_expire_after = expire_after;
  ku_test_mysql_clock_enabled = 1;
}
static void outcome_test_clock_stop(void) {
  ku_test_mysql_clock_enabled = 0;
  ku_test_mysql_clock_calls = 0;
  ku_test_mysql_clock_expire_after = 0;
}
int main(void) {
  uint8_t embedded_nul[] = { 'b', 'a', 'd', 0, 'h', 'o', 's', 't' };
  uint8_t invalid_utf8[] = { 0xc3, 0x28 };
  const char* invalid_keys[] = {
    "port", "pool_size", "host", "user", "max_connections"
  };
  KuValue invalid_values[] = {
    ku_v_null(),
    ku_v_int(1),
    ku_v_str((KuString){
      embedded_nul, sizeof(embedded_nul), 0, KU_STRING_STATIC
    }),
    ku_v_str((KuString){
      invalid_utf8, sizeof(invalid_utf8), 0, KU_STRING_STATIC
    }),
    ku_v_int(KU_MYSQL_MAX_CONNECTIONS + 1)
  };
  for (size_t invalid_index = 0;
       invalid_index < sizeof(invalid_values) / sizeof(invalid_values[0]);
       invalid_index++) {
    KuObject* invalid_config = outcome_config();
    ku_object_set(
        invalid_config, outcome_text(invalid_keys[invalid_index]),
        invalid_values[invalid_index]);
    bool rejected_without_database =
        outcome_invalid_config_rejected_without_database(invalid_config);
    ku_object_drop(invalid_config);
    if (!rejected_without_database) return 1 + (int)invalid_index;
  }

  KuObject* config = outcome_config();
  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (!opened.ok) return 10;
  KuMysqlClient* client = opened.value;
  client->query_timeout_ms = 1;
  KuArray_str no_params = (KuArray_str){0};

  KuResult_int late = ku_mysql_client_execute(client, outcome_text("LATE"), no_params);
  if (late.ok || !outcome_code(late.error, "execution_completed_without_result")
      || !outcome_message_is_non_retryable(late.error)) return 11;
  ku_error_drop(&late.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 1
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 0
      || !outcome_pool_has_connection(client, false)) return 12;

  client->query_timeout_ms = 1000;
  KuResult_int broken = ku_mysql_client_execute(client, outcome_text("BROKEN"), no_params);
  if (broken.ok || !outcome_code(broken.error, "execution_unknown")
      || !outcome_message_is_non_retryable(broken.error)) return 13;
  ku_error_drop(&broken.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 2
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 0
      || !outcome_pool_has_connection(client, false)) return 14;

  KuResult_int client_error = ku_mysql_client_execute(client, outcome_text("CLIENT_ERROR"), no_params);
  if (client_error.ok || !outcome_code(client_error.error, "execution_unknown")
      || !outcome_message_is_non_retryable(client_error.error)) return 15;
  ku_error_drop(&client_error.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 3
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 0
      || !outcome_pool_has_connection(client, false)) return 16;

  KuResult_int rejected = ku_mysql_client_execute(client, outcome_text("SERVER_ERROR"), no_params);
  if (rejected.ok || !outcome_code(rejected.error, "query_error")) return 17;
  ku_error_drop(&rejected.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 4
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 1
      || !outcome_pool_has_connection(client, true)) return 18;

  KuString values[3] = {
    outcome_text("x'; DROP TABLE users; --"), outcome_text(""), outcome_text("你好")
  };
  KuArray_str params = ku_array_make_str(3, values);
  KuResult_mysql_result unsupported = ku_mysql_client_query(
      client, outcome_text("SELECT BINARY ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (unsupported.ok || !outcome_code(unsupported.error, "execution_unknown")
      || !outcome_message_is_non_retryable(unsupported.error)) return 19;
  ku_error_drop(&unsupported.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 5
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 1
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || !outcome_pool_has_connection(client, false)) return 20;

  params = ku_array_make_str(3, values);
  KuResult_mysql_result recovered = ku_mysql_client_query(
      client, outcome_text("SELECT ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (!recovered.ok || !recovered.value) return 21;
  ku_drop_mysql_result(&recovered.value);
  if (fake_atomic_load(&ku_fake_execute_calls) != 6
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 2
      || fake_atomic_load(&ku_fake_connections_live) != 1
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || !outcome_pool_has_connection(client, true)) return 22;

  if (!outcome_rejected_before_database(client, "BEGIN")) return 23;
  if (!outcome_rejected_before_database(
          client, " /* ordinary comment */ SeT autocommit = 0")) return 24;
  if (!outcome_rejected_before_database(
          client, " # MySQL line comment\nSET autocommit = 0")) return 49;
  if (!outcome_rejected_before_database(
          client, "/*!40101 SET autocommit = 0 */ UPDATE fixture")) return 25;
  if (!outcome_rejected_before_database(
          client, "/*M!100100 SET autocommit = 0 */ UPDATE fixture")) return 26;
  if (!outcome_rejected_before_database(
          client,
          "/* outer /* fake nested */ SET GLOBAL max_connections=1 # */")) return 27;
  if (!outcome_rejected_before_database(
          client, "CREATE OR REPLACE TEMPORARY TABLE transient_table(id INT)")) return 28;
  if (!outcome_rejected_before_database(
          client, "FLUSH TABLES WITH READ LOCK")) return 29;
  if (!outcome_rejected_before_database(client, "HANDLER records OPEN")) return 30;
  if (!outcome_rejected_before_database(client, "/* unterminated")) return 31;

  KuString oversized_sql = {
    (uint8_t*)"x", (size_t)KU_MYSQL_MAX_SQL_BYTES + 1, 0, KU_STRING_STATIC
  };
  if (!outcome_input_rejected_before_database(
          client, oversized_sql, no_params, "query_too_large")) return 32;
  uint8_t nul_sql_bytes[] = { 'S', 'E', 'L', 'E', 'C', 'T', ' ', 0, 0xff };
  KuString nul_sql = {
    nul_sql_bytes, sizeof(nul_sql_bytes), 0, KU_STRING_STATIC
  };
  if (!outcome_input_rejected_before_database(
          client, nul_sql, no_params, "query_error")) return 33;
  uint8_t invalid_sql_bytes[] = { 0xc3, 0x28 };
  KuString invalid_sql = {
    invalid_sql_bytes, sizeof(invalid_sql_bytes), 0, KU_STRING_STATIC
  };
  if (!outcome_input_rejected_before_database(
          client, invalid_sql, no_params, "invalid_utf8")) return 34;

  size_t long_length = 128 * 1024;
  uint8_t* long_bytes = (uint8_t*)malloc(long_length);
  if (!long_bytes) return 35;
  memset(long_bytes, ' ', long_length);
  KuString long_sql = {
    long_bytes, long_length, 0, KU_STRING_STATIC
  };
  client->query_timeout_ms = 1;
  outcome_test_clock_start(75);
  bool long_sql_timed_out = outcome_input_rejected_before_database(
      client, long_sql, no_params, "query_timeout");
  outcome_test_clock_stop();
  free(long_bytes);
  if (!long_sql_timed_out) return 36;

  long_bytes = (uint8_t*)malloc(long_length);
  if (!long_bytes) return 37;
  memset(long_bytes, 'p', long_length);
  KuString long_param = {
    long_bytes, long_length, 0, KU_STRING_STATIC
  };
  KuArray_str long_params = ku_array_make_str(1, &long_param);
  if (!long_params.data) { free(long_bytes); return 38; }
  outcome_test_clock_start(20);
  bool long_param_timed_out = outcome_input_rejected_before_database(
      client, outcome_text("SELECT ?"), long_params, "query_timeout");
  outcome_test_clock_stop();
  ku_array_drop_str(&long_params);
  free(long_bytes);
  if (!long_param_timed_out) return 39;
  client->query_timeout_ms = 1000;

  size_t result_frees = ku_test_mysql_result_free_calls;
  long opened_before_pollution = fake_atomic_load(&ku_fake_connections_opened);
  KuResult_mysql_result polluted = ku_mysql_client_query(
      client, outcome_text("POST_STATE_QUERY"), no_params);
  if (polluted.ok || polluted.value
      || !outcome_code(polluted.error, "session_state_unsupported")) return 40;
  ku_error_drop(&polluted.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 7
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 2
      || fake_atomic_load(&ku_fake_connections_opened) != opened_before_pollution
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || ku_test_mysql_result_free_calls != result_frees + 1
      || !outcome_pool_has_connection(client, false)) return 41;

  KuResult_int session_recovered = ku_mysql_client_execute(
      client, outcome_text("UPDATE recovered"), no_params);
  if (!session_recovered.ok) return 42;
  if (fake_atomic_load(&ku_fake_execute_calls) != 8
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 3
      || fake_atomic_load(&ku_fake_connections_opened) != opened_before_pollution + 1
      || fake_atomic_load(&ku_fake_connections_live) != 1
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || !outcome_pool_has_connection(client, true)) return 43;

  size_t result_frees_before_execute_pollution = ku_test_mysql_result_free_calls;
  long opened_before_execute_pollution = fake_atomic_load(&ku_fake_connections_opened);
  KuResult_int polluted_execute = ku_mysql_client_execute(
      client, outcome_text("POST_STATE_EXECUTE"), no_params);
  if (polluted_execute.ok || polluted_execute.value != 0
      || !outcome_code(polluted_execute.error, "session_state_unsupported")) return 44;
  ku_error_drop(&polluted_execute.error);
  if (fake_atomic_load(&ku_fake_execute_calls) != 9
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 3
      || fake_atomic_load(&ku_fake_connections_opened) != opened_before_execute_pollution
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || ku_test_mysql_result_free_calls != result_frees_before_execute_pollution
      || !outcome_pool_has_connection(client, false)) return 45;

  KuResult_int execute_recovered = ku_mysql_client_execute(
      client, outcome_text("UPDATE recovered again"), no_params);
  if (!execute_recovered.ok) return 46;
  if (fake_atomic_load(&ku_fake_execute_calls) != 10
      || fake_atomic_load(&ku_fake_reset_connection_calls) != 4
      || fake_atomic_load(&ku_fake_connections_opened) != opened_before_execute_pollution + 1
      || fake_atomic_load(&ku_fake_connections_live) != 1
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || !outcome_pool_has_connection(client, true)) return 47;

  ku_mysql_client_close(client);
  ku_mysql_thread_shutdown();
  if (fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(&ku_fake_thread_active) != 0) return 48;
  puts("mysql-outcomes-ok");
  return 0;
}
"#
}

fn fake_mysql_oom_and_library_harness() -> &'static str {
    r#"
typedef struct { int outcome; } KuFakeOpenWorker;

static KuString oom_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void oom_config_str(KuObject* config, const char* key, const char* value) {
  ku_object_set(config, oom_text(key), ku_v_str(oom_text(value)));
}
static void oom_config_int(KuObject* config, const char* key, int64_t value) {
  ku_object_set(config, oom_text(key), ku_v_int(value));
}
static KuObject* oom_config(void) {
  KuObject* config = ku_object_new(16);
  oom_config_str(config, "host", "127.0.0.1");
  oom_config_str(config, "user", "tester");
  oom_config_str(config, "password", "secret");
  oom_config_str(config, "database", "fixture");
  oom_config_int(config, "max_connections", 1);
  oom_config_int(config, "max_waiters", 1);
  oom_config_int(config, "connect_timeout_ms", 1000);
  oom_config_int(config, "acquire_timeout_ms", 1000);
  oom_config_int(config, "query_timeout_ms", 1000);
  return config;
}
static bool oom_contains(KuString text, const char* needle) {
  size_t needle_len = strlen(needle);
  if (!needle_len) return true;
  if (needle_len > text.len || (text.len && !text.ptr)) return false;
  for (size_t index = 0; index <= text.len - needle_len; index++) {
    if (memcmp(text.ptr + index, needle, needle_len) == 0) return true;
  }
  return false;
}
static bool oom_error_is_safe(KuError error, const char* code) {
  return ku_string_equal(error.domain, oom_text("mysql"))
      && ku_string_equal(error.code, oom_text(code))
      && !oom_contains(error.message, "secret")
      && !oom_contains(error.message, "DROP TABLE")
      && !oom_contains(error.message, "x'; DROP TABLE users; --");
}
static KuArray_str oom_params(void) {
  KuString values[3] = {
    oom_text("x'; DROP TABLE users; --"), oom_text(""), oom_text("你好")
  };
  return ku_array_make_str(3, values);
}
static bool oom_pool_released(KuMysqlClient* client, bool has_connection) {
  bool released;
  ku_mysql_lock(client);
  released = client->active == 0 && client->waiters == 0
      && (client->slots[0].connection != NULL) == has_connection
      && !client->slots[0].busy;
  ku_mysql_unlock(client);
  return released;
}

#if defined(_WIN32)
static DWORD WINAPI oom_open_worker(void* raw) {
#else
static void* oom_open_worker(void* raw) {
#endif
  KuFakeOpenWorker* worker = (KuFakeOpenWorker*)raw;
  KuObject* config = oom_config();
  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (!opened.ok) {
    worker->outcome = -1;
    ku_error_drop(&opened.error);
  } else {
    ku_mysql_client_close(opened.value);
    worker->outcome = 1;
  }
  ku_mysql_thread_shutdown();
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

static void oom_verify_library_shutdown(void) {
  if (fake_atomic_load(&ku_fake_library_init_calls) == 1
      && fake_atomic_load(&ku_fake_library_end_calls) == 1
      && fake_atomic_load(&ku_fake_thread_active) == 0
      && fake_atomic_load(&ku_fake_thread_init_calls)
          == fake_atomic_load(&ku_fake_thread_end_calls)
      && fake_atomic_load(&ku_fake_connections_live) == 0
      && fake_atomic_load(&ku_fake_statements_live) == 0) return;
  fprintf(stderr,
      "mysql fake lifecycle imbalance: library=%ld/%ld thread=%ld/%ld active=%ld connection=%ld statement=%ld\n",
      fake_atomic_load(&ku_fake_library_init_calls),
      fake_atomic_load(&ku_fake_library_end_calls),
      fake_atomic_load(&ku_fake_thread_init_calls),
      fake_atomic_load(&ku_fake_thread_end_calls),
      fake_atomic_load(&ku_fake_thread_active),
      fake_atomic_load(&ku_fake_connections_live),
      fake_atomic_load(&ku_fake_statements_live));
  fflush(stderr);
#if defined(_WIN32)
  ExitProcess(91);
#else
  _Exit(91);
#endif
}

int main(void) {
  if (atexit(oom_verify_library_shutdown) != 0) return 10;
  KuFakeOpenWorker workers[2] = {{0}, {0}};
#if defined(_WIN32)
  HANDLE threads[2] = {
    CreateThread(NULL, 0, oom_open_worker, &workers[0], 0, NULL),
    CreateThread(NULL, 0, oom_open_worker, &workers[1], 0, NULL)
  };
  if (!threads[0] || !threads[1]) return 11;
  if (WaitForMultipleObjects(2, threads, TRUE, 5000) != WAIT_OBJECT_0) return 12;
  CloseHandle(threads[0]); CloseHandle(threads[1]);
#else
  pthread_t threads[2];
  if (pthread_create(&threads[0], NULL, oom_open_worker, &workers[0]) != 0
      || pthread_create(&threads[1], NULL, oom_open_worker, &workers[1]) != 0) return 11;
  if (pthread_join(threads[0], NULL) != 0
      || pthread_join(threads[1], NULL) != 0) return 12;
#endif
  if (workers[0].outcome != 1 || workers[1].outcome != 1) return 13;
  if (fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_thread_active) != 0
      || fake_atomic_load(&ku_fake_thread_init_calls)
          != fake_atomic_load(&ku_fake_thread_end_calls)
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0) return 14;

  KuObject* config = oom_config();
  ku_mysql_test_fail_allocation_after(0);
  KuResult_mysql_client failed_open = ku_mysql_client_new(config);
  if (failed_open.ok || !oom_error_is_safe(failed_open.error, "out_of_memory")) return 20;
  ku_error_drop(&failed_open.error);

  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (!opened.ok) return 21;
  KuMysqlClient* client = opened.value;

  KuArray_str params = oom_params();
  ku_mysql_test_fail_allocation_after(0);
  KuResult_mysql_result param_oom = ku_mysql_client_query(
      client, oom_text("SELECT ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (param_oom.ok || !oom_error_is_safe(param_oom.error, "out_of_memory")) return 22;
  ku_error_drop(&param_oom.error);
  if (!oom_pool_released(client, true)
      || fake_atomic_load(&ku_fake_statements_live) != 0) return 23;

  params = oom_params();
  ku_mysql_test_fail_allocation_after(2);
  KuResult_mysql_result result_oom = ku_mysql_client_query(
      client, oom_text("SELECT ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (result_oom.ok || !oom_error_is_safe(result_oom.error, "execution_unknown")) return 24;
  ku_error_drop(&result_oom.error);
  if (!oom_pool_released(client, false)
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0) return 25;

  params = oom_params();
  KuResult_mysql_result queried = ku_mysql_client_query(
      client, oom_text("SELECT ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (!queried.ok || queried.value->rows != 2 || queried.value->cols != 3) return 26;

  ku_mysql_test_fail_allocation_after(0);
  KuResult_str value_oom = ku_mysql_result_value(queried.value, 0, 0);
  if (value_oom.ok || !oom_error_is_safe(value_oom.error, "out_of_memory")) return 27;
  ku_error_drop(&value_oom.error);
  KuResult_str value = ku_mysql_result_value(queried.value, 0, 0);
  if (!value.ok || !ku_string_equal(value.value, oom_text("x'; DROP TABLE users; --"))
      || queried.value->rows != 2 || queried.value->cols != 3) return 28;
  ku_string_drop(&value.value);
  ku_drop_mysql_result(&queried.value);
  ku_mysql_client_close(client);
  ku_mysql_thread_shutdown();
  if (fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(&ku_fake_thread_active) != 0
      || fake_atomic_load(&ku_fake_thread_init_calls)
          != fake_atomic_load(&ku_fake_thread_end_calls)) return 29;
  puts("mysql-oom-ok");
  return 0;
}
"#
}

fn fake_mysql_library_failure_harness() -> &'static str {
    r#"
static KuString library_error_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void library_error_config_str(
    KuObject* config, const char* key, const char* value) {
  ku_object_set(config, library_error_text(key), ku_v_str(library_error_text(value)));
}
static int library_error_code_is(KuError error, const char* code) {
  return ku_string_equal(error.domain, library_error_text("mysql"))
      && ku_string_equal(error.code, library_error_text(code))
      && error.domain.storage == KU_STRING_STATIC
      && error.code.storage == KU_STRING_STATIC
      && error.message.storage == KU_STRING_STATIC;
}
int main(void) {
  KuObject* config = ku_object_new(8);
  if (!config) return 10;
  library_error_config_str(config, "host", "127.0.0.1");
  library_error_config_str(config, "user", "tester");
  library_error_config_str(config, "password", "secret");
  library_error_config_str(config, "database", "fixture");
  fake_atomic_store(&ku_fake_fail_library_init, 1);
  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (opened.ok || opened.value
      || !library_error_code_is(opened.error, "sync_error")) return 11;
  ku_error_drop(&opened.error);
  if (fake_atomic_load(&ku_fake_fail_library_init) != 0
      || fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_library_end_calls) != 0
      || fake_atomic_load(&ku_fake_mariadb_version_calls) != 0
      || fake_atomic_load(&ku_fake_mariadb_info_calls) != 0
      || fake_atomic_load(&ku_fake_client_version_calls) != 0
      || fake_atomic_load(&ku_fake_client_info_calls) != 0
      || fake_atomic_load(&ku_fake_probe_before_library_init_calls) != 0
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_thread_active) != 0) return 12;
  puts("mysql-library-error-ok");
  return 0;
}
"#
}

fn fake_mysql_abi_match_harness() -> &'static str {
    r#"
static KuString abi_match_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void abi_match_config_str(
    KuObject* config, const char* key, const char* value) {
  ku_object_set(config, abi_match_text(key), ku_v_str(abi_match_text(value)));
}
int main(void) {
  KuObject* config = ku_object_new(8);
  if (!config) return 10;
  abi_match_config_str(config, "host", "127.0.0.1");
  abi_match_config_str(config, "user", "tester");
  abi_match_config_str(config, "password", "secret");
  abi_match_config_str(config, "database", "fixture");

  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (!opened.ok || !opened.value) return 11;
  ku_mysql_client_close(opened.value);
  ku_mysql_thread_shutdown();

  if (ku_mysql_library_status != KU_MYSQL_LIBRARY_READY
      || fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_library_end_calls) != 0
      || fake_atomic_load(&ku_fake_mariadb_version_calls)
          != KU_FAKE_EXPECT_MARIADB_VERSION_CALLS
      || fake_atomic_load(&ku_fake_mariadb_info_calls)
          != KU_FAKE_EXPECT_MARIADB_INFO_CALLS
      || fake_atomic_load(&ku_fake_client_version_calls)
          != KU_FAKE_EXPECT_MYSQL_VERSION_CALLS
      || fake_atomic_load(&ku_fake_client_info_calls)
          != KU_FAKE_EXPECT_MYSQL_INFO_CALLS
      || fake_atomic_load(&ku_fake_probe_before_library_init_calls) != 0
      || fake_atomic_load(&ku_fake_thread_init_calls) != 1
      || fake_atomic_load(&ku_fake_thread_end_calls) != 1
      || fake_atomic_load(&ku_fake_thread_active) != 0
      || fake_atomic_load(&ku_fake_mysql_init_calls) != 1
      || fake_atomic_load(&ku_fake_real_connect_calls) != 1
      || fake_atomic_load(&ku_fake_reconnect_option_calls) != 0
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0) return 12;
  puts("mysql-abi-match-ok");
  return 0;
}
"#
}

fn fake_mysql_abi_mismatch_harness() -> &'static str {
    r#"
static KuString abi_mismatch_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void abi_mismatch_config_str(
    KuObject* config, const char* key, const char* value) {
  ku_object_set(config, abi_mismatch_text(key), ku_v_str(abi_mismatch_text(value)));
}
static bool abi_mismatch_error_is_expected(KuError error) {
  return ku_string_equal(error.domain, abi_mismatch_text("mysql"))
      && ku_string_equal(error.code, abi_mismatch_text("client_abi_mismatch"))
      && error.domain.storage == KU_STRING_STATIC
      && error.code.storage == KU_STRING_STATIC
      && error.message.storage == KU_STRING_STATIC;
}
int main(void) {
  KuObject* config = ku_object_new(8);
  if (!config) return 10;
  abi_mismatch_config_str(config, "host", "127.0.0.1");
  abi_mismatch_config_str(config, "user", "tester");
  abi_mismatch_config_str(config, "password", "secret");
  abi_mismatch_config_str(config, "database", "fixture");

  KuResult_mysql_client first = ku_mysql_client_new(config);
  if (first.ok || first.value || !abi_mismatch_error_is_expected(first.error)) return 11;
  ku_error_drop(&first.error);
  KuResult_mysql_client second = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (second.ok || second.value || !abi_mismatch_error_is_expected(second.error)) return 12;
  ku_error_drop(&second.error);

  if (ku_mysql_library_status != KU_MYSQL_LIBRARY_ABI_MISMATCH
      || fake_atomic_load(&ku_fake_mariadb_version_calls)
          != KU_FAKE_EXPECT_MARIADB_VERSION_CALLS
      || fake_atomic_load(&ku_fake_mariadb_info_calls)
          != KU_FAKE_EXPECT_MARIADB_INFO_CALLS
      || fake_atomic_load(&ku_fake_client_version_calls)
          != KU_FAKE_EXPECT_MYSQL_VERSION_CALLS
      || fake_atomic_load(&ku_fake_client_info_calls)
          != KU_FAKE_EXPECT_MYSQL_INFO_CALLS
      || fake_atomic_load(&ku_fake_probe_before_library_init_calls) != 0
      || fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_library_end_calls) != 1
      || fake_atomic_load(&ku_fake_thread_init_calls) != 0
      || fake_atomic_load(&ku_fake_mysql_init_calls) != 0
      || fake_atomic_load(&ku_fake_real_connect_calls) != 0
      || fake_atomic_load(&ku_fake_stmt_init_calls) != 0
      || fake_atomic_load(&ku_fake_stmt_prepare_calls) != 0
      || fake_atomic_load(&ku_fake_execute_calls) != 0
      || fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0) return 13;
  puts("mysql-abi-mismatch-ok");
  return 0;
}
"#
}

fn fake_mysql_pool_harness() -> &'static str {
    r#"
typedef struct { KuMysqlClient* client; int outcome; } KuFakeWorker;

static KuString fake_text(const char* value) {
  return ku_string_static((const uint8_t*)value, strlen(value));
}
static void fake_config_str(KuObject* config, const char* key, const char* value) {
  ku_object_set(config, fake_text(key), ku_v_str(fake_text(value)));
}
static void fake_config_int(KuObject* config, const char* key, int64_t value) {
  ku_object_set(config, fake_text(key), ku_v_int(value));
}
static int fake_error_code(KuError error, const char* code) {
  return ku_string_equal(error.code, fake_text(code));
}
static KuArray_str fake_params(void) {
  KuString values[3] = {
    fake_text("x'; DROP TABLE users; --"), fake_text(""), fake_text("hello")
  };
  return ku_array_make_str(3, values);
}
static void pool_release_direct(KuMysqlClient* client, size_t slot_index) {
  ku_mysql_lock(client);
  if (slot_index < client->max_connections
      && client->slots[slot_index].busy) {
    client->slots[slot_index].busy = false;
    if (client->active) client->active--;
    ku_mysql_wake_one(client);
  }
  ku_mysql_unlock(client);
}
static int pool_restore_idle_connection(KuMysqlClient* client) {
  if (!ku_mysql_thread_enter()) return 30;
  KuError error = (KuError){0};
  size_t slot_index = SIZE_MAX;
  MYSQL* connection = ku_mysql_acquire(
      client, &slot_index, &error, __ku_handler_now_ms() + 1000);
  if (connection) pool_release_direct(client, slot_index);
  ku_mysql_thread_leave();
  if (connection) return 0;
  ku_error_drop(&error);
  return 31;
}
static int pool_assert_newcomer_cannot_claim(
    KuMysqlClient* client, bool empty_slot) {
  MYSQL* discard = NULL;
  ku_mysql_lock(client);
  if (empty_slot) {
    discard = client->slots[0].connection;
    client->slots[0].connection = NULL;
  }
  client->waiters = 1;
  ku_mysql_unlock(client);
  if (discard) mysql_close(discard);

  long opened_before = fake_atomic_load(&ku_fake_connections_opened);
  if (!ku_mysql_thread_enter()) return 32;
  KuError error = (KuError){0};
  size_t slot_index = SIZE_MAX;
  MYSQL* connection = ku_mysql_acquire(
      client, &slot_index, &error, __ku_handler_now_ms() + 1000);
  ku_mysql_thread_leave();

  ku_mysql_lock(client);
  bool unchanged = client->waiters == 1 && client->active == 0
      && !client->slots[0].busy
      && (client->slots[0].connection == NULL) == empty_slot;
  if (connection && slot_index < client->max_connections) {
    client->slots[slot_index].busy = false;
    if (client->active) client->active--;
  }
  client->waiters = 0;
  ku_mysql_unlock(client);

  int failure = 0;
  if (connection) failure = empty_slot ? 34 : 33;
  else if (!fake_error_code(error, "pool_busy")) failure = 35;
  if (!connection) ku_error_drop(&error);
  if (!unchanged && !failure) failure = 36;
  if (fake_atomic_load(&ku_fake_connections_opened) != opened_before
      && !failure) failure = 37;
  if (!failure && empty_slot) failure = pool_restore_idle_connection(client);
  return failure;
}
static int pool_assert_failed_connect_is_throttled(KuMysqlClient* client) {
  ku_mysql_lock(client);
  MYSQL* discard = client->slots[0].connection;
  client->slots[0].connection = NULL;
  ku_mysql_unlock(client);
  mysql_close(discard);

  long calls_before = fake_atomic_load(&ku_fake_real_connect_calls);
  fake_atomic_store(&ku_fake_fail_real_connect, 1);
  if (!ku_mysql_thread_enter()) return 38;
  KuError failed_error = (KuError){0};
  size_t failed_index = SIZE_MAX;
  MYSQL* failed = ku_mysql_acquire(
      client, &failed_index, &failed_error, __ku_handler_now_ms() + 1000);
  ku_mysql_thread_leave();
  if (failed) {
    pool_release_direct(client, failed_index);
    return 39;
  }
  bool connect_error = fake_error_code(failed_error, "connect_error");
  ku_error_drop(&failed_error);

  ku_mysql_lock(client);
  bool failure_recorded = client->consecutive_connect_failures == 1
      && client->reconnect_not_before_ms > 0
      && !client->connect_in_flight;
  client->reconnect_not_before_ms = __ku_handler_now_ms() + 1000;
  ku_mysql_unlock(client);
  if (!connect_error || !failure_recorded
      || fake_atomic_load(&ku_fake_real_connect_calls) != calls_before + 1) {
    return 40;
  }

  if (!ku_mysql_thread_enter()) return 41;
  KuError backoff_error = (KuError){0};
  size_t backoff_index = SIZE_MAX;
  MYSQL* during_backoff = ku_mysql_acquire(
      client, &backoff_index, &backoff_error, __ku_handler_now_ms() + 10);
  ku_mysql_thread_leave();
  if (during_backoff) {
    pool_release_direct(client, backoff_index);
    return 42;
  }
  bool backoff_timeout = fake_error_code(backoff_error, "acquire_timeout");
  ku_error_drop(&backoff_error);
  if (!backoff_timeout
      || fake_atomic_load(&ku_fake_real_connect_calls) != calls_before + 1) {
    return 43;
  }

  ku_mysql_lock(client);
  client->reconnect_not_before_ms = 0;
  client->connect_in_flight = true;
  ku_mysql_unlock(client);
  if (!ku_mysql_thread_enter()) return 44;
  KuError inflight_error = (KuError){0};
  size_t inflight_index = SIZE_MAX;
  MYSQL* during_inflight = ku_mysql_acquire(
      client, &inflight_index, &inflight_error, __ku_handler_now_ms() + 10);
  ku_mysql_thread_leave();
  if (during_inflight) {
    pool_release_direct(client, inflight_index);
    return 45;
  }
  bool inflight_timeout = fake_error_code(inflight_error, "acquire_timeout");
  ku_error_drop(&inflight_error);
  if (!inflight_timeout
      || fake_atomic_load(&ku_fake_real_connect_calls) != calls_before + 1) {
    return 46;
  }

  ku_mysql_lock(client);
  client->connect_in_flight = false;
  ku_mysql_unlock(client);
  if (!ku_mysql_thread_enter()) return 47;
  KuError recovered_error = (KuError){0};
  size_t recovered_index = SIZE_MAX;
  MYSQL* recovered = ku_mysql_acquire(
      client, &recovered_index, &recovered_error, __ku_handler_now_ms() + 1000);
  ku_mysql_thread_leave();
  if (!recovered) {
    ku_error_drop(&recovered_error);
    return 48;
  }
  ku_mysql_lock(client);
  bool reset = client->consecutive_connect_failures == 0
      && client->reconnect_not_before_ms == 0
      && !client->connect_in_flight && !client->backoff_timer_armed;
  ku_mysql_unlock(client);
  pool_release_direct(client, recovered_index);
  return reset && fake_atomic_load(&ku_fake_real_connect_calls) == calls_before + 2
      ? 0 : 49;
}
static int pool_assert_backoff_timer_is_handed_off(KuMysqlClient* client) {
  ku_mysql_lock(client);
  int wakes_before = ku_test_mysql_wake_one_calls;
  client->waiters = 2; /* current timer owner plus another blocked waiter */
  client->backoff_timer_armed = true;
  ku_mysql_release_backoff_timer_locked(client, true);
  bool handed_off = !client->backoff_timer_armed
      && ku_test_mysql_wake_one_calls == wakes_before + 1;
  client->waiters = 0;
  ku_mysql_unlock(client);
  return handed_off ? 0 : 50;
}
static int pool_assert_wait_failure_is_structured(KuMysqlClient* client) {
  ku_mysql_lock(client);
  client->slots[0].busy = true;
  client->active = 1;
  ku_mysql_unlock(client);
  ku_test_mysql_wait_failure = 1;
  KuError error = (KuError){0}; size_t slot_index = SIZE_MAX;
  MYSQL* connection = ku_mysql_acquire(
      client, &slot_index, &error, __ku_handler_now_ms() + 1000);
  ku_mysql_lock(client);
  client->slots[0].busy = false;
  client->active = 0;
  ku_mysql_unlock(client);
  int structured = !connection && fake_error_code(error, "sync_error")
      && !ku_test_mysql_wait_failure;
  ku_error_drop(&error);
  return structured ? 0 : 52;
}
static int pool_assert_equal_deadline_is_acquire_timeout(KuMysqlClient* client) {
  ku_mysql_lock(client);
  MYSQL* discard = client->slots[0].connection;
  client->slots[0].connection = NULL;
  client->reconnect_not_before_ms = 0;
  ku_mysql_unlock(client);
  if (!discard) return 53;
  mysql_close(discard);

  unsigned int saved_acquire_timeout = client->acquire_timeout_ms;
  unsigned int saved_connect_timeout = client->connect_timeout_ms;
  client->acquire_timeout_ms = 5;
  client->connect_timeout_ms = 5;
  ku_test_mysql_clock_ms = 1000;
  ku_test_mysql_connect_elapsed_ms = 5;
  ku_test_mysql_clock_enabled = 1;
  if (!ku_mysql_thread_enter()) return 54;
  KuError error = (KuError){0}; size_t slot_index = SIZE_MAX;
  MYSQL* connection = ku_mysql_acquire(
      client, &slot_index, &error, ku_test_mysql_clock_ms + 50);
  ku_mysql_thread_leave();
  ku_mysql_lock(client);
  bool equal_state_clean = client->active == 0 && client->waiters == 0
      && !client->slots[0].busy && !client->slots[0].connection
      && !client->connect_in_flight && client->consecutive_connect_failures == 1
      && client->reconnect_not_before_ms > ku_test_mysql_clock_ms;
  ku_mysql_record_connect_success_locked(client);
  ku_mysql_unlock(client);
  int classified = !connection && fake_error_code(error, "acquire_timeout")
      && ku_test_mysql_clock_ms == 1005 && equal_state_clean;
  ku_error_drop(&error);

  client->acquire_timeout_ms = 50;
  client->connect_timeout_ms = 5;
  ku_test_mysql_clock_ms = 2000;
  if (!ku_mysql_thread_enter()) return 56;
  error = (KuError){0}; slot_index = SIZE_MAX;
  connection = ku_mysql_acquire(
      client, &slot_index, &error, ku_test_mysql_clock_ms + 100);
  ku_mysql_thread_leave();
  ku_mysql_lock(client);
  bool connect_state_clean = client->active == 0 && client->waiters == 0
      && !client->slots[0].busy && !client->slots[0].connection
      && !client->connect_in_flight && client->consecutive_connect_failures == 1
      && client->reconnect_not_before_ms > ku_test_mysql_clock_ms;
  ku_mysql_unlock(client);
  int connect_classified = !connection && fake_error_code(error, "connect_timeout")
      && ku_test_mysql_clock_ms == 2005 && connect_state_clean;
  ku_error_drop(&error);

  ku_test_mysql_clock_enabled = 0;
  ku_test_mysql_connect_elapsed_ms = 0;
  client->acquire_timeout_ms = saved_acquire_timeout;
  client->connect_timeout_ms = saved_connect_timeout;
  ku_mysql_lock(client);
  client->reconnect_not_before_ms = 0;
  ku_mysql_unlock(client);
  if (!classified) return 55;
  if (!connect_classified) return 57;
  return pool_restore_idle_connection(client);
}
static void pool_verify_library_shutdown(void) {
  if (fake_atomic_load(&ku_fake_library_init_calls) == 1
      && fake_atomic_load(&ku_fake_library_end_calls) == 1
      && fake_atomic_load(&ku_fake_thread_active) == 0
      && fake_atomic_load(&ku_fake_thread_init_calls)
          == fake_atomic_load(&ku_fake_thread_end_calls)
      && fake_atomic_load(&ku_fake_connections_live) == 0
      && fake_atomic_load(&ku_fake_statements_live) == 0) return;
  fputs("mysql pool lifecycle imbalance\n", stderr);
  fflush(stderr);
#if defined(_WIN32)
  ExitProcess(91);
#else
  _Exit(91);
#endif
}

#if defined(_WIN32)
static DWORD WINAPI fake_worker(void* raw) {
#else
static void* fake_worker(void* raw) {
#endif
  KuFakeWorker* worker = (KuFakeWorker*)raw;
  KuArray_str params = fake_params();
  KuResult_mysql_result result = ku_mysql_client_query(
      worker->client, fake_text("BLOCK ?, ?, ?"), params);
  ku_array_drop_str(&params);
  if (result.ok) {
    worker->outcome = 1;
    ku_drop_mysql_result(&result.value);
  } else {
    worker->outcome = fake_error_code(result.error, "client_closed") ? 2 : -1;
    ku_error_drop(&result.error);
  }
  ku_mysql_thread_shutdown();
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

int main(void) {
  if (atexit(pool_verify_library_shutdown) != 0) return 9;
  KuObject* config = ku_object_new(16);
  fake_config_str(config, "host", "127.0.0.1");
  fake_config_str(config, "user", "tester");
  fake_config_str(config, "password", "secret");
  fake_config_str(config, "database", "fixture");
  fake_config_int(config, "max_connections", 1);
  fake_config_int(config, "max_waiters", 1);
  fake_config_int(config, "connect_timeout_ms", 1000);
  fake_config_int(config, "acquire_timeout_ms", 5000);
  fake_config_int(config, "query_timeout_ms", 10000);
  KuResult_mysql_client opened = ku_mysql_client_new(config);
  ku_object_drop(config);
  if (!opened.ok) return 10;
  KuMysqlClient* client = opened.value;
  KuResult_mysql_result null_query = ku_mysql_client_query(
      NULL, fake_text("SELECT 1"), (KuArray_str){0});
  KuResult_int null_execute = ku_mysql_client_execute(
      NULL, fake_text("SELECT 1"), (KuArray_str){0});
  if (null_query.ok || !fake_error_code(null_query.error, "client_closed")
      || null_execute.ok || !fake_error_code(null_execute.error, "client_closed")) {
    return 51;
  }
  ku_error_drop(&null_query.error); ku_error_drop(&null_execute.error);
  int priority_failure = pool_assert_newcomer_cannot_claim(client, false);
  if (!priority_failure) {
    priority_failure = pool_assert_newcomer_cannot_claim(client, true);
  }
  if (!priority_failure) {
    priority_failure = pool_assert_failed_connect_is_throttled(client);
  }
  if (!priority_failure) {
    priority_failure = pool_assert_backoff_timer_is_handed_off(client);
  }
  if (!priority_failure) {
    priority_failure = pool_assert_wait_failure_is_structured(client);
  }
  if (!priority_failure) {
    priority_failure = pool_assert_equal_deadline_is_acquire_timeout(client);
  }
  if (priority_failure) {
    ku_mysql_client_close(client);
    ku_mysql_thread_shutdown();
    return priority_failure;
  }
  KuFakeWorker active = {client, 0};
  KuFakeWorker waiting = {client, 0};
  fake_atomic_store(&ku_fake_block_execute, 1);

#if defined(_WIN32)
  HANDLE active_thread = CreateThread(NULL, 0, fake_worker, &active, 0, NULL);
  if (!active_thread) return 11;
#else
  pthread_t active_thread;
  if (pthread_create(&active_thread, NULL, fake_worker, &active) != 0) return 11;
#endif
  unsigned long long wait_deadline = __ku_handler_now_ms() + 5000;
  while (!fake_atomic_load(&ku_fake_execute_entered)
      && __ku_handler_now_ms() < wait_deadline) fake_pause();
  if (!fake_atomic_load(&ku_fake_execute_entered)) return 12;

#if defined(_WIN32)
  HANDLE waiting_thread = CreateThread(NULL, 0, fake_worker, &waiting, 0, NULL);
  if (!waiting_thread) return 13;
#else
  pthread_t waiting_thread;
  if (pthread_create(&waiting_thread, NULL, fake_worker, &waiting) != 0) return 13;
#endif
  bool waiter_registered = false;
  wait_deadline = __ku_handler_now_ms() + 5000;
  while (__ku_handler_now_ms() < wait_deadline) {
    ku_mysql_lock(client);
    waiter_registered = client->waiters == 1;
    ku_mysql_unlock(client);
    if (waiter_registered) break;
    fake_pause();
  }
  if (!waiter_registered) return 14;

  KuArray_str overflow_params = fake_params();
  KuResult_mysql_result overflow = ku_mysql_client_query(
      client, fake_text("BLOCK ?, ?, ?"), overflow_params);
  ku_array_drop_str(&overflow_params);
  if (overflow.ok || !fake_error_code(overflow.error, "pool_busy")) return 15;
  ku_error_drop(&overflow.error);

  unsigned long long close_start = __ku_handler_now_ms();
  ku_mysql_client_close(client);
  unsigned long long close_elapsed = __ku_handler_now_ms() - close_start;
  if (close_elapsed > 500) return 16;
  fake_atomic_store(&ku_fake_release_execute, 1);
#if defined(_WIN32)
  if (WaitForSingleObject(active_thread, 5000) != WAIT_OBJECT_0) return 17;
  if (WaitForSingleObject(waiting_thread, 5000) != WAIT_OBJECT_0) return 18;
  CloseHandle(active_thread); CloseHandle(waiting_thread);
#else
  if (pthread_join(active_thread, NULL) != 0) return 17;
  if (pthread_join(waiting_thread, NULL) != 0) return 18;
#endif
  if (active.outcome != 1 || waiting.outcome != 2) return 19;
  if (fake_atomic_load(&ku_fake_reset_connection_calls) != 0) return 20;
  ku_mysql_thread_shutdown();
  if (fake_atomic_load(&ku_fake_connections_live) != 0
      || fake_atomic_load(&ku_fake_statements_live) != 0
      || fake_atomic_load(&ku_fake_reconnect_option_calls) != 0
      || fake_atomic_load(&ku_fake_thread_active) != 0
      || fake_atomic_load(&ku_fake_thread_init_calls)
          != fake_atomic_load(&ku_fake_thread_end_calls)) return 21;
  puts("mysql-pool-close-ok");
  return 0;
}
"#
}

fn fake_mysql_header() -> &'static str {
    r#"#ifndef KU_FAKE_MYSQL_H
#define KU_FAKE_MYSQL_H
#include <stddef.h>
#include <stdbool.h>
#if !defined(KU_FAKE_ORACLE_HEADER)
#define MARIADB_BASE_VERSION "mariadb-10.11"
#ifndef KU_FAKE_MARIADB_HEADER_VERSION
#define KU_FAKE_MARIADB_HEADER_VERSION 101106
#endif
#define MARIADB_VERSION_ID KU_FAKE_MARIADB_HEADER_VERSION
#define MYSQL_VERSION_ID KU_FAKE_MARIADB_HEADER_VERSION
#define MARIADB_PACKAGE_VERSION "3.4.5"
#define MARIADB_PACKAGE_VERSION_ID 30405
#else
#ifndef KU_FAKE_ORACLE_HEADER_VERSION
#define KU_FAKE_ORACLE_HEADER_VERSION 80029
#endif
#define MYSQL_VERSION_ID KU_FAKE_ORACLE_HEADER_VERSION
#endif
#define CR_MIN_ERROR 2000
#define CR_MAX_ERROR 2999
#define CER_MIN_ERROR 5000
#define CER_MAX_ERROR 5999
typedef unsigned char my_bool;
typedef unsigned long long my_ulonglong;
typedef struct st_mysql {
  int broken;
  long identity;
  unsigned int server_status;
} MYSQL;
typedef struct st_mysql_stmt MYSQL_STMT;
typedef struct st_mysql_res MYSQL_RES;
enum enum_field_types {
  MYSQL_TYPE_BIT = 16, MYSQL_TYPE_STRING = 254, MYSQL_TYPE_VAR_STRING = 253,
  MYSQL_TYPE_BLOB = 252, MYSQL_TYPE_TINY_BLOB = 249,
  MYSQL_TYPE_MEDIUM_BLOB = 250, MYSQL_TYPE_LONG_BLOB = 251,
  MYSQL_TYPE_GEOMETRY = 255
};
typedef struct st_mysql_bind {
  enum enum_field_types buffer_type;
  void* buffer;
  unsigned long buffer_length;
  unsigned long* length;
  my_bool* is_null;
  my_bool* error;
} MYSQL_BIND;
typedef struct st_mysql_field {
  enum enum_field_types type;
  unsigned int charsetnr;
} MYSQL_FIELD;
#if !defined(KU_FAKE_ORACLE_HEADER)
enum mariadb_value {
  MARIADB_CLIENT_VERSION,
  MARIADB_CLIENT_VERSION_ID
};
#endif
enum mysql_option {
  MYSQL_OPT_CONNECT_TIMEOUT, MYSQL_OPT_READ_TIMEOUT, MYSQL_OPT_WRITE_TIMEOUT,
  MYSQL_OPT_LOCAL_INFILE, MYSQL_OPT_RECONNECT, MYSQL_SET_CHARSET_NAME
};
#define MYSQL_NO_DATA 100
#define MYSQL_DATA_TRUNCATED 101
const char* mysql_get_client_info(void);
unsigned long mysql_get_client_version(void);
#if !defined(KU_FAKE_ORACLE_HEADER)
my_bool mariadb_get_infov(MYSQL*, enum mariadb_value, void*, ...);
#endif
int mysql_library_init(int, char**, char**);
void mysql_library_end(void);
MYSQL* mysql_init(MYSQL*);
int mysql_thread_init(void);
void mysql_thread_end(void);
int mysql_options(MYSQL*, enum mysql_option, const void*);
MYSQL* mysql_real_connect(MYSQL*, const char*, const char*, const char*,
                          const char*, unsigned int, const char*, unsigned long);
int mysql_set_character_set(MYSQL*, const char*);
int mysql_reset_connection(MYSQL*);
void mysql_close(MYSQL*);
MYSQL_STMT* mysql_stmt_init(MYSQL*);
int mysql_stmt_prepare(MYSQL_STMT*, const char*, unsigned long);
unsigned int mysql_stmt_errno(MYSQL_STMT*);
unsigned long mysql_stmt_param_count(MYSQL_STMT*);
int mysql_stmt_bind_param(MYSQL_STMT*, MYSQL_BIND*);
int mysql_stmt_execute(MYSQL_STMT*);
unsigned int mysql_stmt_field_count(MYSQL_STMT*);
MYSQL_RES* mysql_stmt_result_metadata(MYSQL_STMT*);
MYSQL_FIELD* mysql_fetch_fields(MYSQL_RES*);
void mysql_free_result(MYSQL_RES*);
int mysql_stmt_bind_result(MYSQL_STMT*, MYSQL_BIND*);
int mysql_stmt_fetch(MYSQL_STMT*);
int mysql_stmt_fetch_column(MYSQL_STMT*, MYSQL_BIND*, unsigned int, unsigned long);
my_bool mysql_stmt_free_result(MYSQL_STMT*);
my_bool mysql_stmt_close(MYSQL_STMT*);
my_ulonglong mysql_stmt_affected_rows(MYSQL_STMT*);
#endif
"#
}

fn fake_mysql_source() -> &'static str {
    r#"#include "mysql.h"
#include <stdlib.h>
#include <string.h>

#if defined(_WIN32)
typedef volatile LONG KuFakeAtomicLong;
static long fake_atomic_load(KuFakeAtomicLong* value) {
  return (long)InterlockedCompareExchange(value, 0, 0);
}
static void fake_atomic_store(KuFakeAtomicLong* value, long next) {
  InterlockedExchange(value, (LONG)next);
}
static long fake_atomic_exchange(KuFakeAtomicLong* value, long next) {
  return (long)InterlockedExchange(value, (LONG)next);
}
static long fake_atomic_add(KuFakeAtomicLong* value, long amount) {
  return (long)InterlockedExchangeAdd(value, (LONG)amount) + amount;
}
#else
#include <stdatomic.h>
typedef _Atomic long KuFakeAtomicLong;
static long fake_atomic_load(KuFakeAtomicLong* value) {
  return atomic_load_explicit(value, memory_order_acquire);
}
static void fake_atomic_store(KuFakeAtomicLong* value, long next) {
  atomic_store_explicit(value, next, memory_order_release);
}
static long fake_atomic_exchange(KuFakeAtomicLong* value, long next) {
  return atomic_exchange_explicit(value, next, memory_order_acq_rel);
}
static long fake_atomic_add(KuFakeAtomicLong* value, long amount) {
  return atomic_fetch_add_explicit(value, amount, memory_order_acq_rel) + amount;
}
#endif

struct st_mysql_stmt {
  MYSQL* connection;
  char* sql;
  unsigned long param_count;
  MYSQL_BIND* params;
  MYSQL_BIND* outputs;
  unsigned int field_count;
  unsigned int error_code;
  size_t row;
};
struct st_mysql_res { MYSQL_FIELD fields[3]; };
#ifndef KU_FAKE_MYSQL_RUNTIME_VERSION
#define KU_FAKE_MYSQL_RUNTIME_VERSION 30405L
#endif
#ifndef KU_FAKE_MYSQL_RUNTIME_INFO
#define KU_FAKE_MYSQL_RUNTIME_INFO "3.4.5"
#endif
#ifndef KU_FAKE_MARIADB_HEADER_VERSION
#define KU_FAKE_MARIADB_HEADER_VERSION 101106
#endif
#ifndef KU_FAKE_MARIADB_RUNTIME_VERSION
#define KU_FAKE_MARIADB_RUNTIME_VERSION KU_FAKE_MARIADB_HEADER_VERSION
#endif
#ifndef KU_FAKE_MARIADB_RUNTIME_INFO
#define KU_FAKE_MARIADB_RUNTIME_INFO "10.11.6"
#endif
#ifndef KU_FAKE_MARIADB_FAIL_VERSION
#define KU_FAKE_MARIADB_FAIL_VERSION 0
#endif
#ifndef KU_FAKE_MARIADB_FAIL_INFO
#define KU_FAKE_MARIADB_FAIL_INFO 0
#endif
static KuFakeAtomicLong ku_fake_client_info_calls = 0;
static KuFakeAtomicLong ku_fake_client_version_calls = 0;
static KuFakeAtomicLong ku_fake_client_version = KU_FAKE_MYSQL_RUNTIME_VERSION;
static const char* ku_fake_client_info = KU_FAKE_MYSQL_RUNTIME_INFO;
static KuFakeAtomicLong ku_fake_mariadb_version_calls = 0;
static KuFakeAtomicLong ku_fake_mariadb_info_calls = 0;
static KuFakeAtomicLong ku_fake_mariadb_version = KU_FAKE_MARIADB_RUNTIME_VERSION;
static const char* ku_fake_mariadb_info = KU_FAKE_MARIADB_RUNTIME_INFO;
static KuFakeAtomicLong ku_fake_fail_mariadb_version = KU_FAKE_MARIADB_FAIL_VERSION;
static KuFakeAtomicLong ku_fake_fail_mariadb_info = KU_FAKE_MARIADB_FAIL_INFO;
static KuFakeAtomicLong ku_fake_probe_before_library_init_calls = 0;
static KuFakeAtomicLong ku_fake_library_ready_for_probe = 0;
static KuFakeAtomicLong ku_fake_block_execute = 0;
static KuFakeAtomicLong ku_fake_execute_entered = 0;
static KuFakeAtomicLong ku_fake_release_execute = 0;
static KuFakeAtomicLong ku_fake_library_init_calls = 0;
static KuFakeAtomicLong ku_fake_library_end_calls = 0;
static KuFakeAtomicLong ku_fake_fail_library_init = 0;
static KuFakeAtomicLong ku_fake_thread_init_calls = 0;
static KuFakeAtomicLong ku_fake_thread_end_calls = 0;
static KuFakeAtomicLong ku_fake_thread_active = 0;
static KuFakeAtomicLong ku_fake_mysql_init_calls = 0;
static KuFakeAtomicLong ku_fake_connections_live = 0;
static KuFakeAtomicLong ku_fake_connections_opened = 0;
static KuFakeAtomicLong ku_fake_real_connect_calls = 0;
static KuFakeAtomicLong ku_fake_fail_real_connect = 0;
static KuFakeAtomicLong ku_fake_local_infile_disabled = 0;
static KuFakeAtomicLong ku_fake_reconnect_option_calls = 0;
static KuFakeAtomicLong ku_fake_statements_live = 0;
static KuFakeAtomicLong ku_fake_stmt_init_calls = 0;
static KuFakeAtomicLong ku_fake_stmt_prepare_calls = 0;
static KuFakeAtomicLong ku_fake_execute_calls = 0;
static KuFakeAtomicLong ku_fake_fail_stmt_free_result = 0;
static KuFakeAtomicLong ku_fake_fail_stmt_close = 0;
static KuFakeAtomicLong ku_fake_fail_reset_connection = 0;
static KuFakeAtomicLong ku_fake_reset_connection_calls = 0;

const char* mysql_get_client_info(void) {
  fake_atomic_add(&ku_fake_client_info_calls, 1);
  if (fake_atomic_load(&ku_fake_library_ready_for_probe) != 1
      || fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_library_end_calls) != 0) {
    fake_atomic_add(&ku_fake_probe_before_library_init_calls, 1);
    return NULL;
  }
  return ku_fake_client_info;
}
unsigned long mysql_get_client_version(void) {
  fake_atomic_add(&ku_fake_client_version_calls, 1);
  if (fake_atomic_load(&ku_fake_library_ready_for_probe) != 1
      || fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_library_end_calls) != 0) {
    fake_atomic_add(&ku_fake_probe_before_library_init_calls, 1);
    return 0;
  }
  return (unsigned long)fake_atomic_load(&ku_fake_client_version);
}
#if !defined(KU_FAKE_ORACLE_HEADER)
my_bool mariadb_get_infov(
    MYSQL* mysql, enum mariadb_value value, void* output, ...) {
  (void)mysql;
  if (!output) return 1;
  if (fake_atomic_load(&ku_fake_library_ready_for_probe) != 1
      || fake_atomic_load(&ku_fake_library_init_calls) != 1
      || fake_atomic_load(&ku_fake_library_end_calls) != 0) {
    fake_atomic_add(&ku_fake_probe_before_library_init_calls, 1);
    return 1;
  }
  if (value == MARIADB_CLIENT_VERSION_ID) {
    fake_atomic_add(&ku_fake_mariadb_version_calls, 1);
    if (fake_atomic_exchange(&ku_fake_fail_mariadb_version, 0)) return 1;
    *(size_t*)output = (size_t)fake_atomic_load(&ku_fake_mariadb_version);
    return 0;
  }
  if (value == MARIADB_CLIENT_VERSION) {
    fake_atomic_add(&ku_fake_mariadb_info_calls, 1);
    if (fake_atomic_exchange(&ku_fake_fail_mariadb_info, 0)) return 1;
    *(const char**)output = ku_fake_mariadb_info;
    return 0;
  }
  return 1;
}
#endif
int mysql_library_init(int argc, char** argv, char** groups) {
  (void)argc; (void)argv; (void)groups;
  fake_atomic_add(&ku_fake_library_init_calls, 1);
  int failed = (int)fake_atomic_exchange(&ku_fake_fail_library_init, 0);
  if (!failed) fake_atomic_store(&ku_fake_library_ready_for_probe, 1);
  return failed;
}
void mysql_library_end(void) {
  fake_atomic_store(&ku_fake_library_ready_for_probe, 0);
  fake_atomic_add(&ku_fake_library_end_calls, 1);
}

static void fake_pause(void) {
#if defined(_WIN32)
  Sleep(1);
#else
  struct timespec wait = {0, 1000000L};
  nanosleep(&wait, NULL);
#endif
}

MYSQL* mysql_init(MYSQL* unused) {
  (void)unused;
  fake_atomic_add(&ku_fake_mysql_init_calls, 1);
  MYSQL* connection = (MYSQL*)calloc(1, sizeof(MYSQL));
  if (connection) {
    connection->identity = fake_atomic_add(&ku_fake_connections_opened, 1);
    connection->server_status = 2U;
    fake_atomic_add(&ku_fake_connections_live, 1);
  }
  return connection;
}
int mysql_thread_init(void) {
  fake_atomic_add(&ku_fake_thread_init_calls, 1);
  fake_atomic_add(&ku_fake_thread_active, 1);
  return 0;
}
void mysql_thread_end(void) {
  fake_atomic_add(&ku_fake_thread_end_calls, 1);
  fake_atomic_add(&ku_fake_thread_active, -1);
}
int mysql_options(MYSQL* c, enum mysql_option o, const void* v) {
  (void)c;
  if (o == MYSQL_OPT_RECONNECT) {
    fake_atomic_add(&ku_fake_reconnect_option_calls, 1);
    return 1;
  }
  if (o == MYSQL_OPT_LOCAL_INFILE) {
    if (!v || *(const unsigned int*)v != 0) return 1;
    fake_atomic_add(&ku_fake_local_infile_disabled, 1);
  }
  return 0;
}
MYSQL* mysql_real_connect(MYSQL* c, const char* h, const char* u, const char* p,
                          const char* d, unsigned int port, const char* s,
                          unsigned long flags) {
  (void)h; (void)u; (void)p; (void)d; (void)port; (void)s; (void)flags;
  fake_atomic_add(&ku_fake_real_connect_calls, 1);
  return fake_atomic_exchange(&ku_fake_fail_real_connect, 0) ? NULL : c;
}
int mysql_set_character_set(MYSQL* c, const char* n) { (void)c; return strcmp(n, "utf8mb4"); }
int mysql_reset_connection(MYSQL* c) {
  fake_atomic_add(&ku_fake_reset_connection_calls, 1);
  if (fake_atomic_exchange(&ku_fake_fail_reset_connection, 0)) {
    if (c) c->broken = 1;
    return 1;
  }
  if (!c || c->broken) return 1;
  c->server_status = 2U;
  return 0;
}
void mysql_close(MYSQL* c) {
  if (c) {
    fake_atomic_add(&ku_fake_connections_live, -1);
    free(c);
  }
}

MYSQL_STMT* mysql_stmt_init(MYSQL* c) {
  fake_atomic_add(&ku_fake_stmt_init_calls, 1);
  if (!c || c->broken) return NULL;
  MYSQL_STMT* s = (MYSQL_STMT*)calloc(1, sizeof(MYSQL_STMT));
  if (s) {
    s->connection = c;
    fake_atomic_add(&ku_fake_statements_live, 1);
  }
  return s;
}
int mysql_stmt_prepare(MYSQL_STMT* s, const char* sql, unsigned long len) {
  fake_atomic_add(&ku_fake_stmt_prepare_calls, 1);
  s->sql = (char*)malloc((size_t)len + 1);
  if (!s->sql) { s->error_code = 1; return 1; }
  memcpy(s->sql, sql, len); s->sql[len] = 0;
  for (unsigned long i = 0; i < len; i++) if (sql[i] == '?') s->param_count++;
  if (strstr(s->sql, "DROP TABLE") != NULL) { s->error_code = 999; return 1; }
  return 0;
}
unsigned int mysql_stmt_errno(MYSQL_STMT* s) { return s ? s->error_code : 1; }
unsigned long mysql_stmt_param_count(MYSQL_STMT* s) { return s->param_count; }
int mysql_stmt_bind_param(MYSQL_STMT* s, MYSQL_BIND* p) { s->params = p; return 0; }
int mysql_stmt_execute(MYSQL_STMT* s) {
  fake_atomic_add(&ku_fake_execute_calls, 1);
  if (strncmp(s->sql, "LATE", 4) == 0) {
    for (int index = 0; index < 10; index++) fake_pause();
  }
  if (fake_atomic_load(&ku_fake_block_execute) && strncmp(s->sql, "BLOCK", 5) == 0) {
    fake_atomic_store(&ku_fake_execute_entered, 1);
    unsigned long long deadline = __ku_handler_now_ms() + 5000;
    while (!fake_atomic_load(&ku_fake_release_execute)
        && __ku_handler_now_ms() < deadline) fake_pause();
    if (!fake_atomic_load(&ku_fake_release_execute)) {
      s->connection->broken = 1;
      s->error_code = 2013;
      return 1;
    }
  }
  if (strncmp(s->sql, "BROKEN", 6) == 0) {
    s->connection->broken = 1; s->error_code = 2013; return 1;
  }
  if (strncmp(s->sql, "CLIENT_ERROR", 12) == 0) {
    s->error_code = 2008; return 1;
  }
  if (strncmp(s->sql, "SERVER_ERROR", 12) == 0) {
    s->error_code = 1064; return 1;
  }
  if (strncmp(s->sql, "POST_STATE_QUERY", 16) == 0) {
    s->field_count = 3;
    /* SERVER_STATUS_IN_TRANS set: query payload must be discarded. */
    s->connection->server_status = 1U;
    return 0;
  }
  if (strncmp(s->sql, "POST_STATE_EXECUTE", 18) == 0) {
    s->field_count = 0;
    /* No IN_TRANS bit, but SERVER_STATUS_AUTOCOMMIT is also absent. */
    s->connection->server_status = 0U;
    return 0;
  }
  if (strncmp(s->sql, "BEGIN", 5) == 0) {
    s->connection->server_status = 1U;
    return 0;
  }
  if (strncmp(s->sql, "SELECT", 6) == 0) {
    static const char payload[] = "x'; DROP TABLE users; --";
    static const char utf8[] = "\xe4\xbd\xa0\xe5\xa5\xbd";
    if (s->param_count != 3
        || *s->params[0].length != sizeof(payload) - 1
        || memcmp(s->params[0].buffer, payload, sizeof(payload) - 1) != 0
        || *s->params[1].length != 0
        || *s->params[2].length != sizeof(utf8) - 1
        || memcmp(s->params[2].buffer, utf8, sizeof(utf8) - 1) != 0) {
      s->error_code = 998; return 1;
    }
    s->field_count = 3;
  }
  return 0;
}
unsigned int mysql_stmt_field_count(MYSQL_STMT* s) { return s->field_count; }
MYSQL_RES* mysql_stmt_result_metadata(MYSQL_STMT* s) {
  if (!s->field_count) return NULL;
  MYSQL_RES* r = (MYSQL_RES*)calloc(1, sizeof(MYSQL_RES));
  if (!r) return NULL;
  for (int i = 0; i < 3; i++) {
    r->fields[i].type = MYSQL_TYPE_VAR_STRING;
    r->fields[i].charsetnr = 45;
  }
  if (strstr(s->sql, "BINARY") != NULL) {
    r->fields[0].type = MYSQL_TYPE_BLOB;
    r->fields[0].charsetnr = 63;
  }
  return r;
}
MYSQL_FIELD* mysql_fetch_fields(MYSQL_RES* r) { return r->fields; }
void mysql_free_result(MYSQL_RES* r) { free(r); }
int mysql_stmt_bind_result(MYSQL_STMT* s, MYSQL_BIND* b) { s->outputs = b; return 0; }

static const unsigned char* cell(size_t row, unsigned int col, size_t* len, my_bool* is_null) {
  static const unsigned char payload[] = "x'; DROP TABLE users; --";
  static const unsigned char utf8[] = "\xe4\xbd\xa0\xe5\xa5\xbd";
  static const unsigned char ok[] = "ok";
  static unsigned char long_text[300];
  static int initialized;
  if (!initialized) { memset(long_text, 'a', sizeof(long_text)); initialized = 1; }
  *is_null = 0;
  if (row == 0 && col == 0) { *len = sizeof(payload) - 1; return payload; }
  if (row == 0 && col == 1) { *len = 0; return (const unsigned char*)""; }
  if (row == 0 && col == 2) { *len = sizeof(utf8) - 1; return utf8; }
  if (row == 1 && col == 0) { *len = 0; *is_null = 1; return NULL; }
  if (row == 1 && col == 1) { *len = sizeof(long_text); return long_text; }
  *len = sizeof(ok) - 1; return ok;
}

int mysql_stmt_fetch(MYSQL_STMT* s) {
  if (s->row >= 2) return MYSQL_NO_DATA;
  int truncated = 0;
  for (unsigned int col = 0; col < 3; col++) {
    size_t len = 0; my_bool is_null = 0;
    const unsigned char* value = cell(s->row, col, &len, &is_null);
    *s->outputs[col].length = (unsigned long)len;
    *s->outputs[col].is_null = is_null;
    *s->outputs[col].error = 0;
    if (!is_null) {
      size_t copy = len < s->outputs[col].buffer_length ? len : s->outputs[col].buffer_length;
      if (copy) memcpy(s->outputs[col].buffer, value, copy);
      if (copy < len) { *s->outputs[col].error = 1; truncated = 1; }
    }
  }
  s->row++;
  return truncated ? MYSQL_DATA_TRUNCATED : 0;
}
int mysql_stmt_fetch_column(MYSQL_STMT* s, MYSQL_BIND* b, unsigned int col, unsigned long off) {
  size_t len = 0; my_bool is_null = 0;
  const unsigned char* value = cell(s->row - 1, col, &len, &is_null);
  if (is_null || off > len || b->buffer_length < len - off) return 1;
  if (len > off) memcpy(b->buffer, value + off, len - off);
  *b->length = (unsigned long)len; *b->is_null = 0; *b->error = 0;
  return 0;
}
my_bool mysql_stmt_free_result(MYSQL_STMT* s) {
  if (fake_atomic_exchange(&ku_fake_fail_stmt_free_result, 0)) {
    if (s && s->connection) s->connection->broken = 1;
    return 1;
  }
  return 0;
}
my_bool mysql_stmt_close(MYSQL_STMT* s) {
  my_bool failed = 0;
  if (s) {
    if (fake_atomic_exchange(&ku_fake_fail_stmt_close, 0)) {
      if (s->connection) s->connection->broken = 1;
      failed = 1;
    }
    free(s->sql);
    fake_atomic_add(&ku_fake_statements_live, -1);
    free(s);
  }
  return failed;
}
my_ulonglong mysql_stmt_affected_rows(MYSQL_STMT* s) {
  return strncmp(s->sql, "UPDATE", 6) == 0 ? 3 : 0;
}
"#
}
