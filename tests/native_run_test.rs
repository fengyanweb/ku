//! End-to-end native tests: compile generated C with the real toolchain
//! (zig/clang/gcc, or MSVC cl.exe via vcvars) and run the produced binary,
//! asserting stdout/exit. When no C compiler is present the tests skip cleanly
//! instead of failing, so they stay green on machines without a toolchain.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bounded_process::{run_bounded, FailureKind, OutputLimits};

const BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const RUN_TIMEOUT: Duration = Duration::from_secs(20);
const BUILD_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(8 * 1024 * 1024, 12 * 1024 * 1024);
const RUN_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(4 * 1024 * 1024, 6 * 1024 * 1024);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = env::var("KU_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    let exe = if cfg!(windows) { "ku.exe" } else { "ku" };
    let target_dir = env::var("CARGO_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    [
        target_dir.join("debug").join(exe),
        target_dir.join("release").join(exe),
        repo_root().join("target").join("debug").join(exe),
        repo_root().join("target").join("release").join(exe),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("ku binary not found; set KU_BIN or build the ku binary first")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "ku-native-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn configure_mysql_runtime_search(command: &mut Command) {
    let mut directories = Vec::new();
    if let Some(configured) = env::var_os("KU_MYSQL_LIB") {
        let library_dir = PathBuf::from(configured);
        directories.push(library_dir.clone());
        if let Some(install_root) = library_dir.parent() {
            directories.push(install_root.join("bin"));
        }
    }

    #[cfg(windows)]
    if directories.is_empty() {
        for base in [r"C:\Program Files\MySQL", r"D:\Program Files\MySQL"] {
            let Ok(entries) = fs::read_dir(base) else {
                continue;
            };
            let mut installs = entries
                .take(256)
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>();
            installs.sort();
            installs.reverse();
            for install in installs {
                directories.push(install.join("lib"));
                directories.push(install.join("bin"));
            }
        }
    }

    directories.retain(|path| path.is_dir());
    let variable = if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    if let Some(existing) = env::var_os(variable) {
        directories.extend(env::split_paths(&existing));
    }
    if !directories.is_empty() {
        let joined = env::join_paths(directories)
            .expect("MySQL runtime search directories must form a valid loader path");
        command.env(variable, joined);
    }
}

/// Build `entry_rel` (relative to `dir`) into a native binary at `dir/out`.
/// Returns the binary path, or `None` when no C compiler is available (skip).
fn native_build(dir: &Path, entry_rel: &str, out_stem: &str) -> Option<PathBuf> {
    native_build_impl(dir, entry_rel, out_stem, false)
}

/// Build an artifact whose generated C includes the deterministic object-ABI
/// allocation fault hook. Ordinary `native_build` artifacts compile the direct
/// allocator path and cannot be affected by the runtime fault variables.
fn native_build_with_object_oom_hook(
    dir: &Path,
    entry_rel: &str,
    out_stem: &str,
) -> Option<PathBuf> {
    native_build_impl(dir, entry_rel, out_stem, true)
}

fn native_build_impl(
    dir: &Path,
    entry_rel: &str,
    out_stem: &str,
    object_oom_hook: bool,
) -> Option<PathBuf> {
    let out = exe_name(out_stem);
    let mut command = Command::new(ku_binary());
    command
        .current_dir(dir)
        .args(["build", "--native", entry_rel, "-o", &out]);
    if object_oom_hook {
        command.env("KU_NATIVE_TEST_OBJECT_OOM_ENABLE", "1");
    }
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("ku build --native did not complete safely:\n{error}"));
    if output.status.success() {
        return Some(dir.join(&out));
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if combined.contains("C compiler not found") {
        eprintln!("skip: no C compiler available for native e2e test");
        return None;
    }
    panic!("ku build --native failed unexpectedly:\n{combined}");
}

fn native_generated_c(dir: &Path, out_stem: &str) -> PathBuf {
    let root = dir.join(".ku").join("build").join("debug").join("c");
    let expected_name = format!("{out_stem}.c");
    let mut matches = Vec::new();
    for entry in fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("read generated C root {}: {error}", root.display()))
        .take(257)
    {
        let entry = entry.expect("read generated C entry");
        let file_type = entry.file_type().expect("read generated C entry type");
        if file_type.is_file() && entry.file_name() == expected_name.as_str() {
            matches.push(entry.path());
        } else if file_type.is_dir() {
            let candidate = entry.path().join(&expected_name);
            if candidate.is_file() {
                matches.push(candidate);
            }
        }
    }
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one generated {expected_name} under {}",
        root.display()
    );
    matches.pop().expect("one generated C path")
}

/// Emit C without linking, so helper-gating regressions remain a hard test even
/// on hosts where no native compiler is installed.
fn native_emit_c(dir: &Path, entry_rel: &str) -> String {
    let mut command = Command::new(ku_binary());
    command
        .current_dir(dir)
        .args(["build", "--native", entry_rel]);
    let output =
        run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!("ku build --native C emission did not complete safely:\n{error}")
        });
    if !output.status.success() {
        panic!(
            "ku build --native C emission failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let c_path = dir.join(entry_rel).with_extension("c");
    fs::read_to_string(&c_path)
        .unwrap_or_else(|err| panic!("read generated C {}: {err}", c_path.display()))
}

/// Compile a self-contained C harness with the same families of native
/// toolchains accepted by `ku build --native`. The harnesses remove external
/// library pragmas and provide local stubs, so they exercise generated runtime
/// guards without requiring a live service or its development library.
fn compile_c_harness(dir: &Path, source: &Path, out_stem: &str) -> Option<PathBuf> {
    let output = dir.join(exe_name(out_stem));
    let mut candidates: Vec<(PathBuf, Vec<String>)> = Vec::new();
    if let Ok(spec) = env::var("KU_CC") {
        let as_path = PathBuf::from(&spec);
        if as_path.exists() {
            candidates.push((as_path, Vec::new()));
        } else {
            let mut words = spec.split_whitespace();
            if let Some(program) = words.next() {
                candidates.push((PathBuf::from(program), words.map(str::to_owned).collect()));
            }
        }
    }
    candidates.extend([
        (PathBuf::from("clang"), Vec::new()),
        (PathBuf::from("gcc"), Vec::new()),
        (PathBuf::from("cc"), Vec::new()),
        (PathBuf::from("zig"), vec!["cc".to_string()]),
    ]);

    for (program, prefix) in candidates {
        let mut command = Command::new(&program);
        command.args(prefix).arg(source).arg("-std=c11");
        if !cfg!(windows) {
            command.arg("-pthread");
        }
        command.arg("-o").arg(&output);
        match run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS) {
            Ok(done) if done.status.success() => return Some(output),
            Ok(done) => panic!(
                "C harness compiler '{}' failed:\n{}{}",
                program.display(),
                String::from_utf8_lossy(&done.stdout),
                String::from_utf8_lossy(&done.stderr)
            ),
            Err(error)
                if error.kind() == FailureKind::Spawn
                    && error.io_error_kind() == Some(std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => panic!("C harness compiler did not complete safely: {error}"),
        }
    }

    #[cfg(windows)]
    {
        let program_files = env::var("ProgramFiles(x86)")
            .or_else(|_| env::var("ProgramFiles"))
            .ok()?;
        let vswhere = Path::new(&program_files)
            .join("Microsoft Visual Studio")
            .join("Installer")
            .join("vswhere.exe");
        if !vswhere.exists() {
            eprintln!("skip: no C compiler available for generated C harness");
            return None;
        }
        let mut command = Command::new(vswhere);
        command.args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ]);
        let found = run_bounded(&mut command, RUN_TIMEOUT, BUILD_OUTPUT_LIMITS)
            .unwrap_or_else(|error| panic!("Visual Studio discovery was not bounded: {error}"));
        if !found.status.success() {
            eprintln!("skip: Visual Studio C++ toolchain was not found");
            return None;
        }
        let install = String::from_utf8_lossy(&found.stdout).trim().to_owned();
        let vcvars = Path::new(&install)
            .join("VC")
            .join("Auxiliary")
            .join("Build")
            .join("vcvars64.bat");
        if install.is_empty() || !vcvars.exists() {
            eprintln!("skip: Visual Studio C++ environment script was not found");
            return None;
        }
        let script = dir.join("compile-harness.bat");
        let object = dir.join(format!("{out_stem}.obj"));
        fs::write(
            &script,
            format!(
                "@echo off\r\ncall \"{}\" >nul\r\nif errorlevel 1 exit /b %errorlevel%\r\ncl.exe /nologo /std:c11 /utf-8 \"{}\" /Fe:\"{}\" /Fo:\"{}\"\r\n",
                vcvars.display(),
                source.display(),
                output.display(),
                object.display()
            ),
        )
        .expect("write MSVC harness script");
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/C"]).arg(&script);
        let done = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
            .unwrap_or_else(|error| panic!("MSVC harness compiler was not bounded: {error}"));
        fs::remove_file(&script).ok();
        fs::remove_file(&object).ok();
        assert!(
            done.status.success(),
            "MSVC C harness compile failed:\n{}{}",
            String::from_utf8_lossy(&done.stdout),
            String::from_utf8_lossy(&done.stderr)
        );
        Some(output)
    }

    #[cfg(not(windows))]
    {
        eprintln!("skip: no C compiler available for generated C harness");
        None
    }
}

fn run_binary(exe: &Path) -> (String, Option<i32>) {
    let (stdout, code) = run_binary_bytes(exe);
    (String::from_utf8_lossy(&stdout).into_owned(), code)
}

fn run_binary_bytes(exe: &Path) -> (Vec<u8>, Option<i32>) {
    let mut command = Command::new(exe);
    command.current_dir(exe.parent().unwrap_or_else(|| Path::new(".")));
    let output =
        run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!(
                "native binary {} did not complete safely:\n{error}",
                exe.display()
            )
        });
    (output.stdout, output.status.code())
}

fn run_binary_with_object_oom(
    exe: &Path,
    site: &str,
    ordinal: usize,
) -> (String, String, Option<i32>) {
    let mut command = Command::new(exe);
    command
        .current_dir(exe.parent().unwrap_or_else(|| Path::new(".")))
        .env("KU_NATIVE_TEST_OBJECT_OOM_SITE", site)
        .env("KU_NATIVE_TEST_OBJECT_OOM_ORDINAL", ordinal.to_string());
    let output =
        run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!(
                "native binary {} did not complete safely:\n{error}",
                exe.display()
            )
        });
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

#[test]
fn native_dynamic_http_server_config_rejects_unknown_fields() {
    let dir = unique_temp_dir("http-dynamic-config");
    fs::write(
        dir.join("main.ku"),
        r#"
import http from "std.http"

fn main(): null! {
    app = http.server({ max_active_requests: 1 })
    return ok(null)
}
"#,
    )
    .expect("write native dynamic http config source");

    let generated = native_emit_c(&dir, "main.ku");
    assert!(generated.contains("static void ku_http_validate_server_config_keys"));
    assert!(generated.contains("ku_http_validate_server_config_keys(config)"));
    let harness = dir.join("http-dynamic-config-harness.c");
    fs::write(
        &harness,
        format!(
            r#"#define main ku_fixture_main
{generated}
#undef main
int main(void) {{
  KuObject* config = ku_object_new(4);
  ku_object_set(config,
      ku_string_static((const uint8_t*)"maxActiveRequests", sizeof("maxActiveRequests") - 1),
      ku_v_int(1));
  (void)ku_http_server_new_cfg(config);
  return 90;
}}
"#
        ),
    )
    .expect("write native dynamic HTTP config harness");
    let Some(exe) = compile_c_harness(&dir, &harness, "http-dynamic-config") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let mut command = Command::new(&exe);
    command.current_dir(&dir);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native dynamic HTTP config test was not bounded: {error}"));
    assert!(!output.status.success(), "unknown HTTP config must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("unknown http config field 'maxActiveRequests'"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_pg_runtime_emits_strict_cells_limits_and_pool_cleanup() {
    let dir = unique_temp_dir("pg-runtime-guards");
    fs::write(
        dir.join("main.ku"),
        r#"import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost", max_connections: 1 })?
    res = client.query("SELECT $1", ["x"])?
    println(res.value(0, 0)?)
    println(res.is_null(0, 0)?)
    pooled = client.query("SELECT 1", [])?
    println(pooled.rows())
    client.close()
    return ok(null)
}
"#,
    )
    .expect("write pg runtime guard fixture");

    let c = native_emit_c(&dir, "main.ku");
    for expected in [
        "extern int PQgetisnull",
        "extern int PQgetlength",
        "extern int PQfformat",
        "extern PGconn* PQconnectStartParams",
        "extern int PQconnectPoll",
        "extern int PQsocket",
        "extern int PQsetnonblocking",
        "extern int PQsendQueryParams",
        "extern int PQsetSingleRowMode",
        "extern PGresult* PQgetResult",
        "extern const char* PQparameterStatus",
        "static KuResult_pg_client ku_pg_client(KuObject* config)",
        "static KuResult_str ku_pg_value",
        "static KuResult_bool ku_pg_is_null",
        "static int ku_pg_utf8_valid",
        "static KuString ku_pg_connection_failure_message",
        "static KuError ku_pg_connect_failure_error",
        "static KuError ku_pg_out_of_memory_error",
        "static void ku_pg_wipe_secret",
        "size_t conninfo_len",
        "ku_pg_wipe_secret(p->conninfo, p->conninfo_len)",
        "static int ku_pg_connection_is_utf8",
        "static int ku_pg_ensure_utf8",
        "static int ku_pg_result_append",
        "typedef struct KuPgResult",
        "#define KU_PG_MAX_RESULT_ROWS 1000000ULL",
        "#define KU_PG_MAX_RESULT_COLS 4096ULL",
        "#define KU_PG_MAX_RESULT_CELLS 1000000ULL",
        "#define KU_PG_MAX_RESULT_BYTES (64ULL * 1024ULL * 1024ULL)",
        "#define KU_PG_MAX_PARAM_COUNT 65535ULL",
        "#define KU_PG_MAX_PARAM_BYTES (64ULL * 1024ULL * 1024ULL)",
        "static int ku_pg_validate_query_params",
        "static int ku_pg_prepare_query_params",
        "static KuResult_pg_result ku_pg_query_params_validated_impl",
        "static KuResult_pg_result ku_pg_query_params_all_validated_impl",
        "static int ku_pg_validate_sql_input",
        "static int ku_pg_sql_has_explicit_session_control",
        "parameter_too_large",
        "PostgreSQL query parameters exceed 64 MiB total UTF-8 bytes",
        "PostgreSQL query parameter is not valid UTF-8",
        "failed to allocate PostgreSQL parameter buffer",
        "prepared->storage = (char*)malloc(total_bytes + params.len)",
        "cursor[value.len] = '\\0'; cursor += value.len + 1",
        "static KuError ku_pg_execution_unknown_error",
        "static KuError ku_pg_execution_completed_without_result_error",
        "never retry automatically",
        "#define KU_PGRES_EMPTY_QUERY 0",
        "#define KU_PGRES_COPY_OUT 3",
        "#define KU_PGRES_COPY_IN 4",
        "#define KU_PGRES_BAD_RESPONSE 5",
        "#define KU_PGRES_COPY_BOTH 8",
        "#define KU_PQTRANS_ACTIVE 1",
        "#define KU_PQTRANS_UNKNOWN 4",
        "static KuResult_pg_result ku_pg_query_impl",
        "static KuResult_pg_result ku_pg_query_params_impl",
        "return ku_pg_finish_query(conn, result, deadline, broken)",
        "empty SQL query is not allowed",
        "PostgreSQL query connection failed; close and reconnect",
        "PostgreSQL client configuration is outside its allowed range",
        "ku_pg_cell_in_bounds",
        "n > KU_PG_MAX_PARAM_COUNT || n > SIZE_MAX / sizeof(char*)",
        "value.len > KU_PG_MAX_PARAM_BYTES - bytes",
        "n > SIZE_MAX - bytes",
        "query parameter contains a NUL byte",
        "size < 1 || size > 256",
        "#define KU_PG_CLIENT_DEFAULT_MAX_CONNECTIONS 8LL",
        "#define KU_PG_CLIENT_DEFAULT_MAX_WAITERS 64LL",
        "#define KU_PG_CLIENT_DEFAULT_CONNECT_TIMEOUT_MS 5000LL",
        "KuPgConnectAttempt initial_attempt = ku_pg_connect_until(p->conninfo, initial_deadline)",
        "#include <pthread.h>",
        "typedef CRITICAL_SECTION KuPgMutex",
        "typedef pthread_mutex_t KuPgMutex",
        "pthread_condattr_setclock(&attr, CLOCK_MONOTONIC)",
        "pthread_cond_timedwait_relative_np",
        "clock_gettime(CLOCK_MONOTONIC, &deadline)",
        "static int ku_pg_cond_wait_ms",
        "__ku_handler_deadline < deadline",
        "KuPgConnectAttempt connect_attempt = ku_pg_connect_until(p->conninfo, connect_deadline)",
        "timed out connecting a PostgreSQL client connection",
        "int connect_expired = ku_pg_now_ms() >= deadline",
        "if (p->closing || connect_expired)",
        "p->closing && p->active == 0 && p->waiters == 0",
        "PostgreSQL client waiter limit reached",
        "p->waiters >= p->max_waiters",
        "timed out waiting for a PostgreSQL client connection",
        "extern int PQtransactionStatus",
        "ku_pg_client_cleanup_connection",
        "if (tx != KU_PQTRANS_IDLE) return 1",
        "p->closing = 1",
        "p->active == 0 && p->waiters == 0",
        "static void ku_pg_client_dispose",
        "int broken = 0; KuResult_pg_result r = ku_pg_query_params_all_validated_impl(c, sql, params, param_bytes, deadline, &broken)",
        "ku_pg_client_release(p, slot, broken || PQstatus(c) != KU_PG_CONNECTION_OK, deadline)",
    ] {
        assert!(c.contains(expected), "generated PG runtime missed: {expected}");
    }
    assert_eq!(
        c.matches("return ku_pg_finish_query(conn, result, deadline, broken)")
            .count(),
        1,
        "both query paths must share one encoding and result validation path"
    );
    assert!(
        c.contains("tx == KU_PQTRANS_ACTIVE || tx == KU_PQTRANS_UNKNOWN"),
        "the shared query path must reject a connection left in COPY or unknown state"
    );
    assert_eq!(
        c.matches(
            "if (!ku_pg_validate_query_params(params, &param_bytes, &param_error, deadline))"
        )
        .count(),
        2,
        "raw and pool parameter APIs must use the same validator"
    );
    assert!(
        !c.contains("values[i] = ku_string_to_cstr(params.data[i])"),
        "parameter conversion must use one bounded contiguous buffer instead of one allocation per value"
    );
    let raw_params = c
        .split_once("static KuResult_pg_result ku_pg_query_params_impl(")
        .expect("raw parameter wrapper")
        .1
        .split_once("static KuResult_pg_result ku_pg_query_params(")
        .expect("raw parameter wrapper end")
        .0;
    assert!(
        raw_params
            .find("ku_pg_validate_query_params")
            .expect("raw validation")
            < raw_params
                .find("ku_pg_query_params_validated_impl")
                .expect("raw execution"),
        "raw query parameters must be budgeted before connection/query work"
    );
    let client_params = c
        .split_once("static KuResult_pg_result ku_pg_client_query(")
        .expect("client parameter wrapper")
        .1
        .split_once("static void ku_pg_client_dispose(")
        .expect("client parameter wrapper end")
        .0;
    let client_closed = client_params.find("if (!p)").expect("closed client guard");
    let client_sql_validation = client_params
        .find("ku_pg_validate_sql_input")
        .expect("client SQL validation");
    let client_sql_policy = client_params
        .find("ku_pg_sql_has_explicit_session_control")
        .expect("client SQL policy");
    let client_validation = client_params
        .find("ku_pg_validate_query_params")
        .expect("client validation");
    let client_acquire = client_params
        .find("ku_pg_client_acquire")
        .expect("client acquire");
    assert!(
        client_closed < client_sql_validation
            && client_sql_validation < client_sql_policy
            && client_sql_policy < client_validation
            && client_validation < client_acquire,
        "client must validate bounded SQL before policy scanning, then reject invalid parameters before borrowing a connection"
    );
    assert!(
        !c.contains("PQexec(c, \"ROLLBACK\")"),
        "client cleanup must discard non-idle sessions instead of issuing an unbounded ROLLBACK"
    );
    assert!(
        !c.contains("clock_gettime(CLOCK_REALTIME, &deadline)"),
        "client waits must use the same monotonic clock as their total deadline"
    );
    assert!(
        !c.contains("PQerrorMessage"),
        "initial connection failures must never read potentially secret-bearing libpq messages"
    );
    assert!(
        !c.contains("KuResult_pg_conn") && !c.contains("__ku_pg_conn"),
        "the public native ABI must not retain the removed raw connection handle"
    );
    assert!(
        !c.contains("PQconnectdb(") && !c.contains("PQconnectdbParams("),
        "native PG connections must not retain the synchronous libpq entrypoints"
    );
    let poll_helper = c
        .split_once("static KuPgConnectAttempt ku_pg_connect_until(")
        .expect("PG poll helper")
        .1
        .split_once("static KuPgResult* ku_move_pg_result(")
        .expect("PG poll helper end")
        .0;
    assert!(
        poll_helper.contains("!ku_pg_connection_is_utf8(h)")
            && poll_helper.contains("PQfinish(h)")
            && !poll_helper.contains("ku_pg_ensure_utf8(h)"),
        "connect poller must validate the startup UTF8 parameter and close failures without a post-connect setter"
    );
    let lazy_connect = c
        .split_once(
            "KuPgConnectAttempt connect_attempt = ku_pg_connect_until(p->conninfo, connect_deadline)",
        )
        .expect("lazy client connection block")
        .1
        .split_once("*out = h; return made;")
        .expect("lazy client connection install boundary")
        .0;
    let final_deadline_check = lazy_connect
        .find("int connect_expired = ku_pg_now_ms() >= deadline")
        .expect("post-validation deadline check");
    let install = lazy_connect
        .find("p->conns[made] = h")
        .expect("lazy connection install");
    assert!(
        final_deadline_check < install,
        "lazy client connection must re-check its absolute deadline before installation"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_pg_connection_failures_redact_libpq_secrets() {
    let dir = unique_temp_dir("pg-connection-secret-redaction");
    fs::write(
        dir.join("main.ku"),
        r#"import pg from "std.pg"
fn main(): null! {
    try {
        pg.client({ conninfo: "host=db password=fixture-secret" })?
    } catch(err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    try {
        pg.client({ conninfo: "host=db password=fixture-secret", connect_timeout_ms: 5 })?
    } catch(err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    try {
        pg.client({ conninfo: "host=db password=fixture-secret", max_connections: 2 })?
    } catch(err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    return ok(null)
}
"#,
    )
    .expect("write PG connection redaction fixture");

    let generated = native_emit_c(&dir, "main.ku");
    assert!(generated.contains("PostgreSQL connection failed"));
    assert!(generated.contains("ku_pg_connect_failure_error()"));
    assert!(!generated.contains("ku_pg_pool_connect_failure_error()"));
    assert!(
        !generated.contains("PQerrorMessage"),
        "generated runtime must not even reference the connection error accessor"
    );
    for function_start in [
        "static KuPgConnectAttempt ku_pg_connect_until(const char* conninfo, unsigned long long deadline)",
        "static KuResult_pg_client ku_pg_client_open(KuString conninfo, int64_t size, int64_t max_waiters, int64_t connect_timeout_ms, int64_t acquire_timeout_ms, int64_t query_timeout_ms)",
        "static int ku_pg_client_acquire(KuPgClient* p, PGconn** out, KuError* err, unsigned long long operation_deadline)",
    ] {
        let body = generated
            .split_once(function_start)
            .unwrap_or_else(|| panic!("missing generated PG function: {function_start}"))
            .1
            .split_once("\n}")
            .expect("generated PG function terminator")
            .0;
        assert!(
            !body.contains("ku_pg_copy_libpq_error"),
            "connection path must not copy raw libpq text: {function_start}"
        );
    }
    assert!(!generated.contains("KuResult_pg_conn"));
    assert!(!generated.contains("KuResult_pg_pool"));

    let pg_poll_hook = r#"
#if defined(_WIN32)
static int ku_test_pg_os_poll(WSAPOLLFD* item, ULONG count, INT timeout_ms);
#define WSAPoll ku_test_pg_os_poll
#else
static int ku_test_pg_os_poll(struct pollfd* item, nfds_t count, int timeout_ms);
#define poll ku_test_pg_os_poll
#endif
"#;
    let mut harness = generated
        .replacen(
            "typedef struct pg_conn PGconn;",
            &format!("{pg_poll_hook}\ntypedef struct pg_conn PGconn;"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    harness.push_str(
        r#"
struct pg_conn { int status; int id; };
struct pg_result { int unused; };
static struct pg_conn ku_test_raw_failed = { 1, 1 };
static struct pg_conn ku_test_timeout_failed = { 1, 2 };
static struct pg_conn ku_test_pool_good = { 0, 3 };
static struct pg_conn ku_test_pool_lazy_failed = { 1, 4 };
static struct pg_result ku_test_result;
static int ku_test_param_mode = 0;
static int ku_test_error_message_calls = 0;
static int ku_test_finish_counts[5] = {0};
static int ku_test_exec_calls = 0;
static int ku_test_pending_result = 0;

PGconn* PQconnectStartParams(const char* const* keys, const char* const* values, int expand) {
  (void)keys; (void)values; (void)expand;
  if (ku_test_param_mode == 1) return &ku_test_pool_good;
  if (ku_test_param_mode == 2) return &ku_test_pool_lazy_failed;
  if (ku_test_finish_counts[1] == 0) return &ku_test_raw_failed;
  return &ku_test_timeout_failed;
}
int PQconnectPoll(PGconn* value) { (void)value; return KU_PGRES_POLLING_OK; }
int PQsocket(const PGconn* value) { return value ? 7 : -1; }
int PQstatus(const PGconn* value) { return value ? value->status : 1; }
char* PQerrorMessage(const PGconn* value) {
  (void)value;
  ku_test_error_message_calls++;
  return "could not connect: password=KU_PG_SECRET_CANARY";
}
int PQsetClientEncoding(PGconn* value, const char* encoding) { (void)value; (void)encoding; return 0; }
const char* PQparameterStatus(const PGconn* value, const char* name) { (void)value; (void)name; return "UTF8"; }
void PQfinish(PGconn* value) { if (value && value->id >= 1 && value->id <= 4) ku_test_finish_counts[value->id]++; }
int PQsetnonblocking(PGconn* value, int mode) { (void)value; return mode == 1 ? 0 : -1; }
int PQflush(PGconn* value) { (void)value; return 0; }
int PQconsumeInput(PGconn* value) { (void)value; return 1; }
int PQisBusy(PGconn* value) { (void)value; return 0; }
int PQsetSingleRowMode(PGconn* value) { (void)value; return 1; }
PGresult* PQgetResult(PGconn* value) { (void)value; if (ku_test_pending_result) { ku_test_pending_result = 0; return &ku_test_result; } return 0; }
int PQsendQuery(PGconn* value, const char* sql) { (void)value; (void)sql; ku_test_exec_calls++; ku_test_pending_result = 1; return 1; }
int PQsendQueryParams(PGconn* value, const char* sql, int count, const void* types, const char* const* values, const int* lengths, const int* formats, int result_format) {
  (void)value; (void)sql; (void)count; (void)types; (void)values; (void)lengths; (void)formats; (void)result_format;
  ku_test_exec_calls++;
  ku_test_pending_result = 1; return 1;
}
int PQresultStatus(const PGresult* value) { (void)value; return KU_PGRES_COMMAND_OK; }
char* PQresultErrorMessage(const PGresult* value) { (void)value; return "query error"; }
int PQntuples(const PGresult* value) { (void)value; return 0; }
int PQnfields(const PGresult* value) { (void)value; return 0; }
int PQfformat(const PGresult* value, int col) { (void)value; (void)col; return 0; }
char* PQgetvalue(const PGresult* value, int row, int col) { (void)value; (void)row; (void)col; return ""; }
int PQgetisnull(const PGresult* value, int row, int col) { (void)value; (void)row; (void)col; return 1; }
int PQgetlength(const PGresult* value, int row, int col) { (void)value; (void)row; (void)col; return 0; }
int PQtransactionStatus(const PGconn* value) { (void)value; return KU_PQTRANS_IDLE; }
void PQclear(PGresult* value) { (void)value; }

#if defined(_WIN32)
static int ku_test_pg_os_poll(WSAPOLLFD* item, ULONG count, INT timeout_ms) {
  (void)timeout_ms;
  if (!item || count != 1) return -1;
  item->revents = item->events; return 1;
}
#else
static int ku_test_pg_os_poll(struct pollfd* item, nfds_t count, int timeout_ms) {
  (void)timeout_ms;
  if (!item || count != 1) return -1;
  item->revents = item->events; return 1;
}
#endif

static int ku_test_string_is(KuString value, const char* expected) {
  size_t len = strlen(expected);
  return value.len == len && (len == 0 || (value.ptr && memcmp(value.ptr, expected, len) == 0));
}

static int ku_test_safe_connection_error(KuError error, const char* expected_code) {
  return ku_test_string_is(error.domain, "pg")
      && ku_test_string_is(error.code, expected_code)
      && ku_test_string_is(error.message, "PostgreSQL connection failed");
}

int main(void) {
  KuString conninfo = ku_string_static((const uint8_t*)"host=db password=fixture-secret", sizeof("host=db password=fixture-secret") - 1);
  if (ku_generated_main() != 0) return 10;
  if (ku_test_finish_counts[1] != 1 || ku_test_finish_counts[2] != 2) return 11;

  ku_test_param_mode = 1;
  KuResult_pg_client opened = ku_pg_client_open(conninfo, 2, 64, 5000, 5000, 30000);
  if (!opened.ok || !opened.value) return 12;
  KuPgClient* client = opened.value;
  opened.value = 0;

  PGconn* held = 0;
  KuError acquire_error = (KuError){0};
  int held_slot = ku_pg_client_acquire(client, &held, &acquire_error, ~0ULL);
  if (held_slot < 0 || held != &ku_test_pool_good) return 13;

  ku_test_param_mode = 2;
  KuString sql = ku_string_static((const uint8_t*)"SELECT 1", sizeof("SELECT 1") - 1);
  KuArray_str no_params = (KuArray_str){0};
  KuResult_pg_result lazy = ku_pg_client_query(client, sql, no_params);
  if (lazy.ok || lazy.value || !ku_test_safe_connection_error(lazy.error, "connect_error")) return 14;
  ku_error_drop(&lazy.error);
  if (ku_test_finish_counts[4] != 1 || ku_test_exec_calls != 0) return 15;

  ku_pg_client_release(client, held_slot, 0, ~0ULL);
  ku_drop_pg_client(&client);
  if (client || ku_test_finish_counts[3] != 1) return 16;
  if (ku_test_error_message_calls != 0) return 17;

  puts("pg connection secrets redacted");
  return 0;
}
"#,
    );
    let harness_path = dir.join("pg-connection-secret-redaction-harness.c");
    fs::write(&harness_path, harness).expect("write PG connection redaction C harness");
    let Some(exe) = compile_c_harness(
        &dir,
        &harness_path,
        "pg-connection-secret-redaction-harness",
    ) else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let mut command = Command::new(&exe);
    command.current_dir(&dir);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("PG connection redaction harness was not bounded: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stdout.replace('\r', ""),
        concat!(
            "pg\nconnect_error\nPostgreSQL connection failed\n",
            "pg\nconnect_error\nPostgreSQL connection failed\n",
            "pg\nconnect_error\nPostgreSQL connection failed\n",
            "pg connection secrets redacted\n",
        )
    );
    assert!(
        !stdout.contains("KU_PG_SECRET_CANARY") && !stderr.contains("KU_PG_SECRET_CANARY"),
        "native runtime output leaked a libpq connection secret: stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        output.status.success(),
        "PG connection redaction harness failed with {:?}: stderr={stderr}",
        output.status.code()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_pg_parameter_budget_generated_c_runs_without_a_database() {
    let dir = unique_temp_dir("pg-param-budget-c-harness");
    fs::write(
        dir.join("main.ku"),
        r#"import pg from "std.pg"
fn main(): null! {
    client = pg.client({ conninfo: "host=localhost", max_connections: 1 })?
    result = client.query("SELECT $1", ["x"])?
    println(result.rows())
    client.close()
    return ok(null)
}
"#,
    )
    .expect("write PG parameter budget fixture");

    let generated = native_emit_c(&dir, "main.ku");
    let mut harness = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    harness.push_str(
        r#"
struct pg_conn { int unused; };
struct pg_result { int unused; };
static struct pg_result ku_test_result;
static int ku_test_exec_params_calls = 0;
static int ku_test_connection_touches = 0;
static int ku_test_bad_values = 0;
static int ku_test_mode = 0;
static int ku_test_pending_result = 0;

PGconn* PQconnectStartParams(const char* const* keys, const char* const* values, int expand) { (void)keys; (void)values; (void)expand; return 0; }
int PQconnectPoll(PGconn* value) { (void)value; return KU_PGRES_POLLING_FAILED; }
int PQsocket(const PGconn* value) { (void)value; return -1; }
int PQstatus(const PGconn* value) { (void)value; ku_test_connection_touches++; return 0; }
char* PQerrorMessage(const PGconn* value) { (void)value; return "stub error"; }
int PQsetClientEncoding(PGconn* value, const char* encoding) { (void)value; (void)encoding; ku_test_connection_touches++; return 0; }
const char* PQparameterStatus(const PGconn* value, const char* name) { (void)value; (void)name; ku_test_connection_touches++; return "UTF8"; }
void PQfinish(PGconn* value) { (void)value; }
int PQsetnonblocking(PGconn* value, int mode) { (void)value; return mode == 1 ? 0 : -1; }
int PQflush(PGconn* value) { (void)value; return 0; }
int PQconsumeInput(PGconn* value) { (void)value; return 1; }
int PQisBusy(PGconn* value) { (void)value; return 0; }
int PQsetSingleRowMode(PGconn* value) { (void)value; return 1; }
PGresult* PQgetResult(PGconn* value) { (void)value; if (ku_test_pending_result) { ku_test_pending_result = 0; return &ku_test_result; } return 0; }
int PQsendQuery(PGconn* value, const char* sql) { (void)value; (void)sql; ku_test_pending_result = 1; return 1; }
int PQsendQueryParams(PGconn* value, const char* sql, int count, const void* types, const char* const* values, const int* lengths, const int* formats, int result_format) {
  (void)value; (void)sql; (void)types; (void)lengths; (void)formats; (void)result_format;
  ku_test_exec_params_calls++;
  if (ku_test_mode == 1 && (count != 0 || values != 0)) ku_test_bad_values = 1;
  if (ku_test_mode == 2 && (count != 2 || !values || strcmp(values[0], "ab") != 0 || strcmp(values[1], "cd") != 0 || values[1] != values[0] + 3)) ku_test_bad_values = 1;
  ku_test_pending_result = 1; return 1;
}
int PQresultStatus(const PGresult* value) { (void)value; return KU_PGRES_COMMAND_OK; }
char* PQresultErrorMessage(const PGresult* value) { (void)value; return "stub result error"; }
int PQntuples(const PGresult* value) { (void)value; return 0; }
int PQnfields(const PGresult* value) { (void)value; return 0; }
int PQfformat(const PGresult* value, int col) { (void)value; (void)col; return 0; }
char* PQgetvalue(const PGresult* value, int row, int col) { (void)value; (void)row; (void)col; return ""; }
int PQgetisnull(const PGresult* value, int row, int col) { (void)value; (void)row; (void)col; return 1; }
int PQgetlength(const PGresult* value, int row, int col) { (void)value; (void)row; (void)col; return 0; }
int PQtransactionStatus(const PGconn* value) { (void)value; ku_test_connection_touches++; return KU_PQTRANS_IDLE; }
void PQclear(PGresult* value) { (void)value; }

static int ku_test_code_is(KuError error, const char* expected) {
  size_t len = strlen(expected);
  return error.code.len == len && (len == 0 || memcmp(error.code.ptr, expected, len) == 0);
}

int main(void) {
  struct pg_conn connection = {0};
  KuString sql = ku_string_static((const uint8_t*)"SELECT $1", sizeof("SELECT $1") - 1);
  int broken = 99;

  KuString oversized_value = { (uint8_t*)"x", (size_t)KU_PG_MAX_PARAM_BYTES + 1, 0, KU_STRING_STATIC };
  KuArray_str oversized = { 1, &oversized_value };
  KuResult_pg_result result = ku_pg_query_params_impl(&connection, sql, oversized, ~0ULL, &broken);
  if (result.ok || !ku_test_code_is(result.error, "parameter_too_large") || broken != 0 || ku_test_connection_touches != 0 || ku_test_exec_params_calls != 0) return 10;
  ku_error_drop(&result.error);

  KuPgClient fake_client = {0};
  fake_client.query_timeout_ms = 30000;
  result = ku_pg_client_query(&fake_client, sql, oversized);
  if (result.ok || !ku_test_code_is(result.error, "parameter_too_large") || ku_test_connection_touches != 0 || ku_test_exec_params_calls != 0) return 11;
  ku_error_drop(&result.error);
  result = ku_pg_client_query(0, sql, oversized);
  if (result.ok || !ku_test_code_is(result.error, "client_closed")) return 12;
  ku_error_drop(&result.error);

  KuArray_str empty = {0, 0};
  ku_test_mode = 1;
  result = ku_pg_query_params_impl(&connection, sql, empty, ~0ULL, &broken);
  if (!result.ok || ku_test_exec_params_calls != 1 || ku_test_bad_values) return 13;
  ku_drop_pg_result(&result.value);

  uint8_t first_bytes[] = { 'a', 'b' };
  uint8_t second_bytes[] = { 'c', 'd' };
  KuString valid_values[] = {
    { first_bytes, sizeof(first_bytes), sizeof(first_bytes), KU_STRING_STATIC },
    { second_bytes, sizeof(second_bytes), sizeof(second_bytes), KU_STRING_STATIC }
  };
  KuArray_str valid = { 2, valid_values };
  ku_test_mode = 2;
  result = ku_pg_query_params_impl(&connection, sql, valid, ~0ULL, &broken);
  if (!result.ok || ku_test_exec_params_calls != 2 || ku_test_bad_values) return 14;
  ku_drop_pg_result(&result.value);

  ku_test_connection_touches = 0;
  uint8_t invalid_utf8_bytes[] = { 0xc3, 0x28 };
  KuString invalid_utf8_value = { invalid_utf8_bytes, sizeof(invalid_utf8_bytes), sizeof(invalid_utf8_bytes), KU_STRING_STATIC };
  KuArray_str invalid_utf8 = { 1, &invalid_utf8_value };
  result = ku_pg_query_params_impl(&connection, sql, invalid_utf8, ~0ULL, &broken);
  if (result.ok || !ku_test_code_is(result.error, "invalid_utf8") || broken != 0 || ku_test_connection_touches != 0 || ku_test_exec_params_calls != 2) return 15;
  ku_error_drop(&result.error);

  uint8_t nul_bytes[] = { 'a', 0, 'b' };
  KuString nul_value = { nul_bytes, sizeof(nul_bytes), sizeof(nul_bytes), KU_STRING_STATIC };
  KuArray_str nul = { 1, &nul_value };
  result = ku_pg_query_params_impl(&connection, sql, nul, ~0ULL, &broken);
  if (result.ok || !ku_test_code_is(result.error, "query_error") || broken != 0 || ku_test_connection_touches != 0 || ku_test_exec_params_calls != 2) return 16;
  ku_error_drop(&result.error);

  const size_t alias_count = (size_t)KU_PG_MAX_PARAM_COUNT;
  uint8_t* alias_bytes = (uint8_t*)malloc(2049);
  KuString* aliases = (KuString*)malloc(alias_count * sizeof(KuString));
  if (!alias_bytes || !aliases) return 17;
  memset(alias_bytes, 'a', 2049);
  aliases[0] = (KuString){ alias_bytes, 2048, 0, KU_STRING_STATIC };
  for (size_t i = 1; i < alias_count; i++) aliases[i] = (KuString){ alias_bytes, 1024, 0, KU_STRING_STATIC };
  KuArray_str exact = { alias_count, aliases };
  size_t total_bytes = 0; KuError validation_error = (KuError){0};
  if (!ku_pg_validate_query_params(exact, &total_bytes, &validation_error, ~0ULL) || total_bytes != (size_t)KU_PG_MAX_PARAM_BYTES) return 18;
  aliases[0].len = 2049;
  if (ku_pg_validate_query_params(exact, &total_bytes, &validation_error, ~0ULL) || !ku_test_code_is(validation_error, "parameter_too_large")) return 19;
  ku_error_drop(&validation_error);
  KuArray_str too_many = { (size_t)KU_PG_MAX_PARAM_COUNT + 1, 0 };
  if (ku_pg_validate_query_params(too_many, &total_bytes, &validation_error, ~0ULL) || !ku_test_code_is(validation_error, "parameter_too_large")) return 20;
  ku_error_drop(&validation_error);
  free(aliases); free(alias_bytes);

  puts("pg parameter budget ok");
  return 0;
}
"#,
    );
    let harness_path = dir.join("pg-param-budget-harness.c");
    fs::write(&harness_path, harness).expect("write PG parameter C harness");
    let Some(exe) = compile_c_harness(&dir, &harness_path, "pg-param-budget-harness") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "pg parameter budget ok\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_pg_client_c_links_and_starts_when_toolchain_and_libpq_are_available() {
    let dir = unique_temp_dir("pg-client-host-compile");
    fs::write(
        dir.join("main.ku"),
        r#"import pg from "std.pg"
fn main(): null! {
    try {
        client = pg.client({ conninfo: "ku_ci_invalid_conninfo_keyword=1", max_connections: 1 })?
        client.close()
        println("unexpected connection")
    } catch(err) {
        println(err.domain)
        println(err.code)
    }
    return ok(null)
}
"#,
    )
    .expect("write pg client host compile fixture");

    let mut command = Command::new(ku_binary());
    command
        .current_dir(&dir)
        .args(["build", "--native", "main.ku", "-o", &exe_name("pg-client")]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("PG pool native build was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let configured_libpq = env::var_os("KU_PG_LIB").is_some();
    let required_libpq = env::var("KU_PG_LINK_REQUIRED").is_ok_and(|value| value == "1");
    if required_libpq {
        assert!(
            configured_libpq,
            "KU_PG_LINK_REQUIRED=1 requires an exact KU_PG_LIB directory"
        );
    }
    let strict_libpq = configured_libpq || required_libpq;
    if !strict_libpq
        && !output.status.success()
        && (combined.contains("C compiler not found")
            || combined.contains("requires an exact shared/import libpq library"))
    {
        eprintln!("skip: host C compiler or libpq development library unavailable: {combined}");
        fs::remove_dir_all(&dir).ok();
        return;
    }
    assert!(
        output.status.success(),
        "native PostgreSQL client C failed to compile/link:\n{combined}"
    );

    let executable = dir.join(exe_name("pg-client"));
    let mut run = Command::new(&executable);
    run.current_dir(&dir);
    let started = run_bounded(&mut run, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("linked PG client could not start safely: {error}"));
    let stdout = String::from_utf8_lossy(&started.stdout).replace('\r', "");
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert_eq!(stdout, "pg\nconnect_error\n");
    assert!(
        stderr.is_empty(),
        "linked PG client wrote unexpected stderr: {stderr}"
    );
    assert!(
        started.status.success(),
        "linked PG client failed to start or close its libpq error path: {:?}",
        started.status.code()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_mysql_client_c_links_and_starts_when_toolchain_and_library_are_available() {
    let dir = unique_temp_dir("mysql-client-host-compile");
    fs::write(
        dir.join("main.ku"),
        r#"import mysql from "std.mysql"
fn main(): null! {
    try {
        client = mysql.client({
            host: "127.0.0.1",
            port: 1,
            user: "ku_runtime_probe",
            password: "not-a-secret",
            database: "ku_runtime_probe",
            max_connections: 1,
            max_waiters: 0,
            connect_timeout_ms: 100,
            acquire_timeout_ms: 100,
            query_timeout_ms: 100
        })?
        client.close()
        println("unexpected connection")
    } catch(err) {
        println(err.domain)
        println(err.code)
    }
    return ok(null)
}
"#,
    )
    .expect("write MySQL client host compile fixture");

    let mut command = Command::new(ku_binary());
    command.current_dir(&dir).args([
        "build",
        "--native",
        "main.ku",
        "-o",
        &exe_name("mysql-client"),
    ]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("MySQL client native build was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let configured_library = env::var_os("KU_MYSQL_LIB").is_some();
    let required_library = env::var("KU_MYSQL_LINK_REQUIRED").is_ok_and(|value| value == "1");
    if required_library {
        assert!(
            configured_library,
            "KU_MYSQL_LINK_REQUIRED=1 requires exact KU_MYSQL_LIB and KU_MYSQL_INCLUDE directories"
        );
        assert!(
            env::var_os("KU_MYSQL_INCLUDE").is_some(),
            "KU_MYSQL_LINK_REQUIRED=1 requires KU_MYSQL_INCLUDE"
        );
    }
    let strict_library = configured_library || required_library;
    if !strict_library
        && !output.status.success()
        && (combined.contains("C compiler not found")
            || combined.contains("requires an exact shared/import client library"))
    {
        eprintln!("skip: host C compiler or MySQL development library unavailable: {combined}");
        fs::remove_dir_all(&dir).ok();
        return;
    }
    assert!(
        output.status.success(),
        "native MySQL client C failed to compile/link:\n{combined}"
    );

    let executable = dir.join(exe_name("mysql-client"));
    let mut run = Command::new(&executable);
    run.current_dir(&dir);
    configure_mysql_runtime_search(&mut run);
    let started = run_bounded(&mut run, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("linked MySQL client could not start safely: {error}"));
    let stdout = String::from_utf8_lossy(&started.stdout).replace('\r', "");
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(
        started.status.success(),
        "linked MySQL client failed to load or execute its dynamic runtime path: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        started.status.code()
    );
    assert!(
        stdout == "mysql\nconnect_error\n" || stdout == "mysql\nconnect_timeout\n",
        "linked MySQL client returned an unexpected bounded connection outcome: {stdout:?}; stderr: {stderr}"
    );
    assert!(
        stderr.is_empty(),
        "linked MySQL client wrote unexpected stderr: {stderr}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_import_graph_binary_runs_after_sources_removed() {
    let dir = unique_temp_dir("import-graph");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("math.ku"),
        "fn Add(a:int, b:int): int {\n    return a + b\n}\n",
    )
    .expect("write math.ku");
    fs::write(
        src.join("main.ku"),
        "import { Add } from \"./math.ku\"\n\nfn main(): null! {\n    println(Add(1, 2))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "src/main.ku", "app") else {
        return;
    };

    // Stage 1 acceptance: the binary must not depend on the .ku source paths.
    fs::remove_dir_all(&src).expect("remove sources");
    assert!(!src.exists(), "source dir should be gone");

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.trim(), "3", "expected 3 after removing sources");
    assert_eq!(code, Some(0), "binary should exit cleanly");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_base_is_executable_relative_relocatable_and_source_free() {
    let container = unique_temp_dir("fs-relocatable-base");
    let original = container.join("original-tree");
    let source = original.join("source");
    let bin = original.join("bin");
    let build_cwd = container.join("build-cwd");
    let launch_cwd = container.join("launch-cwd");
    for directory in [&source, &bin, &build_cwd, &launch_cwd] {
        fs::create_dir_all(directory).expect("create relocatable fs test directory");
    }
    fs::write(
        source.join("math.ku"),
        "fn Label(): str { return \"import-ok\" }\n",
    )
    .expect("write imported module");
    fs::write(source.join("asset.txt"), "asset-data").expect("write source-relative asset");
    fs::write(
        source.join("main.ku"),
        r#"import { Label } from "./math.ku"
import fs from "std.fs"

fn main(): null! {
    println(Label())
    println(fs.read("asset.txt")?)
    println(fs.exists("asset.txt"))
    fs.write("written.txt", "written")?
    entries = fs.read_dir(".")?
    println(entries.len())
    return ok(null)
}
"#,
    )
    .expect("write relocatable entry");

    let entry = source.join("main.ku");
    let executable = bin.join(exe_name("app"));
    let mut command = Command::new(ku_binary());
    command
        .current_dir(&build_cwd)
        .arg("build")
        .arg("--backend")
        .arg("c")
        .arg(&entry)
        .arg("-o")
        .arg(&executable);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("relocatable native build was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let compiler_available = output.status.success();
    if !compiler_available && !combined.contains("C compiler not found") {
        panic!("relocatable native build failed unexpectedly:\n{combined}");
    }

    // C emission remains a hard gate even when this host has no linker.
    let c_artifact = native_generated_c(&source, "app");
    let c = fs::read_to_string(&c_artifact)
        .unwrap_or_else(|err| panic!("read generated C {}: {err}", c_artifact.display()));
    let original_text = original.to_string_lossy();
    assert!(c.contains("ku_fs_base_locator"));
    assert!(c.contains("ku_fs_wide_fully_qualified"));
    assert!(c.contains("ku_fs_wide_drive_relative"));
    assert!(c.contains("ku_fs_wide_root_relative"));
    assert!(c.contains("ku_fs_wide_unc"));
    assert!(c.contains("ku_fs_wide_verbatim_or_device"));
    assert!(c.contains("ku_fs_wide_unc_prefix_len"));
    assert!(c.contains("ku_fs_wide_two_component_prefix_len"));
    assert!(c.contains("ku_fs_wide_base_prefix_len"));
    assert!(
        c.contains("if (first_end == component_start || first_end >= len) return 0;")
            && c.contains("return second_end == second_start ? 0 : second_end;"),
        "a normal UNC path must contain both a non-empty server and share"
    );
    assert!(
        c.contains(
            "return ku_fs_wide_verbatim_or_device(path, len) || ku_fs_wide_unc(path, len) ||"
        ),
        "verbatim/device prefixes and fully-qualified UNC paths need separate classification"
    );
    assert!(
        !c.contains("ku_fs_wide_absolute"),
        "Windows rooted paths must not be conflated with fully-qualified paths"
    );
    assert!(
        c.contains("../source"),
        "artifact must use an executable-relative locator"
    );
    assert!(
        !c.contains(original_text.as_ref()),
        "artifact leaked the build-machine path"
    );
    assert!(
        !c.contains(&original_text.replace('\\', "/")),
        "artifact leaked a normalized build-machine path"
    );
    assert!(!c.contains("run_source"));
    assert!(!c.contains("const SOURCE"));

    if !compiler_available {
        eprintln!("skip runtime half: no C compiler available for native e2e test");
        fs::remove_dir_all(&container).ok();
        return;
    }

    fs::remove_file(source.join("main.ku")).expect("remove entry source");
    fs::remove_file(source.join("math.ku")).expect("remove imported source");
    fs::remove_dir_all(source.join(".ku")).expect("remove generated source-side artifacts");
    let relocated = container.join("relocated-tree");
    fs::rename(&original, &relocated).expect("relocate executable and source data together");

    let relocated_executable = relocated.join("bin").join(exe_name("app"));
    let mut command = Command::new(&relocated_executable);
    command.current_dir(&launch_cwd);
    let run = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
        panic!(
            "relocated native binary {} was not bounded: {error}",
            relocated_executable.display()
        )
    });
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).replace('\r', ""),
        "import-ok\nasset-data\ntrue\n2\n"
    );
    assert_eq!(
        fs::read_to_string(relocated.join("source").join("written.txt")).unwrap(),
        "written"
    );
    assert!(!launch_cwd.join("written.txt").exists());
    assert!(!relocated.join("bin").join("written.txt").exists());

    fs::remove_dir_all(&container).ok();
}

#[cfg(windows)]
#[test]
fn native_fs_windows_root_relative_paths_keep_executable_drive() {
    struct Cleanup {
        drives: Vec<String>,
        container: PathBuf,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            for drive in self.drives.iter().rev() {
                let _ = Command::new("subst").args([drive, "/D"]).status();
            }
            let _ = fs::remove_dir_all(&self.container);
        }
    }

    // Keep the native classifier aligned with the exact Windows PathBuf::join
    // contract used by the CLI when it computes the executable-relative base.
    // In particular, an incomplete UNC-looking prefix is still root-relative.
    let rust_base = Path::new(r"E:\source\dir");
    for (input, absolute, joined) in [
        (r"\foo", false, r"E:\foo"),
        (r"/foo", false, r"E:/foo"),
        (r"\\", false, r"E:\\"),
        (r"\\server", false, r"E:\\server"),
        (r"\\server\", false, r"E:\\server\"),
        (r"\\server\share", true, r"\\server\share"),
        (r"C:foo", false, r"C:foo"),
        (r"C:\foo", true, r"C:\foo"),
        (r"\\?\C:foo", true, r"\\?\C:foo"),
        (r"\\?\C:\foo", true, r"\\?\C:\foo"),
        (r"\\?\UNC\server\share", true, r"\\?\UNC\server\share"),
        (r"\??\C:\foo", false, r"E:\??\C:\foo"),
    ] {
        let input = Path::new(input);
        assert_eq!(input.is_absolute(), absolute, "is_absolute({input:?})");
        assert_eq!(rust_base.join(input), PathBuf::from(joined));
    }

    let container = unique_temp_dir("fs-windows-root-relative");
    let base_backing = container.join("base-drive");
    let launch_backing = container.join("launch-drive");
    fs::create_dir_all(&base_backing).expect("create temporary base drive");
    fs::create_dir_all(&launch_backing).expect("create temporary launch drive");
    let mut cleanup = Cleanup {
        drives: Vec::new(),
        container,
    };

    let (base_drive, launch_drive) = {
        let mut map_drive = |backing: &Path| -> Option<String> {
            for letter in (b'P'..=b'Z').rev() {
                let drive = format!("{}:", char::from(letter));
                if Path::new(&format!("{drive}\\")).exists() {
                    continue;
                }
                let Ok(status) = Command::new("subst").arg(&drive).arg(backing).status() else {
                    return None;
                };
                if status.success() {
                    cleanup.drives.push(drive.clone());
                    return Some(drive);
                }
            }
            None
        };

        let Some(base_drive) = map_drive(&base_backing) else {
            eprintln!("skip Windows cross-drive fs test: no temporary drive mapping available");
            return;
        };
        let Some(launch_drive) = map_drive(&launch_backing) else {
            eprintln!("skip Windows cross-drive fs test: second temporary drive unavailable");
            return;
        };
        (base_drive, launch_drive)
    };

    let base_root = PathBuf::from(format!("{base_drive}\\"));
    let launch_root = PathBuf::from(format!("{launch_drive}\\"));
    fs::write(base_root.join("ku-root-relative.txt"), "executable-drive")
        .expect("write executable-drive sentinel");
    fs::write(launch_root.join("ku-root-relative.txt"), "launch-drive")
        .expect("write launch-drive sentinel");

    fs::write(base_root.join("ku-drive-relative.txt"), "drive-relative")
        .expect("write drive-relative root sentinel");

    let source_dir = base_root.join("source");
    fs::create_dir_all(&source_dir).expect("create mapped source directory");
    fs::write(source_dir.join("ku-drive-relative.txt"), "drive-relative")
        .expect("write drive-relative directory sentinel");

    let ku_literal = |path: &str| format!("\"{}\"", path.replace('\\', "\\\\"));
    let drive_relative = format!("{base_drive}ku-drive-relative.txt");
    let verbatim = format!(r"\\?\{base_drive}\ku-root-relative.txt");
    let source = format!(
        "import fs from \"std.fs\"\n\nfn main(): null! {{\n    println(fs.read({})?)\n    println(fs.read({})?)\n    println(fs.exists({}))\n    println(fs.read({})?)\n    return ok(null)\n}}\n",
        ku_literal("/ku-root-relative.txt"),
        ku_literal(r"\ku-root-relative.txt"),
        ku_literal(&drive_relative),
        ku_literal(&verbatim),
    );
    fs::write(source_dir.join("main.ku"), source).expect("write root-relative fs source");

    let Some(exe) = native_build(&source_dir, "main.ku", "root-relative") else {
        return;
    };
    let mut command = Command::new(&exe);
    command.current_dir(&launch_root);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("root-relative native binary was not bounded: {error}"));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        concat!(
            "executable-drive\n", // forward-slash root-relative
            "executable-drive\n", // backslash root-relative
            "true\n",             // drive-relative path is not appended to the base
            "executable-drive\n", // verbatim/device prefix remains fully-qualified
        ),
        "Windows rooted, drive-relative, and verbatim paths must match PathBuf::join"
    );
}

#[test]
fn native_kustring_clone_prints_utf8_twice() {
    let dir = unique_temp_dir("kustring-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    a = \"hé\" + \"llo\"\n    b = a.clone()\n    println(a)\n    println(b)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "kustr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "héllo\nhéllo\n",
        "clone must deep-copy and print UTF-8 by length, not NUL"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kustring_clone_of_owned_empty_string_drops_cleanly() {
    let dir = unique_temp_dir("kustring-empty-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    a = \"\" + \"\"\n    b = a.clone()\n    println(a.len())\n    println(b.len())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "kustrempty") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "0\n0\n");
    assert_eq!(code, Some(0), "both owned empty strings must drop cleanly");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kustring_prints_embedded_nul_by_length() {
    let dir = unique_temp_dir("kustring-nul-print");
    fs::write(dir.join("nul.bin"), b"a\0b").expect("write NUL input");
    fs::write(
        dir.join("main.ku"),
        "import fs from \"std.fs\"\n\nfn main(): null! {\n    println(fs.read(\"nul.bin\")?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "kustrnul") else {
        return;
    };
    let (stdout, code) = run_binary_bytes(&exe);
    let normalized: Vec<u8> = stdout.into_iter().filter(|byte| *byte != b'\r').collect();
    assert_eq!(normalized, b"a\0b\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_nested_array_clone_does_not_double_free() {
    let dir = unique_temp_dir("nested-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    a = [[1, 2], [3, 4]]\n    b = a.clone()\n    println(a[0][0])\n    println(b[1][1])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nested") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n4\n");
    // Regression: `a[0]` reads an owned element as a borrow; dropping it as if it
    // owned the container's inner pointer used to double-free (0xC0000374).
    assert_eq!(code, Some(0), "reading a[0] must borrow, not double-free");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_of_results_clones_and_drops_payloads() {
    // `KuArray_Result<T>` embeds Result values and its helpers call the Result
    // clone/drop ABI. Both the Result typedef and those helpers must therefore be
    // available in dependency order. The nested successful `[str]` payload makes
    // Result<Array<T>> depend on the inner array helpers, while the outer
    // Array<Result<Array<T>>> depends back on the Result helpers. This is an
    // acyclic inner-before-outer graph, not a reason to shallow-copy either owner.
    // Owned ok/error payloads make missing clone/drop visible as heap corruption.
    let dir = unique_temp_dir("result-array");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn Good(): int! { return ok(7) }\n",
            "fn Bad(): int! { fail \"b\" + \"oom\" }\n\n",
            "fn Words(): [str]! { return ok([\"K\" + \"u\"]) }\n\n",
            "fn main(): null! {\n",
            "    results: [int!] = [Good(), Bad()]\n",
            "    copy = results.clone()\n",
            "    nested: [[str]!] = [Words()]\n",
            "    nested_copy = nested.clone()\n",
            "    println(results.len())\n",
            "    println(copy.len())\n",
            "    println(nested.len())\n",
            "    println(nested_copy.len())\n",
            "    return ok(null)\n",
            "}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "resultarray") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "2\n2\n1\n1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_push_len_match_interpreter() {
    let dir = unique_temp_dir("array-push");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    nums = [1, 2, 3]\n    more = nums.push(4)\n    println(nums.len())\n    println(more.len())\n    println(more[3])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "push") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // push is immutable: nums stays length 3, the returned array is length 4.
    assert_eq!(stdout.replace('\r', ""), "3\n4\n4\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_try_get_ok_path() {
    let dir = unique_temp_dir("try-get-ok");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    nums = [10, 20, 30]\n    got = nums.try_get(1)?\n    println(got)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "tryget") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "20\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_try_get_out_of_bounds_propagates_err() {
    let dir = unique_temp_dir("try-get-oob");
    // `nums[i]` would abort; `try_get(i)?` returns a recoverable Err that `?`
    // propagates, so the main wrapper exits non-zero instead of crashing.
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    nums = [10, 20, 30]\n    bad = nums.try_get(9)?\n    println(bad)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "trygetoob") else {
        return;
    };

    let (_stdout, code) = run_binary(&exe);
    assert_eq!(
        code,
        Some(1),
        "out-of-bounds try_get must propagate an error"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_direct_array_bounds_failure_uses_structured_identifier() {
    let dir = unique_temp_dir("array-direct-oob");
    fs::write(
        dir.join("main.ku"),
        "fn main() { values = [1, 2, 3]\nprintln(values[9])\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "arraydirectoob") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let mut command = Command::new(&exe);
    command.current_dir(&dir);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("direct array bounds failure was not bounded: {error}"));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("array/index_out_of_bounds"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_array_index_overwrite_drops_old_value_and_evaluates_once() {
    let dir = unique_temp_dir("array-owned-overwrite");
    fs::write(
        dir.join("main.ku"),
        r#"
fn main(): null! {
    events = ""
    rhs_calls = 0
    index_calls = 0
    rhs = () => {
        events = events + "R"
        rhs_calls = rhs_calls + 1
        return "new" + ""
    }
    index = () => {
        events = events + "I"
        index_calls = index_calls + 1
        return 0
    }
    values = ["old" + ""]

    values[index()] = rhs()
    println(events)
    println(rhs_calls)
    println(index_calls)
    println(values[0])

    values[index()] += rhs()
    println(events)
    println(rhs_calls)
    println(index_calls)
    println(values[0])
    return ok(null)
}
"#,
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "arrayoverwrite") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "RI\n1\n1\nnew\nRIRI\n2\n2\nnewnew\n"
    );
    assert_eq!(
        code,
        Some(0),
        "owned slot overwrite must drop exactly one old owner"
    );
    let c_path = native_generated_c(&dir, "arrayoverwrite");
    let c = fs::read_to_string(c_path).expect("read generated C");
    assert!(
        c.contains("ku_string_drop(&(*__ku_slot));"),
        "owned array overwrite must drop the old string payload before storing"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_element_field_assignment_updates_the_real_slot_once() {
    let dir = unique_temp_dir("array-field-place");
    fs::write(
        dir.join("main.ku"),
        r#"
struct Entry { name: str, count: int }

fn main(): null! {
    events = ""
    rhs_calls = 0
    index_calls = 0
    next_int = () => {
        events = events + "R"
        rhs_calls = rhs_calls + 1
        return 4
    }
    next_str = () => {
        events = events + "R"
        rhs_calls = rhs_calls + 1
        return "new" + ""
    }
    index = () => {
        events = events + "I"
        index_calls = index_calls + 1
        return 0
    }
    entries = [Entry { name: "old" + "", count: 1 }]

    entries[index()].count = next_int()
    entries[index()].name = next_str()
    entries[index()].count += next_int()

    println(events)
    println(rhs_calls)
    println(index_calls)
    println(entries[0].name)
    println(entries[0].count)
    return ok(null)
}
"#,
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "arrayfieldplace") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "RIRIRI\n3\n3\nnew\n8\n");
    assert_eq!(
        code,
        Some(0),
        "field stores must update the array slot without double free"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fail_object_catch_fields_and_finally() {
    let dir = unique_temp_dir("fail-object-catch");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    try {\n        fail {\n            domain: \"test\",\n            code: \"failed\",\n            message: \"boom\"\n        }\n    } catch(err) {\n        println(err.domain)\n        println(err.code)\n        println(err.message)\n    } finally {\n        println(\"cleanup\")\n    }\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "failobj") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "test\nfailed\nboom\ncleanup\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fail_object_propagates_via_question() {
    let dir = unique_temp_dir("fail-object-prop");
    fs::write(
        dir.join("main.ku"),
        "fn Load(): str! {\n    fail {\n        domain: \"fs\",\n        code: \"read_failed\",\n        message: \"cannot read\"\n    }\n}\n\nfn main(): null! {\n    text = Load()?\n    println(text)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "failprop") else {
        return;
    };

    let (_stdout, code) = run_binary(&exe);
    assert_eq!(code, Some(1), "fail must propagate to a non-zero exit");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_return_through_finally_runs_cleanup() {
    let dir = unique_temp_dir("return-finally");
    fs::write(
        dir.join("main.ku"),
        "fn value(flag:bool): int! {\n    try {\n        if (flag) {\n            return ok(7)\n        }\n    } finally {\n        println(\"cleanup\")\n    }\n    return ok(9)\n}\n\nfn main(): null! {\n    v = value(true)?\n    println(v)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "retfinally") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // finally runs even though try returns; the returned 7 flows through it.
    assert_eq!(stdout.replace('\r', ""), "cleanup\n7\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_object_index_strict_read() {
    let dir = unique_temp_dir("object-read");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    obj = { name: \"Ku\", age: 18 }\n    println(obj[\"name\"]?)\n    println(obj[\"age\"]?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "objread") else {
        return;
    };

    // obj[key]? yields a KuValue printed by tag (str -> Ku, int -> 18).
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Ku\n18\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_object_missing_key_and_get_or() {
    let dir = unique_temp_dir("object-missing");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    obj = { name: \"Ku\" }\n    try {\n        v = obj[\"age\"]?\n        println(v)\n    } catch(err) {\n        println(err.code)\n    }\n    println(obj.get_or(\"age\", 99))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "objmiss") else {
        return;
    };

    // Missing key -> Err{code:"missing_key"} caught; get_or returns the default.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "missing_key\n99\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_dynamic_object_store_bracket_and_dot_are_owned_and_readable() {
    let dir = unique_temp_dir("object-store");
    fs::write(
        dir.join("main.ku"),
        r#"fn main(): null! {
    obj = { name: "old" }
    obj.name = "dot"
    key = "count"
    obj[key] = 3
    name = obj["name"]?.as_str()?
    count = obj["count"]?.as_int()?
    println(name)
    println(count)
    return ok(null)
}
"#,
    )
    .expect("write dynamic object store source");

    // This remains a hard C-artifact gate on hosts without a C compiler.
    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_object_try_set_copy_key(__ku_object_store_target"));
    assert!(c.contains("KuString __ku_object_store_key"));
    assert!(!c.contains("__ku_object_store_target->name"));

    let Some(exe) = native_build(&dir, "main.ku", "objectstore") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "dot\n3\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_dynamic_object_store_replaces_owned_string_object_and_nested_array() {
    let dir = unique_temp_dir("object-store-owned");
    fs::write(
        dir.join("main.ku"),
        r#"fn main(): null! {
    obj = {
        name: "old" + " value",
        child: { label: "before" + " value" },
        matrix: [[1, 2]]
    }
    obj["name"] = "new" + " value"
    obj["child"] = { label: "after" + " value" }
    obj["matrix"] = [[3, 4], [5, 6]]
    name = obj["name"]?.as_str()?
    child = obj["child"]?
    label = child["label"]?.as_str()?
    matrix = obj["matrix"]?
    row = matrix[1]?
    value = row[0]?.as_int()?
    println(name)
    println(label)
    println(value)
    return ok(null)
}
"#,
    )
    .expect("write owned dynamic object store source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_try_v_typed_array_array_int"));
    assert!(c.contains("ku_value_drop(&__ku_object_store_value);"));

    let Some(exe) = native_build(&dir, "main.ku", "objectstoreowned") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "new value\nafter value\n5\n");
    assert_eq!(
        code,
        Some(0),
        "replacing owned object values must not double-free"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_dynamic_object_store_evaluates_rhs_before_key_once() {
    let dir = unique_temp_dir("object-store-order");
    fs::write(
        dir.join("main.ku"),
        r#"fn main(): null! {
    events = ""
    rhs_calls = 0
    key_calls = 0
    rhs = () => {
        events = events + "R"
        rhs_calls = rhs_calls + 1
        return "value" + ""
    }
    key = () => {
        events = events + "K"
        key_calls = key_calls + 1
        return "name"
    }
    obj = { name: "old" }
    obj[key()] = rhs()
    value = obj["name"]?.as_str()?
    println(events)
    println(rhs_calls)
    println(key_calls)
    println(value)
    return ok(null)
}
"#,
    )
    .expect("write dynamic object evaluation-order source");

    let Some(exe) = native_build(&dir, "main.ku", "objectstoreorder") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "RK\n1\n1\nvalue\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_dynamic_object_store_oom_hard_fails_after_cleanup() {
    let cases = [
        (
            "object-store-string-clone-oom",
            r#"fn main(): null! {
    obj = { keep: "old" }
    key = "new" + " key"
    value = "owned" + " string"
    obj[key] = value
    return ok(null)
}
"#,
            "string_clone",
        ),
        (
            "object-store-rehash-oom",
            r#"fn main(): null! {
    obj = { a: 1, b: 2, c: 3, d: 4, e: 5 }
    value = "owned" + " string"
    obj["f"] = value
    return ok(null)
}
"#,
            "object_rehash",
        ),
        (
            "object-store-typed-array-oom",
            r#"fn main(): null! {
    obj = { keep: 1 }
    values = ["a" + "b", "c" + "d"]
    obj["values"] = values
    return ok(null)
}
"#,
            "value_array_data",
        ),
    ];

    for (name, source, site) in cases {
        let dir = unique_temp_dir(name);
        fs::write(dir.join("main.ku"), source).expect("write dynamic object OOM source");

        let c = native_emit_c(&dir, "main.ku");
        assert!(c.contains("ku_object_try_set_copy_key"));
        assert!(c.contains("ku_value_drop(&__ku_object_store_value);"));
        if site == "value_array_data" {
            assert!(c.contains("ku_try_v_typed_array_str"));
        }

        let Some(exe) = native_build_with_object_oom_hook(&dir, "main.ku", "objectstoreoom") else {
            fs::remove_dir_all(&dir).ok();
            return;
        };
        let (stdout, stderr, code) = run_binary_with_object_oom(&exe, site, 1);
        assert!(
            stdout.is_empty(),
            "unexpected stdout for fault site {site}: {stdout}"
        );
        assert_eq!(
            stderr.replace('\r', ""),
            "object/out_of_memory: object allocation failed\n",
            "unexpected stderr for fault site {site}"
        );
        assert_eq!(code, Some(1), "fault site {site} must hard-fail the Store");

        fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn native_object_strict_clone_oom_is_result_and_preserves_source() {
    let dir = unique_temp_dir("object-clone-oom");
    fs::write(
        dir.join("main.ku"),
        r#"fn main(): null! {
    owned = "own" + "ed"
    obj = { value: owned }
    try {
        ignored = obj["value"]?
        println(ignored)
    } catch (err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    println(obj["value"]?)
    return ok(null)
}
"#,
    )
    .expect("write object clone OOM source");

    let Some(exe) = native_build_with_object_oom_hook(&dir, "main.ku", "objectcloneoom") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, stderr, code) = run_binary_with_object_oom(&exe, "string_clone", 1);
    assert_eq!(
        stdout.replace('\r', ""),
        "object\nout_of_memory\nobject allocation failed\nowned\n"
    );
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_index_recursive_clone_oom_is_result_and_preserves_source() {
    let dir = unique_temp_dir("kuvalue-index-clone-oom");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn main(): null! {
    value = json.parse("[{\"name\":\"Ku\"}]")?
    try {
        ignored = value[0]?
        println(ignored)
    } catch (err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    copy = value[0]?
    println(copy["name"]?)
    return ok(null)
}
"#,
    )
    .expect("write recursive index clone OOM source");

    let Some(exe) = native_build_with_object_oom_hook(&dir, "main.ku", "indexcloneoom") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    // The parsed nested object consumes object_header #1. Cloning it through
    // value[0]? consumes #2; failing there must leave the parsed source intact.
    let (stdout, stderr, code) = run_binary_with_object_oom(&exe, "object_header", 2);
    assert_eq!(
        stdout.replace('\r', ""),
        "object\nout_of_memory\nobject allocation failed\nKu\n"
    );
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_result_object_payload_moves_and_drops_cleanly() {
    let dir = unique_temp_dir("result-object");
    fs::write(
        dir.join("main.ku"),
        "fn load() {\n    return ok({ name: \"Ku\" })\n}\n\nfn main(): null! {\n    obj = load()?\n    println(obj[\"name\"]?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "resultobject") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Ku\n");
    assert_eq!(code, Some(0));

    let c_path = native_generated_c(&dir, "resultobject");
    let c = fs::read_to_string(c_path).expect("read generated C");
    assert!(c.contains("ku_object_drop(result->value)"));
    assert!(!c.contains("ku_drop_struct___ku_object"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_result_struct_and_enum_owned_payloads_move_and_drop_cleanly() {
    let dir = unique_temp_dir("result-named-owned");
    fs::write(
        dir.join("main.ku"),
        r#"struct Box { text: str }

enum MaybeText {
    Some(value: str)
    None
}

fn load_box(): Box! {
    return ok(Box { text: "bo" + "x" })
}

fn load_enum(): MaybeText! {
    return ok(MaybeText.Some("en" + "um"))
}

fn main(): null! {
    boxed = load_box()?
    println(boxed.text)
    value = load_enum()?
    text = match value {
        MaybeText.Some(payload) => payload
        MaybeText.None => "none"
    }
    println(text)
    return ok(null)
}
"#,
    )
    .expect("write named Result payload source");

    let Some(exe) = native_build(&dir, "main.ku", "resultnamedowned") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "box\nenum\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_as_int_as_str_chain() {
    let dir = unique_temp_dir("kuvalue-as");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    obj = { age: 18, name: \"Ku\" }\n    n = obj[\"age\"]?.as_int()?\n    s = obj[\"name\"]?.as_str()?\n    println(n)\n    println(s)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "kvas") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "18\nKu\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_time_wall_object_elapsed_and_steady_clock() {
    let dir = unique_temp_dir("time-native");
    fs::write(
        dir.join("main.ku"),
        r#"
import time from "std.time"

fn main(): null! {
    epoch = time.now()
    current = time.instant()
    elapsed = time.elapsed(current)
    steady_before = time.steady_millis()
    steady_after = time.steady_millis()

    println(current.kind)
    println(time.millis(current) == current.millis)
    println(elapsed == elapsed)
    println(epoch > 0)
    println(steady_after >= steady_before)
    println(current)
    return ok(null)
}
"#,
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nativetime") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    let normalized = stdout.replace('\r', "");
    let lines = normalized.lines().collect::<Vec<_>>();
    assert_eq!(&lines[..5], ["time.time", "true", "true", "true", "true"]);
    assert!(
        lines.get(5).is_some_and(
            |line| line.starts_with("{ kind: time.time, millis: ") && line.ends_with(" }")
        ),
        "unexpected Time display: {normalized:?}"
    );
    assert_eq!(code, Some(0));

    let c_path = native_generated_c(&dir, "nativetime");
    let c = fs::read_to_string(c_path).expect("read generated C");
    let steady_start = c
        .find("static int64_t ku_time_steady_millis")
        .expect("steady clock helper must be emitted");
    let steady_tail = &c[steady_start..];
    let steady_end = steady_tail
        .find("static KuTime ku_time_instant")
        .unwrap_or(steady_tail.len());
    let steady_helper = &steady_tail[..steady_end];
    assert!(
        steady_helper.contains("GetTickCount64")
            && steady_helper.contains("clock_gettime(CLOCK_MONOTONIC"),
        "steady clock helper must use the platform monotonic clock in both C branches"
    );
    assert!(
        !steady_helper.contains("timespec_get") && !steady_helper.contains("TIME_UTC"),
        "steady clock helper must not fall back to a wall clock"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_time_virtual_kind_field_is_not_assignable() {
    let dir = unique_temp_dir("time-kind-read-only");
    fs::write(
        dir.join("main.ku"),
        "import time from \"std.time\"\nfn main() { value = time.instant()\nvalue.kind = \"changed\"\n}\n",
    )
    .expect("write main.ku");

    let mut command = Command::new(ku_binary());
    command
        .current_dir(&dir)
        .args(["build", "--native", "main.ku"]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("invalid Time.kind build was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "Time.kind assignment must fail");
    assert!(
        combined.contains("native Time.kind is read-only"),
        "unexpected diagnostic: {combined}"
    );
    assert!(
        !dir.join("main.c").exists(),
        "an invalid Time.kind lvalue must not produce uncompilable C"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_time_equality_compares_validated_values() {
    let dir = unique_temp_dir("time-equality");
    fs::write(
        dir.join("main.ku"),
        r#"import time from "std.time"

fn main(): null! {
    first = time.instant()
    same = first.clone()
    second = time.instant()
    second.millis = first.millis + 1
    println(first == same)
    println(first != second)
    return ok(null)
}
"#,
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nativetimeequality") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "true\ntrue\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_time_elapsed_overflow_fails_without_wrapping() {
    let dir = unique_temp_dir("time-overflow");
    fs::write(
        dir.join("main.ku"),
        r#"
import time from "std.time"

fn main(): null! {
    previous = time.instant()
    previous.millis = -9223372036854775807 - 1
    println(time.elapsed(previous))
    return ok(null)
}
"#,
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nativetimeoverflow") else {
        return;
    };
    let mut command = Command::new(&exe);
    command.current_dir(exe.parent().unwrap_or_else(|| Path::new(".")));
    let output =
        run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!(
                "native binary {} did not complete safely: {error}",
                exe.display()
            )
        });
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("time.elapsed: elapsed milliseconds overflow"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_write_read_roundtrip() {
    let dir = unique_temp_dir("fs-rt");
    fs::write(
        dir.join("main.ku"),
        "import fs from \"std.fs\"\n\nfn main(): null! {\n    fs.write(\"s7.txt\", \"native fs works\")?\n    println(fs.read(\"s7.txt\")?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fsrt") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "native fs works\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_intrinsic_gates_emit_complete_cross_platform_helpers() {
    let dir = unique_temp_dir("fs-gates-c");
    let cases = [
        (
            "read",
            "import fs from \"std.fs\"\nfn load(): str! { return fs.read(\"missing.txt\") }\nfn main() {}\n",
            &["KuResult_str", "ku_fs_read", "ku_fs_read_impl"][..],
            &["ku_fs_try_read", "ku_fs_try_write", "ku_fs_read_dir"][..],
        ),
        (
            "write",
            "import fs from \"std.fs\"\nfn save(): null! { return fs.write(\"out.txt\", \"ok\") }\nfn main() {}\n",
            &["KuResult_null", "ku_fs_write", "ku_fs_write_impl"][..],
            &["ku_fs_try_read", "ku_fs_try_write", "ku_fs_read_dir"][..],
        ),
        (
            "try_read",
            "import fs from \"std.fs\"\nfn load(): str! { return fs.try_read(\"missing.txt\") }\nfn main() {}\n",
            &["KuResult_str", "ku_fs_try_read", "ku_fs_read_impl"][..],
            &["ku_fs_try_write", "ku_fs_read_dir"][..],
        ),
        (
            "try_write",
            "import fs from \"std.fs\"\nfn save(): null! { return fs.try_write(\"out.txt\", \"ok\") }\nfn main() {}\n",
            &["KuResult_null", "ku_fs_try_write", "ku_fs_write_impl"][..],
            &["ku_fs_try_read", "ku_fs_read_dir"][..],
        ),
        (
            "read_dir",
            "import fs from \"std.fs\"\nfn list(): [str]! { return fs.read_dir(\".\") }\nfn main() {}\n",
            &["KuArray_str", "KuResult_array_str", "ku_fs_read_dir"][..],
            &["ku_fs_try_read", "ku_fs_try_write"][..],
        ),
    ];

    for (name, source, required, forbidden) in cases {
        let case_dir = dir.join(name);
        fs::create_dir_all(&case_dir).expect("create gate case dir");
        fs::write(case_dir.join("main.ku"), source).expect("write gate source");
        let c = native_emit_c(&case_dir, "main.ku");
        for symbol in required {
            assert!(c.contains(symbol), "{name} artifact missing {symbol}");
        }
        for symbol in forbidden {
            assert!(
                !c.contains(symbol),
                "{name} artifact unexpectedly emitted {symbol}"
            );
        }
        assert!(c.contains("MultiByteToWideChar"));
        assert!(c.contains("_wfopen") || name == "read_dir");
        assert!(c.contains("opendir") || name != "read_dir");
        assert!(c.contains("#if defined(_WIN32)"));
    }

    let no_fs = dir.join("no_fs");
    fs::create_dir_all(&no_fs).expect("create no-fs dir");
    fs::write(no_fs.join("main.ku"), "fn main() { println(\"ok\") }\n")
        .expect("write no-fs source");
    let c = native_emit_c(&no_fs, "main.ku");
    assert!(!c.contains("KU_FS_MAX_PATH_BYTES"));
    assert!(!c.contains("ku_fs_native_path"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_result_intrinsics_compile_and_run_independently() {
    let dir = unique_temp_dir("fs-gates-link");
    let cases = [
        (
            "read",
            "import fs from \"std.fs\"\nfn main(): null! {\n    try {\n        fs.read(\"missing.txt\")?\n    } catch (err) {\n        println(err.code)\n    }\n    return ok(null)\n}\n",
            "read_failed\n",
        ),
        (
            "write",
            "import fs from \"std.fs\"\nfn main(): null! {\n    fs.write(\"out.txt\", \"ok\")?\n    println(\"write_ok\")\n    return ok(null)\n}\n",
            "write_ok\n",
        ),
        (
            "try_read",
            "import fs from \"std.fs\"\nfn main(): null! {\n    try {\n        fs.try_read(\"missing.txt\")?\n    } catch (err) {\n        println(err.code)\n    }\n    return ok(null)\n}\n",
            "read_failed\n",
        ),
        (
            "try_write",
            "import fs from \"std.fs\"\nfn main(): null! {\n    fs.try_write(\"out.txt\", \"ok\")?\n    println(\"write_ok\")\n    return ok(null)\n}\n",
            "write_ok\n",
        ),
        (
            "read_dir",
            "import fs from \"std.fs\"\nfn main(): null! {\n    entries = fs.read_dir(\"empty\")?\n    println(entries.len())\n    return ok(null)\n}\n",
            "0\n",
        ),
    ];

    for (name, source, expected) in cases {
        let case_dir = dir.join(name);
        fs::create_dir_all(case_dir.join("empty")).expect("create independent fs case");
        fs::write(case_dir.join("main.ku"), source).expect("write independent fs source");
        let Some(exe) = native_build(&case_dir, "main.ku", name) else {
            fs::remove_dir_all(&dir).ok();
            return;
        };
        let (stdout, code) = run_binary(&exe);
        assert_eq!(
            stdout.replace('\r', ""),
            expected,
            "independent {name} output"
        );
        assert_eq!(code, Some(0), "independent {name} exit");
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_unicode_paths_nul_content_exists_and_sorted_read_dir() {
    let dir = unique_temp_dir("fs-unicode");
    let listing = dir.join("目录");
    fs::create_dir_all(&listing).expect("create unicode directory");
    let first = listing.join("a.txt");
    let second = listing.join("中.bin");
    fs::write(&first, "first").expect("write first unicode-dir entry");
    fs::write(&second, b"a\0b").expect("write embedded-NUL content");
    let ku_path = |path: &Path| path.display().to_string().replace('\\', "\\\\");
    let source = format!(
        r#"import fs from "std.fs"
fn main(): null! {{
    if (!fs.exists("{}") || !fs.exists("{}")) {{
        panic("unicode file or directory should exist")
    }}
    entries = fs.read_dir("{}")?
    if (entries.len() != 2 || entries[0] != "{}" || entries[1] != "{}") {{
        panic("directory entries should be sorted full paths")
    }}
    println("sorted")
    print(fs.read("{}")?)
    return ok(null)
}}
"#,
        ku_path(&listing),
        ku_path(&second),
        ku_path(&listing),
        ku_path(&first),
        ku_path(&second),
        ku_path(&second),
    );
    fs::write(dir.join("main.ku"), source).expect("write unicode fs source");

    let Some(exe) = native_build(&dir, "main.ku", "fsunicode") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary_bytes(&exe);
    let normalized: Vec<u8> = stdout.into_iter().filter(|byte| *byte != b'\r').collect();
    assert_eq!(normalized, b"sorted\na\0b");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_exists_follows_targets_and_closes_windows_handles() {
    let dir = unique_temp_dir("fs-exists-follow-target");
    fs::write(dir.join("present.txt"), "present").expect("write exists fixture");
    fs::create_dir_all(dir.join("present-dir")).expect("create exists directory fixture");

    #[cfg(windows)]
    let dangling_link_created = {
        use std::os::windows::fs::symlink_file;
        match symlink_file(
            dir.join("missing-target.txt"),
            dir.join("dangling-link.txt"),
        ) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("skip Windows dangling-symlink subcase: {error}");
                false
            }
        }
    };
    #[cfg(not(windows))]
    let dangling_link_created = {
        use std::os::unix::fs::symlink;
        symlink("missing-target.txt", dir.join("dangling-link.txt"))
            .expect("create POSIX dangling symlink fixture");
        true
    };

    fs::write(
        dir.join("main.ku"),
        r#"import fs from "std.fs"
fn main() {
    println(fs.exists("present.txt"))
    println(fs.exists("present-dir"))
    println(fs.exists("dangling-link.txt"))
}
"#,
    )
    .expect("write fs.exists source");

    // Generated C is the hard gate on every host: Windows must open the target
    // (not inspect the reparse point), share with concurrent readers/writers/
    // deleters, support directories, and close every successful handle.
    let c = native_emit_c(&dir, "main.ku");
    for required in [
        "HANDLE handle = CreateFileW(",
        "FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE",
        "OPEN_EXISTING",
        "FILE_FLAG_BACKUP_SEMANTICS",
        "if (exists) CloseHandle(handle);",
    ] {
        assert!(
            c.contains(required),
            "generated fs.exists missing {required}"
        );
    }
    assert!(
        !c.contains("GetFileAttributesW(native)"),
        "GetFileAttributesW observes a dangling reparse point instead of following its target"
    );

    let Some(exe) = native_build(&dir, "main.ku", "fsexists") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "true\ntrue\nfalse\n");
    assert_eq!(code, Some(0));
    if dangling_link_created {
        assert!(
            fs::symlink_metadata(dir.join("dangling-link.txt")).is_ok(),
            "the false result must come from a dangling link, not a missing fixture"
        );
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_fs_limits_invalid_paths_and_errors_are_closed_results() {
    let dir = unique_temp_dir("fs-limits");
    fs::write(dir.join("one.bin"), vec![b'a'; 1_000_000]).expect("write max-size input");
    fs::write(dir.join("too-large.bin"), vec![b'a'; 1_000_001]).expect("write oversized input");
    fs::write(dir.join("nul-path.bin"), b"real.txt\0ignored").expect("write NUL path");
    fs::write(dir.join("long-path.txt"), vec![b'x'; 32 * 1024 + 1]).expect("write long path input");
    fs::write(dir.join("invalid-utf8.bin"), [0xff]).expect("write invalid UTF-8 input");
    fs::write(dir.join("real.txt"), "real").expect("write NUL truncation sentinel");
    fs::write(dir.join("target.txt"), "keep").expect("write truncation sentinel");
    fs::write(
        dir.join("main.ku"),
        r#"import fs from "std.fs"
fn main(): null! {
    try {
        fs.try_read("too-large.bin")?
        panic("oversized read should fail")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    content = fs.read("one.bin")? + "x"
    try {
        fs.try_write("target.txt", content)?
        panic("oversized write should fail")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    nul_path = fs.read("nul-path.bin")?
    println(fs.exists(nul_path))
    try {
        fs.try_read(nul_path)?
        panic("NUL path should fail")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    long_path = fs.read("long-path.txt")?
    println(fs.exists(long_path))
    try {
        fs.try_read(long_path)?
        panic("long path should fail")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    try {
        fs.try_read("invalid-utf8.bin")?
        panic("invalid UTF-8 should fail")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    try {
        fs.read_dir("missing-directory")?
        panic("missing directory should fail")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }
    return ok(null)
}
"#,
    )
    .expect("write fs limit source");

    let Some(exe) = native_build(&dir, "main.ku", "fslimits") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "fs/file_too_large\nfs/content_too_large\nfalse\nfs/read_failed\nfalse\nfs/read_failed\nfs/read_failed\nfs/read_dir_failed\n"
    );
    assert_eq!(code, Some(0));
    assert_eq!(fs::read_to_string(dir.join("target.txt")).unwrap(), "keep");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_parse_read_and_convert() {
    let dir = unique_temp_dir("json-rt");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    obj = json.parse(\"{\\\"name\\\":\\\"Ku\\\",\\\"age\\\":18}\")?\n    println(obj[\"name\"]?.as_str()?)\n    println(obj[\"age\"]?.as_int()?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonrt") else {
        return;
    };
    let generated_c = fs::read_to_string(native_generated_c(&dir, "jsonrt"))
        .expect("read JSON Result generated C");
    assert!(generated_c.contains("static KuResult_kuvalue ku_json_parse(KuString text)"));
    assert!(generated_c.contains("static KuResult_str ku_json_stringify(KuValue value)"));
    assert!(!generated_c.contains("ku_json_panic"));

    // json.parse -> KuValue -> obj[key]? -> as_str/as_int, native == interpreter.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Ku\n18\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_equality_is_structural_borrowed_and_array_aware() {
    let dir = unique_temp_dir("kuvalue-equality");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

struct Note { text: str }

fn main(): null! {
    integer = json.parse("1")?
    decimal = json.parse("0.5")?
    boolean = json.parse("true")?
    nothing = json.parse("null")?
    text = json.parse("\"Ku\"")?
    owned = "K" + "u"
    println(integer == 1)
    println(1 == integer)
    println(integer == 1.0)
    println(decimal == 0.5)
    println(boolean == true)
    println(nothing == null)
    println(null == nothing)
    println(text == owned)
    println(owned == text)
    println(owned)

    left = json.parse("{\"a\":[1,{\"b\":true}],\"z\":null}")?
    right = json.parse("{\"z\":null,\"a\":[1,{\"b\":true}]}")?
    different = json.parse("{\"a\":[1,{\"b\":false}],\"z\":null}")?
    println(left == right)
    println(left != different)
    parsed_plain = json.parse("{\"age\":1,\"name\":\"Ku\"}")?
    plain = { name: "Ku", age: 1 }
    println(parsed_plain == plain)
    println(plain["name"]?)

    dynamic = json.parse("[[1,2],[3]]")?
    typed = [[1, 2], [3]]
    println(dynamic == typed)
    println(typed == dynamic)
    println(dynamic != [[1, 2], [4]])
    println(typed.len())
    note = Note { text: "A" + "B" }
    println(integer == note)
    println(note != integer)
    println(note.text)
    return ok(null)
}
"#,
    )
    .expect("write KuValue equality source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("static bool ku_value_equal(KuValue left, KuValue right)"));
    assert!(c.contains("static bool ku_value_equal_typed_array_int("));
    assert!(c.contains("static bool ku_value_equal_typed_array_array_int("));
    assert!(c.contains("static bool ku_object_equal("));
    assert!(c.contains("ku_object_get(right, left->entries[index].key)"));
    assert!(
        !c.contains("static KuValue ku_v_typed_array_int("),
        "borrowed equality must not pull in the consuming typed-array bridge"
    );

    let Some(exe) = native_build(&dir, "main.ku", "kuvalueequal") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "true\ntrue\nfalse\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\nKu\ntrue\ntrue\ntrue\nKu\ntrue\ntrue\ntrue\n2\nfalse\ntrue\nAB\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_non_object_index_reports_type_unsupported() {
    let dir = unique_temp_dir("kuvalue-type-unsupported");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn main(): null! {
    value = json.parse("1")?
    try {
        found = value["age"]?
        println(found)
    } catch (err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    return ok(null)
}
"#,
    )
    .expect("write KuValue type error source");

    let Some(exe) = native_build(&dir, "main.ku", "kuvaluetype") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "object\ntype_unsupported\nexpected object value\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_function_equality_does_not_consume_closure() {
    let dir = unique_temp_dir("kuvalue-function-equality");
    fs::write(
        dir.join("main.ku"),
        r#"fn main(): null! {
    count = 40
    f = () => { count = count + 1  return count }
    obj = { handler: f.clone() }
    value = obj.get_or("handler", null)
    println(value == f)
    println(f == value)
    println(value != f)
    println(value == value)
    println(f())
    println(f())
    return ok(null)
}
"#,
    )
    .expect("write KuValue function equality source");

    let Some(exe) = native_build(&dir, "main.ku", "kuvaluefunctionequal") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "false\nfalse\ntrue\nfalse\n41\n42\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_array_roundtrip() {
    let dir = unique_temp_dir("json-arr");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    println(json.stringify(json.parse(\"[1,2,3]\")?)?)\n    println(json.stringify(json.parse(\"[{\\\"a\\\":1},{\\\"a\\\":2}]\")?)?)\n    println(json.stringify(json.parse(\"[]\")?)?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonarr") else {
        return;
    };

    // KuValue KU_ARRAY parse/stringify round-trip: scalars, object arrays, empty.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "[1,2,3]\n[{\"a\":1},{\"a\":2}]\n[]\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_try_parse_rejects_trailing_and_invalid_number_grammar() {
    let dir = unique_temp_dir("json-strict-grammar");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn Reject(text:str): null! {
    json.try_parse(text)?
    return ok(null)
}

fn ExpectReject(text:str): null! {
    rejected = false
    try {
        Reject(text)?
    } catch (err) {
        rejected = true
        if (err.domain != "json" || err.code != "parse_error") {
            panic("unexpected json error")
        }
    }
    if (!rejected) {
        panic("invalid json was accepted")
    }
    return ok(null)
}

fn main(): null! {
    ExpectReject("true false")?
    ExpectReject("nullx")?
    ExpectReject("[1]x")?
    ExpectReject("{\"a\":1}x")?
    ExpectReject("+1")?
    ExpectReject("01")?
    ExpectReject("-")?
    ExpectReject("1.")?
    ExpectReject("1e")?
    ExpectReject("1e+")?
    ExpectReject("9223372036854775808")?
    ExpectReject("1e400")?
    println("done")
    return ok(null)
}
"#,
    )
    .expect("write strict json grammar source");

    // This program deliberately uses only json.try_parse. It is a hard gate for
    // emitting KuValue + Result<KuValue> without relying on parse/stringify to
    // pull the runtime in accidentally.
    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("static KuResult_kuvalue ku_json_try_parse("));
    assert!(c.contains("unexpected trailing characters"));
    assert!(c.contains("ku_json_parse_number"));

    let Some(exe) = native_build(&dir, "main.ku", "jsonstrictgrammar") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "done\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_float_roundtrip_typed_array_and_non_finite_rejection() {
    let dir = unique_temp_dir("json-float");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn main(): null! {
    println(json.parse("1.5")?)
    println(json.stringify(json.parse("-0.25")?)?)
    println(json.stringify([1.5, 2.25])?)
    return ok(null)
}
"#,
    )
    .expect("write json float source");

    let Some(exe) = native_build(&dir, "main.ku", "jsonfloat") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1.5\n-0.25\n[1.5,2.25]\n");
    assert_eq!(code, Some(0));

    fs::write(
        dir.join("nonfinite.ku"),
        r#"import json from "std.json"

fn main(): null! {
    value = 1.0
    i = 0
    while (i < 1024) {
        value = value * 2.0
        i = i + 1
    }
    println(json.stringify(value)?)
    return ok(null)
}
"#,
    )
    .expect("write non-finite json source");
    let Some(nonfinite_exe) = native_build(&dir, "nonfinite.ku", "jsonnonfinite") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let mut command = Command::new(&nonfinite_exe);
    command.current_dir(&dir);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("non-finite JSON binary was not bounded: {error}"));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("json.stringify does not support non-finite float"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_float_stringify_matches_rust_shortest_fixed_decimal() {
    let dir = unique_temp_dir("json-float-shortest");
    let mut bits = vec![
        0.1f64.to_bits(),
        1e20f64.to_bits(),
        1e-20f64.to_bits(),
        (-0.0f64).to_bits(),
        0x3fefffffffffffff, // predecessor of 1.0
        0x3ff0000000000000, // 1.0
        0x3ff0000000000001, // successor of 1.0
        f64::MIN_POSITIVE.to_bits(),
        f64::MAX.to_bits(),
        0x0000000000000001, // minimum subnormal
        0x000fffffffffffff, // maximum subnormal
        0x0010000000000001, // successor of minimum normal
        0x43143ff3c1cb0959, // a long shortest representation
        0xc3143ff3c1cb0959,
    ];
    // Deterministic adjacent/exponent coverage. Non-finite patterns are skipped;
    // the formatter itself has a hard 17-attempt bound, so this cannot create an
    // unbounded native test even when a conversion regresses.
    let mut state = 0x6a09e667f3bcc909u64;
    for _ in 0..96 {
        state = state
            .wrapping_mul(0x5851f42d4c957f2d)
            .wrapping_add(0x14057b7ef767814f);
        if state & 0x7ff0000000000000 != 0x7ff0000000000000 {
            bits.push(state);
        }
    }
    let values = bits.into_iter().map(f64::from_bits).collect::<Vec<_>>();
    let input = values
        .iter()
        .map(|value| format!("{value:.17e}"))
        .collect::<Vec<_>>()
        .join(",");
    let expected = values
        .iter()
        .map(f64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let source = format!(
        r#"import json from "std.json"

fn main(): null! {{
    value = json.parse("[{input}]")?
    print(json.stringify(value)?)
    return ok(null)
}}
"#,
    );
    fs::write(dir.join("main.ku"), source).expect("write shortest float source");

    let Some(exe) = native_build(&dir, "main.ku", "jsonfloatshortest") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout, format!("[{expected}]"));
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_escapes_unicode_and_rejects_invalid_escapes() {
    let dir = unique_temp_dir("json-escapes");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn Reject(text:str): null! {
    json.try_parse(text)?
    return ok(null)
}

fn ExpectReject(text:str): null! {
    rejected = false
    try {
        Reject(text)?
    } catch (err) {
        rejected = err.domain == "json" && err.code == "parse_error"
    }
    if (!rejected) {
        panic("invalid json escape was accepted")
    }
    return ok(null)
}

fn main(): null! {
    text = "\"\\\"\\\\\\/\\b\\f\\n\\r\\t\\u4f60\""
    println(json.stringify(json.parse(text)?)?)
    println(json.stringify(json.parse("\"\\uD83D\\uDE00\"")?)?)
    ExpectReject("\"\\x\"")?
    ExpectReject("\"\\u12G4\"")?
    ExpectReject("\"\\uD800\"")?
    ExpectReject("\"\\uD83Dx\"")?
    ExpectReject("\"\\uD83D\\u0041\"")?
    ExpectReject("\"\\uDE00\"")?
    ExpectReject("\"unterminated")?
    return ok(null)
}
"#,
    )
    .expect("write json escape source");

    let Some(exe) = native_build(&dir, "main.ku", "jsonescapes") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "\"\\\"\\\\/\\u0008\\u000c\\n\\r\\t你\"\n\"😀\"\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_accepts_raw_del_and_c1_but_rejects_c0() {
    let dir = unique_temp_dir("json-raw-controls");
    fs::write(dir.join("legal.json"), [b'"', 0x7f, 0xc2, 0x85, b'"'])
        .expect("write legal raw JSON controls");
    fs::write(dir.join("illegal.json"), b"\"\n\"").expect("write illegal raw JSON control");
    let source = r#"import fs from "std.fs"
import json from "std.json"

fn main(): null! {
    legal = fs.read("legal.json")?
    print(json.stringify(json.parse(legal)?)?)
    rejected = false
    try {
        json.try_parse(fs.read("illegal.json")?)?
    } catch (err) {
        rejected = err.domain == "json" && err.code == "parse_error"
    }
    if (!rejected) {
        panic("raw C0 control was accepted")
    }
    return ok(null)
}
"#;
    fs::write(dir.join("main.ku"), source).expect("write raw JSON controls source");

    let Some(exe) = native_build(&dir, "main.ku", "jsonrawcontrols") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout, "\"\u{007f}\u{0085}\"");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_depth_matches_interpreter_boundary() {
    let dir = unique_temp_dir("json-depth");
    // The interpreter currently checks `depth > 32`, so 33 nested empty
    // containers are accepted and the 34th is rejected. Native intentionally
    // mirrors that existing behavior; changing it requires a two-runtime change.
    let allowed = format!("{}{}", "[".repeat(33), "]".repeat(33));
    let rejected = format!("{}{}", "[".repeat(34), "]".repeat(34));
    let source = format!(
        r#"import json from "std.json"

fn TooDeep(): null! {{
    json.try_parse("{rejected}")?
    return ok(null)
}}

fn main(): null! {{
    println(json.stringify(json.parse("{allowed}")?)?)
    caught = false
    try {{
        TooDeep()?
    }} catch (err) {{
        caught = err.domain == "json" && err.code == "parse_error"
    }}
    if (!caught) {{
        panic("deep json should fail")
    }}
    return ok(null)
}}
"#,
    );
    fs::write(dir.join("main.ku"), source).expect("write json depth source");

    let Some(exe) = native_build(&dir, "main.ku", "jsondepth") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), format!("{allowed}\n"));
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_stringify_sorts_object_keys_stably() {
    let dir = unique_temp_dir("json-sort");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn main(): null! {
    value = json.parse("{\"z\":0,\"é\":1,\"aa\":2,\"a\":3}")?
    println(json.stringify(value)?)
    return ok(null)
}
"#,
    )
    .expect("write json sort source");

    let Some(exe) = native_build(&dir, "main.ku", "jsonsort") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "{\"a\":3,\"aa\":2,\"z\":0,\"é\":1}\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_input_and_output_limits_are_hard_boundaries() {
    let dir = unique_temp_dir("json-limits");
    fs::write(dir.join("ok.txt"), vec![b'a'; 999_998]).expect("write boundary payload");
    fs::write(dir.join("too-large.txt"), vec![b'a'; 999_999]).expect("write oversized payload");
    fs::write(
        dir.join("main.ku"),
        r#"import fs from "std.fs"
import json from "std.json"

fn ParseTooLarge(): null! {
    text = "\"" + fs.read("too-large.txt")? + "\""
    json.try_parse(text)?
    return ok(null)
}

fn main(): null! {
    ok_text = "\"" + fs.read("ok.txt")? + "\""
    rendered = json.stringify(json.parse(ok_text)?)?
    println(rendered.len())
    caught = false
    try {
        ParseTooLarge()?
    } catch (err) {
        caught = err.domain == "json" && err.code == "parse_error"
    }
    if (!caught) {
        panic("oversized json input should fail")
    }
    return ok(null)
}
"#,
    )
    .expect("write json input limit source");

    let Some(exe) = native_build(&dir, "main.ku", "jsonlimits") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1000000\n");
    assert_eq!(code, Some(0));

    fs::write(
        dir.join("output-too-large.ku"),
        r#"import fs from "std.fs"
import json from "std.json"

fn main(): null! {
    println(json.stringify(fs.read("too-large.txt")?)?)
    return ok(null)
}
"#,
    )
    .expect("write json output limit source");
    let Some(output_exe) = native_build(&dir, "output-too-large.ku", "jsonoutputlimit") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let mut command = Command::new(&output_exe);
    command.current_dir(&dir);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("oversized JSON output binary was not bounded: {error}"));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("json.stringify output is too large"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_partial_parse_failures_drop_every_owned_layer() {
    let dir = unique_temp_dir("json-cleanup");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn ParseBad(): null! {
    json.try_parse("{\"a\":[\"owned\",{\"b\":\"owned\"}],\"c\":[1,2,]}")?
    return ok(null)
}

fn main(): null! {
    i = 0
    while (i < 2000) {
        caught = false
        try {
            ParseBad()?
        } catch (err) {
            caught = err.domain == "json" && err.code == "parse_error"
        }
        if (!caught) {
            panic("partial json should fail")
        }
        i = i + 1
    }
    println("done")
    return ok(null)
}
"#,
    )
    .expect("write json cleanup source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_json_buffer_drop(&buffer);"));
    assert!(c.contains("ku_value_array_drop(array);"));
    assert!(c.contains("ku_object_drop(object);"));
    assert!(c.contains("ku_value_drop(&value);"));

    let Some(exe) = native_build(&dir, "main.ku", "jsoncleanup") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "done\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_try_parse_container_oom_is_structured_and_cleans_partial_values() {
    let dir = unique_temp_dir("json-container-oom");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn Load(): null! {
    json.try_parse("{\"a\":[\"owned\"],\"b\":2,\"c\":3,\"d\":4,\"e\":5,\"f\":6}")?
    return ok(null)
}

fn main(): null! {
    try {
        Load()?
    } catch (err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    return ok(null)
}
"#,
    )
    .expect("write JSON container OOM source");

    let ordinary_c = native_emit_c(&dir, "main.ku");
    assert!(ordinary_c.contains("#define KU_NATIVE_TEST_OBJECT_OOM 0"));
    assert!(ordinary_c.contains("#define ku_object_malloc(site, bytes) malloc(bytes)"));
    assert!(ordinary_c.contains("ku_value_drop(&element);"));
    assert!(ordinary_c.contains("ku_string_drop(&key);"));
    assert!(ordinary_c.contains("ku_object_drop(object);"));

    let Some(exe) = native_build_with_object_oom_hook(&dir, "main.ku", "jsoncontaineroom") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let generated_c = fs::read_to_string(native_generated_c(&dir, "jsoncontaineroom"))
        .expect("read fault-enabled generated C");
    assert!(generated_c.contains("#define KU_NATIVE_TEST_OBJECT_OOM 1"));

    for (site, ordinal) in [
        ("object_header", 1),
        ("object_entries", 1),
        ("value_array_header", 1),
        ("value_array_grow", 1),
        ("object_rehash", 1),
    ] {
        let (stdout, stderr, code) = run_binary_with_object_oom(&exe, site, ordinal);
        assert_eq!(
            stdout.replace('\r', ""),
            "json\nout_of_memory\njson allocation failed\n",
            "unexpected stdout for fault site {site}"
        );
        assert!(stderr.is_empty(), "unexpected stderr for {site}: {stderr}");
        assert_eq!(code, Some(0), "fault site {site} must return Result error");
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_typed_array_oom_is_structured_result_and_cleans_owned_values() {
    let dir = unique_temp_dir("typed-array-result-oom");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn main(): null! {
    try {
        println(json.stringify(["a" + "b", "c" + "d"])? )
    } catch (err) {
        println(err.domain)
        println(err.code)
        println(err.message)
    }
    return ok(null)
}
"#,
    )
    .expect("write typed array Result OOM source");

    let Some(exe) = native_build_with_object_oom_hook(&dir, "main.ku", "typedarrayoom") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let generated_c = fs::read_to_string(native_generated_c(&dir, "typedarrayoom"))
        .expect("read typed array Result generated C");
    assert!(generated_c.contains("ku_json_stringify_typed_array_str"));
    assert!(generated_c.contains("ku_try_v_typed_array_str"));

    let (stdout, stderr, code) = run_binary_with_object_oom(&exe, "value_array_data", 1);
    assert_eq!(
        stdout.replace('\r', ""),
        "json\nout_of_memory\njson allocation failed\n"
    );
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_stringify_typed_primitive_arrays() {
    let dir = unique_temp_dir("json-typed-arrays");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    println(json.stringify([1, 2, 3])?)\n    println(json.stringify([true, false])?)\n    println(json.stringify([null, null])?)\n    first = json.parse(\"{\\\"x\\\":1}\")?\n    second = json.parse(\"[2,3]\")?\n    println(json.stringify([first, second])?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsontypedarrays") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "[1,2,3]\n[true,false]\n[null,null]\n[{\"x\":1},[2,3]]\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_stringify_nested_user_struct_and_arrays() {
    let dir = unique_temp_dir("json-nested-user-struct");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

struct Leaf {
    name: str
    scores: [int]
}

struct Branch {
    leaves: [Leaf]
    primary: Leaf
    title: str
}

fn main(): null! {
    first = Leaf { name: "le" + "af", scores: [1, 2] }
    second = Leaf { name: "twig" + "", scores: [3] }
    primary = Leaf { name: "main" + "", scores: [0] }
    root = Branch { leaves: [first, second], primary: primary, title: "root" + "" }
    println(json.stringify(root)?)
    return ok(null)
}
"#,
    )
    .expect("write nested struct JSON source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("static bool ku_json_write_typed_struct_Leaf("));
    assert!(c.contains("static bool ku_json_write_typed_array_struct_Leaf("));
    assert!(c.contains("static bool ku_json_write_typed_struct_Branch("));
    assert!(c.contains("static KuResult_str ku_json_stringify_typed_struct_Branch("));
    assert!(
        !c.contains("ku_try_v_typed_array_struct_Leaf"),
        "struct arrays must use the typed JSON writer, not unsupported KuValue boxing"
    );

    let Some(exe) = native_build(&dir, "main.ku", "jsonstruct") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "{\"leaves\":[{\"name\":\"leaf\",\"scores\":[1,2]},{\"name\":\"twig\",\"scores\":[3]}],\"primary\":{\"name\":\"main\",\"scores\":[0]},\"title\":\"root\"}\n"
    );
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_stringify_unsupported_owned_values_are_catchable_and_dropped() {
    let dir = unique_temp_dir("json-unsupported-owned");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

enum State { Ready(label: str) }

fn Load(): str! {
    return ok("owned" + " result")
}

fn main(): null! {
    try {
        json.stringify(Load())?
        panic("Result must not stringify")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    state = State.Ready("owned" + " enum")
    try {
        json.stringify(state)?
        panic("enum must not stringify")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }

    captured = "owned" + " closure"
    operation: fn(): str = () => { return captured.clone() }
    try {
        json.stringify(operation)?
        panic("closure must not stringify")
    } catch (err) {
        println(err.domain + "/" + err.code)
    }
    return ok(null)
}
"#,
    )
    .expect("write unsupported JSON source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_json_stringify_typed_result_str"));
    assert!(c.contains("ku_json_stringify_typed_enum_State"));
    assert!(c.contains("ku_result_drop_str(&value);"));
    assert!(c.contains("ku_drop_enum_State(&value);"));
    assert!(c.contains("json.stringify does not support result"));
    assert!(c.contains("json.stringify does not support enum"));

    let Some(exe) = native_build(&dir, "main.ku", "jsonunsupported") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "json/stringify_error\njson/stringify_error\njson/stringify_error\n"
    );
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_object_and_json_stringify_nested_typed_arrays() {
    let dir = unique_temp_dir("json-nested-typed-arrays");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    matrix = { values: [[1, 2], [3, 4]] }\n    println(json.stringify(matrix)?)\n    payload = { items: [{ values: [1, 2] }, { values: [3, 4] }] }\n    println(json.stringify(payload)?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonnestedtyped") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "{\"values\":[[1,2],[3,4]]}\n{\"items\":[{\"values\":[1,2]},{\"values\":[3,4]}]}\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_stringify_owned_string_array_clone_drops_once() {
    let dir = unique_temp_dir("json-owned-string-array");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    left = \"a\" + \"b\"\n    right = \"c\" + \"d\"\n    values = [left, right]\n    copy = values.clone()\n    println(json.stringify(values)?)\n    println(json.stringify(copy)?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonownedstrings") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "[\"ab\",\"cd\"]\n[\"ab\",\"cd\"]\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_kuvalue_array_bridge_is_emitted_only_for_wrapped_types() {
    let dir = unique_temp_dir("json-array-helper-gating");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    unboxed = [true, false]\n    println(unboxed.len())\n    println(json.stringify([1, 2])?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("static KuValue ku_v_typed_array_int("));
    assert!(!c.contains("static KuValue ku_v_typed_array_bool("));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_json_array_element_access() {
    let dir = unique_temp_dir("json-idx");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    a = json.parse(\"[10,20,30]\")?\n    println(a[0]?.as_int()?)\n    println(a[2]?.as_int()?)\n    obj = json.parse(\"{\\\"items\\\":[7,8,9]}\")?\n    items = obj[\"items\"]?\n    println(items[1]?.as_int()?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "jsonidx") else {
        return;
    };

    // KuValue array int-index `arr[i]?` -> element, including a nested
    // obj["items"]?[i]? read; native matches the interpreter.
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "10\n30\n8\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_try_catch_mixed_result_chain() {
    // A try block whose `?` operators unwrap DIFFERENT Result types in one
    // statement — `a[9]?` (KuValue) then `.as_int()?` (int) — share a single
    // KuError-typed error slot, so the out-of-bounds error reaches catch as
    // `index_out_of_bounds`. This regression-guards the fix where the slot
    // was pinned to the first `?`'s Result type and a later differently-typed
    // `?` failed to compile.
    let dir = unique_temp_dir("try-mixed");
    fs::write(
        dir.join("main.ku"),
        "import json from \"std.json\"\n\nfn main(): null! {\n    a = json.parse(\"[10,20]\")?\n    try {\n        x = a[9]?.as_int()?\n        println(x)\n    } catch (e) {\n        println(e.code)\n    }\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "trymixed") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "index_out_of_bounds\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_literal_no_capture() {
    // Stage 6a: a no-capture closure literal lowers to a lifted `__ku_closure_N`
    // C function reached through an indirect `{invoke, env=NULL}` call.
    let dir = unique_temp_dir("closure-lit");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    f = () => { return 42 }\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closlit") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

/// try/catch/finally return behavior, pinned against the interpreter. Stage 8e
/// added return-type inference for functions declared WITHOUT a `: T` annotation;
/// this locks in that it did not disturb any of the existing paths. Annotated
/// functions skip the inference pass entirely, and every case below is annotated.
///
/// Covers: try completing normally, `return` inside try, `?` returning early out
/// of try into catch, `?` succeeding inside try, `return` inside catch, finally
/// completing normally, and finally running without changing the enclosing
/// function's return type when try returned.
const TRY_FINALLY_SOURCE: &str = concat!(
    "fn boom(): int! {\n    fail { domain: \"t\", code: \"b\", message: \"m\" }\n}\n\n",
    "fn ok_src(): int! {\n    return ok(7)\n}\n\n",
    "fn a(): int {\n    x = 0\n    try {\n        x = 1\n    } catch (e) {\n        x = 2\n    }\n    return x\n}\n\n",
    "fn b(): int {\n    try {\n        return 10\n    } catch (e) {\n    }\n    return 99\n}\n\n",
    "fn c(): int {\n    try {\n        v = boom()?\n        return v\n    } catch (e) {\n        return 30\n    }\n    return 31\n}\n\n",
    "fn d(): int {\n    try {\n        v = ok_src()?\n        return v + 1\n    } catch (e) {\n        return 40\n    }\n    return 41\n}\n\n",
    "fn e_fin(): int {\n    r = 0\n    try {\n        r = 50\n    } finally {\n        println(\"fin-e\")\n    }\n    return r\n}\n\n",
    "fn f_fin(): int {\n    try {\n        return 60\n    } finally {\n        println(\"fin-f\")\n    }\n    return 61\n}\n\n",
    "fn main(): null! {\n    println(a())\n    println(b())\n    println(c())\n    println(d())\n    println(e_fin())\n    println(f_fin())\n    return ok(null)\n}\n",
);

#[test]
fn native_try_finally_return_paths_match_interpreter() {
    let dir = unique_temp_dir("try-finally");
    fs::write(dir.join("main.ku"), TRY_FINALLY_SOURCE).expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "tryfin") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // Verified identical under `ku run` on the interpreter.
    assert_eq!(
        stdout.replace('\r', ""),
        "1\n10\n30\n8\nfin-e\n50\nfin-f\n60\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_unannotated_function_with_try_used_as_value() {
    // The one shape Stage 8e's inference pass actually touches: no return
    // annotation AND a try in the body. The `return` outside the try gives the
    // pass a concrete type, so the function is usable as a value; before, it
    // lowered as `void` and could not be emitted as a closure at all.
    let dir = unique_temp_dir("try-noann");
    fs::write(
        dir.join("main.ku"),
        "fn t() {\n    try {\n        return 5\n    } catch (e) {\n    }\n    return 6\n}\n\nfn main(): null! {\n    f = t\n    println(f())\n    println(t())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "trynoann") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "5\n5\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_unannotated_return_type_is_inferred_from_body() {
    // Stage 8e: a top-level function with no `: T` return annotation gets its
    // return type inferred from the body, exactly like the checker does. Two
    // things are pinned here:
    //   * The function is usable as a *value* at all -- an unannotated return used
    //     to lower as `void`, so `f = pick` produced a `Closure { ret: void }` the
    //     C backend could not emit.
    //   * `null` is the identity element when folding the body's returns (the
    //     checker's merge_return_types), so a body that returns `null` on one path
    //     and `int` on another infers `int`. Taking the first return instead would
    //     infer `null` here and disagree with the checker.
    let dir = unique_temp_dir("infer-return");
    fs::write(
        dir.join("main.ku"),
        "fn pick(flag: bool) {\n    if (flag) {\n        return null\n    }\n    return 5\n}\n\nfn main(): null! {\n    f = pick\n    println(f(false))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "inferret") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "5\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

/// Owned-field move coverage for `c_move_place` (Stage 8e). Reading an owned field
/// in value position moves it, so the source must be cleared — otherwise the
/// owning struct's own drop frees the buffer the moved value now holds. Each
/// function below routes an owned field out through a different control-flow exit
/// (plain return, early return, Result ok, catch, finally) so that every cleanup
/// path is exercised in one binary; a missed clear shows up as a double free.
// c_move_place is the backend safety net for owned values read in value position.
// The checker's consume_expr already forbids *directly* moving a plain owned field
// or indexed element (it demands `.clone()`), so this pins the shapes that DO reach
// codegen as a move: a `.clone()`d field (its own fresh allocation), and -- the
// shape that slipped past the checker into a real double free -- an HTTP handler
// returning `req.body`, covered end-to-end in native_http_test.rs. Here we drive
// the language-level paths that are legal, through every control-flow exit, so a
// missing clear on any of them double-frees instead of exiting 0.
const FIELD_MOVE_SOURCE: &str = concat!(
    "struct Holder {\n    name: str\n}\n\n",
    // an owned COPY (clone) returned out of the function: the source struct still
    // owns its field and must drop it exactly once when the function returns.
    "fn take_clone(h: Holder): str {\n    return h.name.clone()\n}\n\n",
    // read-only use through concatenation must NOT be treated as a move: the field
    // is still owned by the struct and must drop exactly once.
    "fn peek(h: Holder): str {\n    return \"[\" + h.name + \"]\"\n}\n\n",
    // early return before touching the field still drops the struct exactly once.
    "fn early(h: Holder, skip: bool): str {\n    if (skip) {\n        return \"skipped\"\n    }\n    return h.name.clone()\n}\n\n",
    // clone moved out through a Result payload.
    "fn take_result(h: Holder): str! {\n    return ok(h.name.clone())\n}\n\n",
    // clone moved out from inside a try, with a finally running afterwards.
    "fn take_try(h: Holder): str {\n    try {\n        return h.name.clone()\n    } finally {\n        println(\"fin\")\n    }\n    return \"unreachable\"\n}\n\n",
    // a whole owned local (a struct) moved out of the function as a value.
    "fn passthrough(h: Holder): Holder {\n    return h\n}\n\n",
    "fn make(n: str): Holder {\n    return Holder{ name: n }\n}\n\n",
    "fn main(): null! {\n",
    "    println(take_clone(make(\"alpha\")))\n",
    "    println(peek(make(\"gamma\")))\n",
    "    println(early(make(\"delta\"), true))\n",
    "    println(early(make(\"epsilon\"), false))\n",
    "    println(take_result(make(\"zeta\"))?)\n",
    "    println(take_try(make(\"eta\")))\n",
    "    println(passthrough(make(\"theta\")).name)\n",
    "    return ok(null)\n}\n",
);

#[test]
fn native_nested_field_move_preserves_siblings() {
    // Moving a nested field (`c.user.name`) must move only that leaf, not the whole
    // intermediate struct — the sibling `c.user.email` and `c.host` must survive
    // with their values intact.
    let dir = unique_temp_dir("nested-move");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct User { name: str, email: str }\n",
            "struct Config { user: User, host: str }\n\n",
            "fn main() {\n",
            "  dom = \"example\"\n",
            "  c = Config { user: User { name: \"alice\", email: dom + \".com\" }, host: \"localhost\" }\n",
            "  n = c.user.name\n",
            "  println(n)\n",
            "  println(c.user.email)\n",
            "  println(c.host)\n}\n",
        ),
    )
    .expect("write main.ku");
    let Some(exe) = native_build(&dir, "main.ku", "nestedmove") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "alice\nexample.com\nlocalhost\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_match_binding_used_more_than_once_is_not_re_moved() {
    // A match-bound owned payload must be extracted once; using the binding twice
    // must read the same value, not re-move an already-cleared enum slot.
    let dir = unique_temp_dir("match-multi");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Payload { name: str, note: str }\n",
            "enum Box { Full(p: Payload)  Empty }\n",
            "fn build(a: str, b: str): Payload { return Payload { name: a + \"-tag\", note: b + \"-tag\" } }\n\n",
            "fn main() {\n",
            "  b = Box.Full(build(\"alice\", \"hello\"))\n",
            "  text = match b {\n    Box.Full(p) => p.name + \":\" + p.note\n    Box.Empty => \"empty\"\n  }\n",
            "  println(text)\n}\n",
        ),
    )
    .expect("write main.ku");
    let Some(exe) = native_build(&dir, "main.ku", "matchmulti") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "alice-tag:hello-tag\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_void_returning_function_call_compiles() {
    // A call to a user function with no return type must not emit `void t0 = f()`.
    let dir = unique_temp_dir("void-call");
    fs::write(
        dir.join("main.ku"),
        "fn sink(v: str) { println(v) }\nfn main() {\n    sink(\"literal\")\n    println(\"after\")\n}\n",
    )
    .expect("write main.ku");
    let Some(exe) = native_build(&dir, "main.ku", "voidcall") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "literal\nafter\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_call_depth_counter_no_drift_on_void_fallthrough() {
    // A void function that reaches the closing brace takes the emitter's implicit
    // fallthrough return path. That path must release its call-depth slot just like
    // an explicit `return`; otherwise many sequential (non-recursive) calls are
    // mistaken for deep recursion and abort after KU_MAX_CALL_DEPTH invocations.
    let dir = unique_temp_dir("void-depth-drift");
    fs::write(
        dir.join("main.ku"),
        "fn ping() {}\n\nfn main() {\n    i = 0\n    while (i < 1500) {\n        ping()\n        i = i + 1\n    }\n    println(\"ok\")\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "voiddepthdrift") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "ok\n");
    assert_eq!(
        code,
        Some(0),
        "sequential void fallthrough calls must not accumulate call depth"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_clone_is_deep_not_aliasing() {
    // `.clone()` of a struct with an owned string field must DEEP-clone the field,
    // not shallow-copy it. A shallow copy aliases the buffer, so moving the field
    // out of both the original and the clone frees it twice.
    let dir = unique_temp_dir("struct-clone");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U { name: str }\n\n",
            "fn main() {\n",
            "  base = \"hel\"\n",
            "  u = U{ name: base + \"lo\" }\n",
            "  v = u.clone()\n",
            "  a = u.name\n",
            "  b = v.name\n",
            "  println(a)\n",
            "  println(b)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structclone") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hello\nhello\n");
    assert_eq!(code, Some(0)); // a double free would abort with 0xC0000374
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_literal_consumes_its_owned_field_source() {
    // Building a struct from an owned local must MOVE the value into the field
    // (clearing the source), not shallow-copy it — otherwise the source local and
    // the field both own the same buffer and both free it.
    let dir = unique_temp_dir("struct-literal");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U { name: str }\n\n",
            "fn main() {\n",
            "  base = \"hel\"\n",
            "  s = base + \"lo\"\n",
            "  u = U{ name: s }\n",
            "  a = u.name\n",
            "  println(a)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structlit") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hello\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_owned_field_does_not_leak_on_drop() {
    // A struct's owned fields must be deep-dropped when it goes out of scope; a
    // no-op drop would leak them. This just pins that such a program runs and
    // exits cleanly (ASan/CRT leak runs are done separately).
    let dir = unique_temp_dir("struct-drop");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U { name: str, tag: str }\n\n",
            "fn build(n: str): U {\n    return U{ name: n, tag: \"t\".clone() }\n}\n\n",
            "fn main() {\n",
            "  u = build(\"kept\".clone())\n",
            "  println(u.name)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structdrop") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "kept\n");
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_enum_payload_is_moved_in_not_double_freed() {
    // The enum literal takes ownership of its payload argument. The construction
    // used to shallow-copy the argument (leaving the source binding/temp still
    // owning the same heap buffer), so extracting the payload via `match` and then
    // dropping both the extracted value and the un-cleared source double-freed the
    // string. The fix moves-and-clears the argument into the payload.
    let dir = unique_temp_dir("enum-payload");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "enum Box {\n  Full(value: str)\n  Empty\n}\n\n",
            "fn main() {\n",
            "  n = \"world\"\n",
            "  b = Box.Full(\"hello \" + n)\n",
            "  msg = match b {\n    Box.Full(value) => value\n    Box.Empty => \"empty\"\n  }\n",
            "  println(msg)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "enumpayload") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hello world\n");
    // A double free aborts with STATUS_HEAP_CORRUPTION instead of exiting 0.
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_partial_field_move_keeps_sibling_and_drops_safely() {
    // The checker now allows moving a single owned struct field out (`n = u.name`)
    // while keeping the siblings usable. Native must execute that safely: the move
    // clears `u.name`, so when `u` is later dropped only the un-moved fields are
    // freed (no double free of the moved-out string). This is a real field MOVE,
    // not a `.clone()`.
    let dir = unique_temp_dir("partial-move");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct U {\n    name: str\n    tag: str\n}\n\n",
            "fn make(): U {\n    return U{ name: \"kept\".clone(), tag: \"also\".clone() }\n}\n\n",
            "fn main(): null! {\n",
            "    u = make()\n",
            "    n = u.name\n",     // move the name field out
            "    println(n)\n",     // moved value still owned here
            "    println(u.tag)\n", // sibling still usable
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "partialmove") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "kept\nalso\n");
    // A double free of the moved-out field aborts (STATUS_HEAP_CORRUPTION) rather
    // than exiting 0, so the exit code is the real assertion.
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_primitive_array_fields() {
    // A struct may hold `[int]`/`[bool]`/`[str]` fields: the primitive array ABI is
    // emitted before the struct layout so it embeds by value, and the struct's deep
    // clone/drop recurses into the array (verified: clone is independent, no leak or
    // double free — the exit code is the real assertion for the cloned/dropped run).
    let dir = unique_temp_dir("struct-array-field");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Rec { tags: [str], flags: [bool], nums: [int] }\n",
            "fn describe(r: Rec): int { return r.nums.len() + r.tags.len() }\n",
            "fn main(): null! {\n",
            "    r = Rec { tags: [\"a\", \"b\"], flags: [true, false], nums: [1, 2, 3, 4] }\n",
            "    println(r.tags[0])\n",       // a
            "    println(str(r.flags[1]))\n", // false
            "    println(r.nums.len())\n",    // 4
            "    c = r.clone()\n",            // deep clone of the array fields
            "    println(c.nums[3])\n",       // 4
            "    println(describe(c))\n",     // 4 + 2 = 6
            "    println(r.tags[1])\n",       // b — original still intact after clone
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structarr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "a\nfalse\n4\n4\n6\nb\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_array_of_struct_field() {
    // A struct may hold an array of another struct (`[Worker]`), including a nested
    // array field on the element (`tags: [str]`). The layered emission resolves the
    // struct↔array cycle (forward-declared tags + array typedefs before the struct
    // bodies, forward-declared array helpers), and deep clone/drop recurse through
    // both levels — no leak or double free (exit code is the assertion).
    let dir = unique_temp_dir("struct-array-of-struct");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Worker { id: int, tags: [str] }\n",
            "struct Team { members: [Worker], tag: str }\n",
            "fn main(): null! {\n",
            "    t = Team { members: [ Worker{id: 7, tags: [\"x\", \"y\"]}, Worker{id: 9, tags: [\"z\"]} ], tag: \"T\" }\n",
            "    println(t.members.len())\n",         // 2
            "    println(t.members[0].id)\n",         // 7
            "    println(t.members[0].tags[1])\n",    // y
            "    c = t.clone()\n",                     // deep clone through both levels
            "    println(c.members[1].tags[0])\n",    // z
            "    println(t.members[1].id)\n",         // 9 — original intact
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "arrofstruct") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "2\n7\ny\nz\n9\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_template_string_interpolation_matches_interpreter() {
    // Backtick templates must be interpolated in native, not emitted as a literal
    // with `{placeholders}`. Each `{expr}` becomes str(expr); `\{`/`\}` are literal
    // braces.
    let dir = unique_temp_dir("template-string");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    name = \"Ku\"\n",
            "    n = 30\n",
            "    println(`Hello {name} {n}`)\n",
            "    println(`sum={n + n} done`)\n",
            "    println(`brace \\{ x \\}`)\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "template") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "Hello Ku 30\nsum=60 done\nbrace { x }\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_string_literal_with_non_ascii_bytes_is_not_corrupted() {
    // A string literal carrying a non-printable byte (U+00A0 NBSP) must survive to
    // the C source intact. Rust's Debug `\u{a0}` escape is invalid C and MSVC would
    // mangle it to the ASCII text `u{a0}`, corrupting len/contains/println.
    let dir = unique_temp_dir("nonascii-literal");
    // "x" + U+00A0 (0xC2 0xA0) + "y"
    let src = "fn main() {\n    s = \"x\u{a0}y\"\n    println(s.len())\n    println(str(s.contains(\"u\")))\n}\n";
    fs::write(dir.join("main.ku"), src).expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "nbsp") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    // 3 codepoints (x, NBSP, y); contains("u") is false — not the mangled "u{a0}".
    assert_eq!(stdout.replace('\r', ""), "3\nfalse\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_pushing_an_owned_literal_does_not_leak() {
    // array.push clones its value into the new array (the source stays usable, like
    // the interpreter). A pushed fresh struct literal used to never be dropped,
    // leaking its owned fields; it must now be materialized and freed. The run
    // completing with the right output (and, under a leak checker, zero leaks) is the
    // assertion — here we at least confirm it runs correctly and the source is intact.
    let dir = unique_temp_dir("push-owned-literal");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct W { id: int, tags: [str] }\n",
            "fn main(): null! {\n",
            "    xs = [ W{ id: 0, tags: [\"seed\"] } ]\n",
            "    i = 0\n",
            "    while (i < 20) {\n",
            "        ys = xs.clone()\n",
            "        r = ys.push(W{ id: i, tags: [\"a\" + \"x\", \"b\" + \"y\"] })\n",
            "        i = i + r.len() - 1\n",
            "    }\n",
            "    println(i)\n",
            "    println(xs.len())\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "pushowned") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "20\n1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_struct_with_enum_field_topological() {
    // A struct may hold an enum field, and the enum may carry a struct payload — a
    // struct→enum→struct value-embedding chain. The unified topological layout pass
    // emits Point, then Shape, then Figure so every by-value type is complete before
    // its user. Deep clone/drop recurse through the enum payload; no leak/double free.
    let dir = unique_temp_dir("struct-enum-field");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Point { x: int, y: int }\n",
            "enum Shape { Dot, Circle(p: Point), Tag(s: str) }\n",
            "struct Figure { shape: Shape, name: str }\n",
            "fn main(): null! {\n",
            "    f = Figure { shape: Shape.Tag(\"hi\" + \"!\"), name: \"fig\" }\n",
            "    c = f.clone()\n",
            "    match c.shape { Shape.Tag(s) => println(s)  Shape.Dot => println(\"d\")  Shape.Circle(p) => println(p.x) }\n",
            "    println(f.name)\n",
            "    g = Figure { shape: Shape.Circle(Point{x: 4, y: 9}), name: \"g\" }\n",
            "    match g.shape { Shape.Circle(p) => println(p.y)  Shape.Dot => println(\"d\")  Shape.Tag(s) => println(s) }\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "structenum") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hi!\nfig\n9\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_string_methods_match_interpreter() {
    // len counts Unicode scalar values (café = 4), contains/starts_with/ends_with
    // are byte substring tests (empty needle is always true), replace is a
    // non-overlapping all-occurrences swap, and slice is char-indexed and returns
    // a Result. byte_len counts UTF-8 bytes without changing len or consuming its
    // input; its edge cases share this executable to avoid another C build.
    let dir = unique_temp_dir("string-methods");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn VerifyByteLength(text: str, bytes: int, scalars: int) {\n",
            "    method_bytes:int = text.byte_len()\n",
            "    module_bytes:int = string.byte_len(text)\n",
            "    if (method_bytes != bytes || module_bytes != bytes) panic(\"byte_len must count UTF-8 bytes\")\n",
            "    if (text.len() != scalars || string.len(text) != scalars || len(text) != scalars) panic(\"len must still count Unicode scalars\")\n",
            "    if (text.byte_len() != bytes || string.byte_len(text) != bytes) panic(\"byte_len must borrow its receiver\")\n",
            "}\n",
            "fn main(): null! {\n",
            "    a = \"hello world\"\n",
            "    println(a.len())\n",                      // 11
            "    println(str(a.contains(\"world\")))\n",   // true
            "    println(str(a.contains(\"\")))\n",        // true
            "    println(str(a.starts_with(\"hell\")))\n", // true
            "    println(str(a.ends_with(\"rld\")))\n",    // true
            "    println(a.replace(\"o\", \"0\"))\n",      // hell0 w0rld
            "    println(a.replace(\"\", \"-\"))\n",       // -h-e-l-l-o- -w-o-r-l-d-
            "    b = \"café\"\n",
            "    println(b.len())\n", // 4
            "    owned = \"A\" + \"界😀\"\n",
            "    chars:[str] = owned.chars()\n",
            "    println(chars.len())\n",
            "    if (len(chars) != 3 || chars[1] != \"界\") panic(\"len must borrow arrays\")\n",
            "    println(chars[0])\n",
            "    println(chars[1])\n",
            "    println(chars[2])\n",
            "    println(owned)\n",
            "    global_chars:[str] = string.chars(owned)\n",
            "    println(global_chars[1])\n",
            "    println(owned)\n",
            "    empty_chars:[str] = \"\".chars()\n",
            "    println(empty_chars.len())\n",
            "    println(a.slice(0, 5)?)\n", // hello
            "    println(b.slice(0, 4)?)\n", // café
            "    println(b)\n",              // receiver is borrowed
            "    VerifyByteLength(\"\", 0, 0)\n",
            "    VerifyByteLength(\"ASCII\", 5, 5)\n",
            "    VerifyByteLength(\"界\", 3, 1)\n",
            "    VerifyByteLength(\"😀\", 4, 1)\n",
            "    VerifyByteLength(\"e\u{301}\", 3, 2)\n",
            // Embed a real NUL in the Ku source; byte_len must not use strlen.
            "    VerifyByteLength(\"A\0界😀\", 9, 4)\n",
            "    if (owned.byte_len() != 8 || string.byte_len(owned) != 8 || owned.len() != 3) panic(\"owned string byte length is wrong\")\n",
            "    copy = owned.clone()\n",
            "    owned += \"!\"\n",
            "    if (copy.byte_len() != 8 || string.byte_len(copy) != 8 || copy != \"A界😀\") panic(\"byte_len consumed or changed an owned clone\")\n",
            "    moved = copy\n",
            "    VerifyByteLength(moved, 8, 3)\n",
            "    VerifyByteLength(owned, 9, 4)\n",
            "    println(\"byte-len-ok\")\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("static KuArray_str ku_string_chars(KuString s)"));
    assert!(c.contains("result.data[index] = ku_string_alloc(len)"));

    let Some(exe) = native_build(&dir, "main.ku", "strmethods") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "11\ntrue\ntrue\ntrue\ntrue\nhell0 w0rld\n-h-e-l-l-o- -w-o-r-l-d-\n4\n3\nA\n界\n😀\nA界😀\n界\nA界😀\n0\nhello\ncafé\ncafé\nbyte-len-ok\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_string_slice_out_of_bounds_returns_matching_error() {
    // The recoverable slice error must carry the same domain/code/message as the
    // interpreter so a caught error reads identically.
    let dir = unique_temp_dir("string-slice-err");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main() {\n",
            "    try {\n",
            "        r = \"hello\".slice(0, 100)?\n",
            "        println(r)\n",
            "    } catch (e) {\n",
            "        println(e.domain + \"/\" + e.code + \"/\" + e.message)\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "sliceerr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "string/slice_error/string.slice end 100 out of bounds for length 5\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_str_builtin_matches_interpreter_for_primitives() {
    // `str(x)` mirrors the interpreter's `value.to_string()`: int in decimal,
    // bool as true/false, a string identity (borrowed, so the source stays live),
    // and it composes with `+` so an int can be built into a larger string — the
    // gap that blocked the acceptance tool.
    let dir = unique_temp_dir("str-builtin");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    n = 42\n",
            "    println(str(n))\n",      // 42
            "    println(str(0 - 7))\n",  // -7
            "    println(str(true))\n",   // true
            "    println(str(false))\n",  // false
            "    println(str(\"hi\"))\n", // hi
            "    line = \"age=\" + str(n) + \"!\"\n",
            "    println(line)\n", // age=42!
            "    println(n)\n",    // 42 — str(n) did not consume n
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "strbuiltin") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "42\n-7\ntrue\nfalse\nhi\nage=42!\n42\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_non_consuming_field_read_leaves_the_field_intact() {
    // Regression (R3): a value-position field read used to lower as move-and-clear,
    // so `println(u.name)` emptied the field and the second read printed nothing,
    // and `u.name.clone()` cleared the source it was supposed to copy. A read must
    // borrow: native output must match the interpreter (both print the value twice).
    let dir = unique_temp_dir("field-read-borrow");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Inner { tag: str }\n",
            "struct Outer { inner: Inner, label: str }\n",
            "fn main(): null! {\n",
            "    o = Outer{ inner: Inner{ tag: \"t\" + \"ag\" }, label: \"L\" + \"bl\" }\n",
            "    println(o.inner.tag)\n", // read a nested field...
            "    println(o.inner.tag)\n", // ...twice; the second must still work
            "    c = o.label.clone()\n",  // clone must not clear the source
            "    println(c)\n",
            "    println(o.label)\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fieldread") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "tag\ntag\nLbl\nLbl\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_reboxed_each_iteration_releases_prior_cells() {
    // Regression (R8): a captured local re-boxed each loop iteration was CellNew'd
    // over the previous box without releasing it, leaking a cell (and its captured
    // string) per iteration — CRT reported 398 leaked blocks over 200 loops, now 0.
    // The fix releases the prior cell before overwriting; over-releasing would
    // double-free and abort, so a clean exit 0 across many iterations is the guard.
    let dir = unique_temp_dir("closure-loop");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "fn main(): null! {\n",
            "    i = 0\n",
            "    while (i < 50) {\n",
            "        u = \"row\" + \"x\"\n",
            "        g = () => { return u + \"?\" }\n",
            "        println(g())\n",
            "        i = i + 1\n",
            "    }\n",
            "    println(\"end\")\n",
            "    return ok(null)\n}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closureloop") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    let normalized = stdout.replace('\r', "");
    let lines: Vec<&str> = normalized.lines().collect();
    assert_eq!(lines.len(), 51, "50 loop lines + end");
    assert!(lines.iter().take(50).all(|l| *l == "rowx?"));
    assert_eq!(lines[50], "end");
    assert_eq!(code, Some(0), "re-boxing must not double-free");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_field_move_across_every_exit_path() {
    let dir = unique_temp_dir("field-move-paths");
    fs::write(dir.join("main.ku"), FIELD_MOVE_SOURCE).expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "movepaths") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "alpha\n[gamma]\nskipped\nepsilon\nzeta\nfin\neta\ntheta\n"
    );
    // A missed clear double-frees and aborts (STATUS_HEAP_CORRUPTION) instead of
    // exiting 0, so the exit code is the real assertion here.
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_struct_field_moved_into_a_value_is_not_double_freed() {
    // Stage 8e: reading an owned field in value position MOVES it, so the source
    // field must be cleared. It used to be copied, leaving the struct's own drop
    // to free the same buffer the moved value now owned -- a double free that
    // corrupted the heap (this is the `http.text(req.body)` handler shape from
    // cli_v001, which no native test had ever reached).
    let dir = unique_temp_dir("field-move");
    fs::write(
        dir.join("main.ku"),
        "struct Holder {\n    name: str\n}\n\nfn take(h: Holder): str {\n    return h.name\n}\n\nfn main(): null! {\n    h = Holder{ name: \"kept\" }\n    println(take(h))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fieldmove") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "kept\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_top_level_function_value() {
    // Stage 6a: a top-level function used as a value lowers to a `{name__thunk,
    // NULL}` closure and is invoked indirectly, matching the interpreter.
    let dir = unique_temp_dir("fn-value");
    fs::write(
        dir.join("main.ku"),
        "fn add(x: int): int {\n    return x + 1\n}\n\nfn main(): null! {\n    g = add\n    println(g(3))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fnvalue") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "4\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_function_value_typed_binding_multi_call() {
    // Stage 6a: a typed function binding `f: fn(): int = Answer` lowers the
    // `fn(): int` annotation to the same closure type as the value, and calling
    // it twice does not consume it.
    let dir = unique_temp_dir("fn-typed");
    fs::write(
        dir.join("main.ku"),
        "fn Answer(): int {\n    return 42\n}\n\nfn main(): null! {\n    f: fn(): int = Answer\n    println(f())\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fntyped") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_function_value_as_parameter() {
    // Stage 6a: a function value flows through a `fn(int,int): int` parameter,
    // is invoked indirectly inside the callee, and stays usable after being
    // passed (no-capture closures are copied by value, env=NULL).
    let dir = unique_temp_dir("fn-param");
    fs::write(
        dir.join("main.ku"),
        "fn Add(a: int, b: int): int {\n    return a + b\n}\n\nfn Apply(op: fn(int, int): int, a: int, b: int): int {\n    return op(a, b)\n}\n\nfn main(): null! {\n    op: fn(int, int): int = Add\n    println(op(1, 2))\n    println(Apply(op, 3, 4))\n    println(op(5, 6))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "fnparam") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n7\n11\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_shared_cell() {
    // Stage 6b: a captured Copy local is boxed into a ref-counted cell shared by
    // the closure and the enclosing scope; the closure mutates it and the outer
    // scope observes the change (counter -> 1, 2; outer count == 2).
    let dir = unique_temp_dir("cap-cell");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    count = 0\n    inc = () => { count = count + 1  return count }\n    println(inc())\n    println(inc())\n    println(count)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capcell") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_sees_outer_mutation() {
    // Stage 6b: capture is by reference (shared cell), not a value snapshot, so a
    // mutation of the outer variable made after the closure is built is visible
    // when the closure later reads it.
    let dir = unique_temp_dir("cap-see");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    x = 1\n    f = () => { return x }\n    x = 99\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capsee") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "99\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_early_return() {
    // Stage 6b R2 guard: a boxed cell released on every return path must not
    // double-free; each control-flow path reaches exactly one return.
    let dir = unique_temp_dir("cap-ret");
    fs::write(
        dir.join("main.ku"),
        "fn pick(flag: bool): int! {\n    n = 0\n    bump = () => { n = n + 1  return n }\n    if (flag) {\n        x = bump()\n        return ok(x)\n    }\n    y = bump()\n    z = bump()\n    return ok(z)\n}\n\nfn main(): null! {\n    println(pick(true)?)\n    println(pick(false)?)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capret") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_bool_println() {
    // Booleans (literals and comparison results) print as `true`/`false`,
    // matching the interpreter rather than the numeric `1`/`0`.
    let dir = unique_temp_dir("bool-print");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    println(true)\n    println(false)\n    println(1 < 2)\n    println(2 < 1)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "boolprint") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "true\nfalse\ntrue\nfalse\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_call_depth_guard_matches_interpreter() {
    // Stage 6f: deep/infinite recursion reports "maximum function call depth
    // exceeded" and exits cleanly (code 1) instead of a native stack-overflow
    // crash, matching the interpreter's MAX_CALL_DEPTH. A shallow recursion runs.
    let dir = unique_temp_dir("depth-guard");
    fs::write(
        dir.join("shallow.ku"),
        "fn rec(n: int): int {\n    if (n <= 0) { return 0 }\n    return rec(n - 1)\n}\n\nfn main(): null! {\n    println(rec(10))\n    return ok(null)\n}\n",
    )
    .expect("write shallow.ku");
    fs::write(
        dir.join("deep.ku"),
        "fn rec(n: int): int {\n    if (n <= 0) { return 0 }\n    return rec(n - 1)\n}\n\nfn main(): null! {\n    println(rec(1000))\n    return ok(null)\n}\n",
    )
    .expect("write deep.ku");

    if let Some(exe) = native_build(&dir, "shallow.ku", "depthshallow") {
        let (stdout, code) = run_binary(&exe);
        assert_eq!(stdout.replace('\r', ""), "0\n");
        assert_eq!(code, Some(0));
    }
    if let Some(exe) = native_build(&dir, "deep.ku", "depthdeep") {
        let (_stdout, code) = run_binary(&exe);
        // Clean guarded exit, not a stack-overflow crash (127 / access violation).
        assert_eq!(code, Some(1), "deep recursion must exit cleanly, not crash");
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_call_depth_counter_no_drift_on_fail_paths() {
    // Regression: every function exit path — including `fail`, `?` propagation
    // and catch — must decrement the thread-local call-depth counter. Otherwise
    // sequential fail-and-catch calls drift it up and spuriously trip the guard.
    // rec(400) with a fail-then-catch helper at every level stays well under 512
    // active frames, so it must succeed.
    let dir = unique_temp_dir("depth-drift");
    fs::write(
        dir.join("main.ku"),
        "fn helper(): int! {\n    fail { domain: \"x\", code: \"boom\", message: \"z\" }\n}\n\nfn rec(n: int): int {\n    if (n <= 0) { return 0 }\n    try {\n        v = helper()?\n        return v\n    } catch (e) {\n    }\n    return rec(n - 1)\n}\n\nfn main(): null! {\n    println(rec(400))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "depthdrift") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "0\n");
    assert_eq!(
        code,
        Some(0),
        "fail/? exit paths must decrement; no counter drift"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_str_shared() {
    // Stage 6c-str: a captured owned str lives in a shared cell; rebinding the
    // outer variable is visible to the closure, which borrows the cell on read
    // (the `prefix + name` concat borrows `prefix`, no implicit clone).
    let dir = unique_temp_dir("cap-str");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    prefix = \"Hello \"\n    greet = (name: str) => { return prefix + name }\n    prefix = \"Bye \"\n    println(greet(\"Ku\"))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capstr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Bye Ku\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_str_owned_heap_reassign() {
    // Stage 6c-str: reassigning a captured owned str drops the old heap buffer
    // and moves the new one in; the self-read `s = s + "e"` reads before the
    // old value is dropped. `.clone()` returns an owned copy. No double-free.
    let dir = unique_temp_dir("cap-heap");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    s = \"a\" + \"b\"\n    show = () => { return s.clone() }\n    println(show())\n    s = \"c\" + \"d\"\n    println(show())\n    s = s + \"e\"\n    println(show())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capheap") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "ab\ncd\ncde\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_array_borrowed_read() {
    // Stage 6c-array: a closure captures an owned array through a shared cell and
    // borrows it on read (`xs.len()`), no clone/drop; native matches the
    // interpreter (`3`).
    let dir = unique_temp_dir("cap-arr");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    xs = [1, 2, 3]\n    f = () => { return xs.len() }\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "caparr") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_array_reassign_visible() {
    // Stage 6c-array: rebinding the captured array drops the old heap buffer and
    // moves the new one into the shared cell; the closure sees the new length
    // (`4`). No double-free (the old buffer is dropped exactly once).
    let dir = unique_temp_dir("cap-arr-reassign");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    xs = [1, 2]\n    f = () => { return xs.len() }\n    xs = [9, 9, 9, 9]\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "caparrre") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "4\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_object_borrowed_read_and_reassign() {
    // Stage 6c-object: a closure captures an owned object through a shared cell
    // and borrows it on read (`get_or`), no clone/drop of the object. Rebinding
    // the object is visible to the closure (`1` then `7`); the old object is
    // dropped exactly once.
    let dir = unique_temp_dir("cap-obj");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    o = {\"a\": 1}\n    g = () => { return o.get_or(\"a\", null) }\n    println(g())\n    o = {\"a\": 7}\n    println(g())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "capobj") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n7\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_struct_and_result_owned_payloads() {
    let dir = unique_temp_dir("capture-struct-result");
    fs::write(
        dir.join("main.ku"),
        r#"struct Box { text: str }

fn main(): null! {
    current = Box { text: "a" + "b" }
    read_box = () => { return current.text.clone() }
    println(read_box())
    current = Box { text: "c" + "d" }
    println(read_box())

    saved = ok("r" + "1")
    read_result = () => { return saved.clone() }
    println(read_result()?)
    saved = ok("r" + "2")
    println(read_result()?)
    return ok(null)
}
"#,
    )
    .expect("write captured struct/result source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_drop_struct_Box(&c->value);"));
    assert!(c.contains("ku_result_drop_str(&c->value);"));

    let Some(exe) = native_build(&dir, "main.ku", "capturestructresult") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "ab\ncd\nr1\nr2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_kuvalue_reassigns_and_drops() {
    let dir = unique_temp_dir("capture-kuvalue");
    fs::write(
        dir.join("main.ku"),
        r#"import json from "std.json"

fn main(): null! {
    value = json.parse("{\"name\":\"first\"}")?
    read = () => { return value.clone() }
    first = read()
    println(first["name"]?)
    value = json.parse("{\"name\":\"second\"}")?
    second = read()
    println(second["name"]?)
    return ok(null)
}
"#,
    )
    .expect("write captured KuValue source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_value_drop(&c->value);"));

    let Some(exe) = native_build(&dir, "main.ku", "capturekuvalue") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "first\nsecond\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_captured_function_argument_is_borrowed_across_calls() {
    let dir = unique_temp_dir("capture-function-borrow");
    fs::write(
        dir.join("main.ku"),
        r#"fn Apply(op: fn(): int): int {
    return op()
}

fn main(): null! {
    count = 0
    inner = () => { count = count + 1  return count }
    outer = () => { return Apply(inner) }
    println(outer())
    println(outer())
    println(inner())
    return ok(null)
}
"#,
    )
    .expect("write captured function borrow source");

    let c = native_emit_c(&dir, "main.ku");
    assert!(c.contains("ku_refcount_retain(&c->rc, \"closure cell\")"));
    assert!(c.contains("ku_closure_clone_fn__to_int((__e->inner)->value)"));

    let Some(exe) = native_build(&dir, "main.ku", "capturefunctionborrow") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n3\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_capture_array_clone_returns_owned() {
    // Stage 6c-array: `.clone()` on a captured array borrows the cell and produces
    // a fresh owned array that can be returned/stored; native matches interp.
    let dir = unique_temp_dir("cap-arr-clone");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    xs = [5, 6, 7]\n    f = () => { return xs.clone() }\n    ys = f()\n    println(ys.len())\n    println(ys[2])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "caparrcl") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n7\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_param_inferred_from_typed_binding() {
    // A/G: a typed binding `greet: fn(str): str` supplies the type of the
    // otherwise unannotated closure parameter `name`; native output matches the
    // interpreter (`Hello Ku`).
    let dir = unique_temp_dir("closure-typed-binding");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    greet: fn(str): str = (name) => { return \"Hello \" + name }\n    println(greet(\"Ku\"))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closbind") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "Hello Ku\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_param_inferred_from_higher_order_parameter() {
    // B/G: a higher-order parameter `op: fn(int): int` supplies the type of the
    // unannotated closure parameter `x`; `Apply((x) => x + 1, 41)` is 42 both
    // natively and in the interpreter.
    let dir = unique_temp_dir("closure-hof-param");
    fs::write(
        dir.join("main.ku"),
        "fn Apply(op: fn(int): int, v: int): int {\n    return op(v)\n}\n\nfn main(): null! {\n    println(Apply((x) => x + 1, 41))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closhof") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_escape_factory_counter() {
    // Stage 6e: a factory returns a capturing closure whose env (the boxed `n`
    // cell) escapes the factory's frame. The returned closure keeps mutating its
    // own cell across calls (1, 2); the env is ref-counted so it outlives the
    // factory without a double-free.
    let dir = unique_temp_dir("closure-escape-factory");
    fs::write(
        dir.join("main.ku"),
        "fn make_counter(): fn(): int {\n    n = 0\n    return () => { n = n + 1  return n }\n}\n\nfn main(): null! {\n    c = make_counter()\n    println(c())\n    println(c())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "escfactory") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_escape_two_independent_counters() {
    // Stage 6e: two factory calls produce two closures over *separate* cells, so
    // their counts are independent (1, 2 for the first; 1 for the second). Each
    // env is released exactly once.
    let dir = unique_temp_dir("closure-two-counters");
    fs::write(
        dir.join("main.ku"),
        "fn make_counter(): fn(): int {\n    n = 0\n    return () => { n = n + 1  return n }\n}\n\nfn main(): null! {\n    a = make_counter()\n    b = make_counter()\n    println(a())\n    println(a())\n    println(b())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "twocounters") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_clone_shares_captured_cell() {
    // Stage 6e-2: `.clone()` on a capturing closure bumps the env refcount and
    // shares the same cell (it is not deep-copied). `f()` then `g()` observe the
    // same counter (1 then 2). No double-free (env released once per owner).
    let dir = unique_temp_dir("closure-clone-shared");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    n = 0\n    f = () => { n = n + 1  return n }\n    g = f.clone()\n    println(f())\n    println(g())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "cloneshared") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n2\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_stored_in_typed_array() {
    // Stage 6e-3: a capturing closure is moved into a `[fn(): int]` array and
    // invoked through `fns[0]()`. The array owns the closure's env and releases
    // it on drop (no leak, no double-free).
    let dir = unique_temp_dir("closure-typed-array");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    n = 10\n    f = () => { n = n + 1  return n }\n    fns: [fn(): int] = [f]\n    println(fns[0]())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closarray") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "11\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_of_late_signature_closures() {
    // Closure signatures that mention aggregate return types are emitted in the
    // late ABI pass. Arrays of those closures still need the closure typedef and
    // its clone/drop helpers before their own helper bodies are compiled.
    let dir = unique_temp_dir("closure-late-signature-array");
    fs::write(
        dir.join("main.ku"),
        concat!(
            "struct Boxed { text: str }\n\n",
            "fn Numbers(): [int] { return [7] }\n",
            "fn BoxValue(): Boxed { return Boxed { text: \"Ku\" + \"String\" } }\n\n",
            "fn Words(): [str]! { return ok([\"typed\" + \" result\"]) }\n\n",
            "fn main(): null! {\n",
            "    number_factories: [fn(): [int]] = [Numbers]\n",
            "    box_factories: [fn(): Boxed] = [BoxValue]\n",
            "    result_factories: [fn(): [str]!] = [Words]\n",
            "    values = number_factories[0]()\n",
            "    boxed = box_factories[0]()\n",
            "    words = result_factories[0]()?\n",
            "    println(values[0])\n",
            "    println(boxed.text)\n",
            "    println(words[0])\n",
            "    return ok(null)\n",
            "}\n",
        ),
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "lateclosurearray") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "7\nKuString\ntyped result\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_stored_in_dynamic_object() {
    // Stage 6e-4: a capturing closure is boxed into a dynamic object as a
    // `KU_FUNCTION` KuValue. Retrieving it with `get_or` clones the KuValue
    // (env retained) and prints `<function>`, matching the interpreter. Both the
    // object and the retrieved value release the env, so it is freed once.
    let dir = unique_temp_dir("closure-dynamic-object");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    n = 100\n    f = () => { n = n + 1  return n }\n    o = { \"handler\": f }\n    g = o.get_or(\"handler\", null)\n    println(g)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "closobject") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "<function>\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_argument_is_borrowed_not_moved() {
    // Stage 6d: passing a capturing closure to a higher-order function borrows it
    // (a plain struct copy sharing the env); the callee does not release it, so
    // the caller's binding stays live for a later direct call. `CallTwice(f)`
    // yields 3 (1+2) and the following `f()` yields 3, matching the interpreter.
    let dir = unique_temp_dir("closure-borrow-arg");
    fs::write(
        dir.join("main.ku"),
        "fn CallTwice(op: fn(): int): int {\n    return op() + op()\n}\n\nfn main(): null! {\n    n = 0\n    f = () => { n = n + 1  return n }\n    println(CallTwice(f))\n    println(f())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "borrowarg") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "3\n3\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_closure_returned_from_parameter_is_ref_counted() {
    // Stage 6d soundness: a function receives a capturing closure by retain (the
    // callee owns its own env reference) and returns it. The returned closure and
    // the caller's original binding then share the env, each releasing it once —
    // no double-free (regression guard for pass-by-retain of function arguments).
    let dir = unique_temp_dir("closure-return-param");
    fs::write(
        dir.join("main.ku"),
        "fn id(op: fn(): int): fn(): int {\n    return op\n}\n\nfn main(): null! {\n    n = 0\n    f = () => { n = n + 1  return n }\n    g = id(f)\n    println(g())\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "retparam") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "1\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_self_recursion() {
    // Stage 6f: a local named function `fn fact(...)` defined inside `main` is
    // lifted like a closure; a self-recursive call reuses the running env by
    // calling the lifted body directly (no self-capture, no RC cycle). fact(5)
    // is 120 both native and interpreted.
    let dir = unique_temp_dir("local-fn-fact");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    fn fact(n: int): int {\n        if (n <= 1) { return 1 }\n        return n * fact(n - 1)\n    }\n    println(fact(5))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localfact") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "120\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_helper() {
    // Stage 6f: a non-recursive local helper is bound to a closure value and
    // invoked indirectly; dbl(21) is 42.
    let dir = unique_temp_dir("local-fn-dbl");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    fn dbl(x: int): int { return x * 2 }\n    println(dbl(21))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localdbl") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "42\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_captures_outer() {
    // Stage 6f: a local function captures an enclosing Copy local through a
    // shared cell (the closure machinery), reading it inside the body; addk(5)
    // with k == 10 is 15.
    let dir = unique_temp_dir("local-fn-capture");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    k = 10\n    fn addk(x: int): int { return x + k }\n    println(addk(5))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localcapk") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "15\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_as_argument() {
    // Stage 6f: a local named function flows as a first-class closure value
    // through a `fn(int): int` parameter and is invoked inside the callee;
    // apply(dbl, 20) is 40.
    let dir = unique_temp_dir("local-fn-arg");
    fs::write(
        dir.join("main.ku"),
        "fn apply(f: fn(int): int, v: int): int { return f(v) }\n\nfn main(): null! {\n    fn dbl(x: int): int { return x * 2 }\n    println(apply(dbl, 20))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localapply") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "40\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_recursive_and_capturing() {
    // Stage 6f soundness: a local function that BOTH captures an outer cell and
    // self-recurses. The self-call threads the running `__env` (holding `base`'s
    // cell) directly instead of re-boxing the function into that env, so there is
    // no reference cycle and the cell is released exactly once. sumdown(3) adds
    // 3 + 2 + 1 + base(100) == 106.
    let dir = unique_temp_dir("local-fn-recap");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    base = 100\n    fn sumdown(n: int): int {\n        if (n <= 0) { return base }\n        return n + sumdown(n - 1)\n    }\n    println(sumdown(3))\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "localrecap") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "106\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_local_named_function_depth_guard() {
    // Stage 6f: self-recursion respects the shared MAX_CALL_DEPTH guard — a deep
    // local recursion exits cleanly with code 1 ("maximum function call depth
    // exceeded") instead of a native stack-overflow crash, matching the
    // interpreter. A shallow local recursion still runs to completion.
    let dir = unique_temp_dir("local-fn-depth");
    fs::write(
        dir.join("shallow.ku"),
        "fn main(): null! {\n    fn rec(n: int): int {\n        if (n <= 0) { return 0 }\n        return rec(n - 1)\n    }\n    println(rec(5))\n    return ok(null)\n}\n",
    )
    .expect("write shallow.ku");
    fs::write(
        dir.join("deep.ku"),
        "fn main(): null! {\n    fn rec(n: int): int {\n        if (n <= 0) { return 0 }\n        return rec(n - 1)\n    }\n    println(rec(1000))\n    return ok(null)\n}\n",
    )
    .expect("write deep.ku");

    if let Some(exe) = native_build(&dir, "shallow.ku", "localdepthshallow") {
        let (stdout, code) = run_binary(&exe);
        assert_eq!(stdout.replace('\r', ""), "0\n");
        assert_eq!(code, Some(0));
    }
    if let Some(exe) = native_build(&dir, "deep.ku", "localdepthdeep") {
        let (_stdout, code) = run_binary(&exe);
        assert_eq!(code, Some(1), "deep local recursion must exit cleanly");
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_map_matches_interpreter() {
    // Stage 6f: `[T].map(fn(T): U) -> [U]`. The mapper's parameter carries NO
    // annotation, so its type is propagated from the array's element type (the
    // checker infers it, so the interpreter accepts `map(x => x*2)`; native must
    // too — rule 8). The result array is built by invoking the mapper per element.
    let dir = unique_temp_dir("array-map-basic");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    r = [1, 2, 3].map(x => x * 2)\n    println(r[0])\n    println(r[1])\n    println(r[2])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "mapbasic") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "2\n4\n6\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_map_captured_mapper_matches_interpreter() {
    // Stage 6f: the mapper captures an outer cell (`k`). Every element invokes the
    // same env, and the env (and its captured cell) is released exactly once when
    // map finishes — no leak, no double-free (verified under ASan + the CRT debug
    // heap). `[1,2].map(x => x + k)` with k==10 yields 11/12.
    let dir = unique_temp_dir("array-map-capture");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    k = 10\n    r = [1, 2].map(x => x + k)\n    println(r[0])\n    println(r[1])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "mapcapture") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "11\n12\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_array_map_result_stored_in_variable() {
    // Stage 6f: the map result is a first-class array value — bind it to a local
    // and index it. `[10,20].map(x => x * 3)` yields 30/60.
    let dir = unique_temp_dir("array-map-store");
    fs::write(
        dir.join("main.ku"),
        "fn main(): null! {\n    d = [10, 20].map(x => x * 3)\n    println(d[0])\n    println(d[1])\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "mapstore") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "30\n60\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_void_match_statement_runs_arms() {
    // Regression: a match used as a statement has no value, so lowering must emit
    // the arms as expressions. Storing them into a `void t0` local failed to compile.
    let dir = unique_temp_dir("void-match");
    fs::write(
        dir.join("main.ku"),
        "enum Mode { Hi  Lo }\nfn main(): null! {\n    m = Mode.Hi\n    match m { Mode.Hi => println(\"hi\")  Mode.Lo => println(\"lo\") }\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "voidmatch") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "hi\n");
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_owned_local_reassigned_in_loop_does_not_double_free() {
    // Regression: an owned local rebound each iteration must drop its previous
    // value before the new one is stored — and must not drop the value it just
    // took ownership of.
    let dir = unique_temp_dir("loop-owned");
    fs::write(
        dir.join("main.ku"),
        "struct Box { tag: str }\nfn main(): null! {\n    i = 0\n    last = \"\"\n    while (i < 3) {\n        b = Box { tag: \"row\".clone() }\n        s = b.tag\n        last = s\n        i = i + 1\n    }\n    println(last)\n    return ok(null)\n}\n",
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "loopowned") else {
        return;
    };

    let (stdout, code) = run_binary(&exe);
    assert_eq!(stdout.replace('\r', ""), "row\n");
    assert_eq!(
        code,
        Some(0),
        "loop-rebound owned locals must not double-free"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_for_int_and_owned_arrays_evaluate_once_and_preserve_source() {
    let dir = unique_temp_dir("for-owned-arrays");
    fs::write(
        dir.join("main.ku"),
        r#"
fn Values(): [str] {
    println("make")
    return ["a".clone(), "b".clone()]
}

fn Find(): str! {
    rows = [["skip".clone()], ["hit".clone(), "later".clone()]]
    try {
        for row in rows {
            for text in row {
                if (text == "hit") { return ok(text) }
            }
        }
    } finally {
        println("find-finally")
    }
    fail "missing"
}

fn main(): null! {
    total = 0
    for i in 3 { total += i }
    println(total)

    values = Values()
    for text in values { println(text) }
    println(values.len())

    nested = [[1, 2], [3, 4]]
    nested_total = 0
    for row in nested {
        for number in row { nested_total += number }
    }
    println(nested_total)
    println(nested[0][1])

    empty: [int] = []
    for empty_value in empty { println(empty_value) }
    println(empty.len())
    println(Find()?)
    return ok(null)
}
"#,
    )
    .expect("write main.ku");

    let Some(exe) = native_build(&dir, "main.ku", "forowned") else {
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "3\nmake\na\nb\n2\n10\n2\n0\nfind-finally\nhit\n"
    );
    assert_eq!(code, Some(0));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_logical_operators_short_circuit_effectful_rhs() {
    let dir = unique_temp_dir("logical-short-circuit");
    fs::write(
        dir.join("main.ku"),
        r#"fn ExplodeAnd(): bool! {
    fail {
        domain: "short_circuit",
        code: "and_rhs",
        message: "logical AND evaluated its failing RHS"
    }
}

fn ExplodeOr(): bool! {
    fail {
        domain: "short_circuit",
        code: "or_rhs",
        message: "logical OR evaluated its failing RHS"
    }
}

fn MaybeAnd(left: bool): bool! {
    return ok(left && ExplodeAnd()?)
}

fn MaybeOr(left: bool): bool! {
    return ok(left || ExplodeOr()?)
}

fn BoundsAnd(left: bool): bool {
    values = [1]
    return left && values[99] == 0
}

fn BoundsOr(left: bool): bool {
    values = [1]
    return left || values[99] == 0
}

fn Inspect(values: [str]): bool! {
    copy = values.clone()
    return ok(copy[0] == "owned result")
}

fn UseClosure(op: fn(): bool): bool {
    return op()
}

fn main(): null! {
    println(MaybeAnd(false)?)
    println(MaybeOr(true)?)
    try {
        MaybeAnd(true)?
    } catch(err) {
        println(err.domain)
        println(err.code)
    }
    try {
        MaybeOr(false)?
    } catch(err) {
        println(err.domain)
        println(err.code)
    }
    println("after-errors")
    println(BoundsAnd(false))
    println(BoundsOr(true))

    enabled = false
    println((enabled && Inspect(["skip " + "owned"])?))
    enabled = true
    println((enabled && Inspect(["owned " + "result"])?))
    println((enabled || Inspect(["skip " + "owned"])?))
    enabled = false
    println((enabled || Inspect(["owned " + "result"])?))

    println(false && (true && Inspect(["nested " + "skip"])?))
    println(true || (false || Inspect(["nested " + "skip"])?))
    println(true && (false || Inspect(["owned " + "result"])?))

    count = 0
    run_closure = false
    println(run_closure && UseClosure(() => {
        count = count + 100
        return true
    }))
    run_closure = true
    i = 0
    while (i < 3) {
        ran = run_closure && UseClosure(() => {
            count = count + 1
            return true
        })
        if (!ran) { fail "required closure RHS was skipped" }
        i = i + 1
    }
    println(count)
    return ok(null)
}
"#,
    )
    .expect("write logical short-circuit source");

    let c = native_emit_c(&dir, "main.ku");

    fn assert_rhs_is_behind_branch(
        body: &str,
        condition: &str,
        rhs_marker: &str,
        rhs_on_true: bool,
    ) {
        let branch_marker = format!("if ({condition}) goto block");
        let branch_pos = body
            .find(&branch_marker)
            .unwrap_or_else(|| panic!("missing logical branch '{branch_marker}' in:\n{body}"));
        let branch_line = body[branch_pos..]
            .lines()
            .next()
            .expect("logical branch line");
        let (_, targets) = branch_line
            .split_once("goto block")
            .expect("logical then target");
        let (then_target, else_target) = targets
            .split_once("; else goto block")
            .expect("logical else target");
        let else_target = else_target
            .strip_suffix(';')
            .expect("logical branch terminator");
        let (rhs_target, skip_target) = if rhs_on_true {
            (then_target, else_target)
        } else {
            (else_target, then_target)
        };
        let rhs_label = format!("block{rhs_target}:;");
        let skip_label = format!("block{skip_target}:;");
        let rhs_label_pos = body
            .find(&rhs_label)
            .unwrap_or_else(|| panic!("missing RHS label '{rhs_label}' in:\n{body}"));
        let rhs_pos = body
            .find(rhs_marker)
            .unwrap_or_else(|| panic!("missing RHS marker '{rhs_marker}' in:\n{body}"));
        let skip_label_pos = body
            .find(&skip_label)
            .unwrap_or_else(|| panic!("missing merge label '{skip_label}' in:\n{body}"));
        assert!(
            branch_pos < rhs_label_pos && rhs_label_pos < rhs_pos && rhs_pos < skip_label_pos,
            "RHS must exist only behind its selected logical edge:\n{body}"
        );
    }

    let after_and = c
        .split_once("KuResult_bool MaybeAnd(bool left) {")
        .expect("generated MaybeAnd function")
        .1;
    let (and_body, after_or) = after_and
        .split_once("KuResult_bool MaybeOr(bool left) {")
        .expect("generated MaybeOr function");
    let (or_body, after_bounds_and) = after_or
        .split_once("bool BoundsAnd(bool left) {")
        .expect("generated BoundsAnd function");
    let (bounds_and_body, after_bounds_or) = after_bounds_and
        .split_once("bool BoundsOr(bool left) {")
        .expect("generated BoundsOr function");
    let (bounds_or_body, _) = after_bounds_or
        .split_once("KuResult_bool Inspect(")
        .expect("generated Inspect function");

    assert!(and_body.contains("bool __ku_logical_") && and_body.contains("= false;"));
    assert!(or_body.contains("bool __ku_logical_") && or_body.contains("= true;"));
    assert!(!and_body.contains("&&"), "logical AND must be explicit CFG");
    assert!(!or_body.contains("||"), "logical OR must be explicit CFG");
    assert_rhs_is_behind_branch(and_body, "left", "ExplodeAnd()", true);
    assert_rhs_is_behind_branch(or_body, "left", "ExplodeOr()", false);
    assert_rhs_is_behind_branch(bounds_and_body, "left", "ku_array_get_int", true);
    assert_rhs_is_behind_branch(bounds_or_body, "left", "ku_array_get_int", false);

    // The source contains skipped owned Result/array/string and closure RHS
    // values. Their instructions must remain inside logical RHS blocks; native
    // execution below exercises both skipped zero-initialized cleanup and the
    // required path repeatedly (including reuse of an owned closure temp).
    assert!(c.contains("ku_result_drop_bool(&t"));
    assert!(c.contains("ku_array_drop_str(&t"));
    assert!(c.contains("ku_string_drop(&t"));
    assert!(c.contains("KuClosure_fn__to_bool"));
    assert!(c.contains(")->release((t"));

    let Some(exe) = native_build(&dir, "main.ku", "logicalshort") else {
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let (stdout, code) = run_binary(&exe);
    assert_eq!(
        stdout.replace('\r', ""),
        "false\ntrue\nshort_circuit\nand_rhs\nshort_circuit\nor_rhs\nafter-errors\nfalse\ntrue\nfalse\ntrue\ntrue\ntrue\nfalse\ntrue\ntrue\nfalse\n3\n"
    );
    assert_eq!(code, Some(0));

    fs::remove_dir_all(&dir).ok();
}
