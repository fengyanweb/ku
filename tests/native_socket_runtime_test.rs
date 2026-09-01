//! Process-level Winsock ownership shared by the native net and Redis clients.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::fs;
use std::process::Command;

use native_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

fn fixture() -> &'static str {
    r#"import net from "std.net"
import redis from "std.redis"

fn main(): null! {
    socket = net.client({ host: "127.0.0.1", port: 9 })?
    socket.close()
    cache = redis.client({ host: "127.0.0.1", port: 6379 })?
    cache.close()
    return ok(null)
}
"#
}

fn function<'a>(generated: &'a str, signature: &str) -> &'a str {
    let start = generated
        .find(signature)
        .unwrap_or_else(|| panic!("generated C is missing {signature}"));
    let body = &generated[start..];
    let open = body
        .find('{')
        .unwrap_or_else(|| panic!("generated C function has no body: {signature}"));
    let mut depth = 0usize;
    for (offset, byte) in body.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &body[..open + offset + 1];
                }
            }
            _ => {}
        }
    }
    panic!("generated C function is unterminated: {signature}");
}

fn winsock_runtime(generated: &str) -> &str {
    let start = generated
        .find("#define KU_NATIVE_RUNTIME_WINSOCK 1")
        .expect("shared Winsock runtime marker");
    let end = generated[start..]
        .find("#define KU_NATIVE_RUNTIME_NET_SOCKET 1")
        .map(|offset| start + offset)
        .expect("net runtime after shared Winsock runtime");
    &generated[start..end]
}

#[test]
fn native_net_and_redis_share_one_cached_process_winsock_owner() {
    let directory = TempDir::new("shared-winsock-source");
    let generated = emit_c(directory.path(), fixture());
    let runtime = winsock_runtime(&generated);

    assert_eq!(generated.matches("WSAStartup(").count(), 1);
    assert_eq!(
        generated
            .matches("static INIT_ONCE ku_winsock_runtime_once = INIT_ONCE_STATIC_INIT")
            .count(),
        1
    );
    assert!(!generated.contains("static INIT_ONCE ku_net_wsa_once ="));
    assert!(!generated.contains("static INIT_ONCE ku_redis_wsa_once ="));
    assert_eq!(
        runtime
            .matches("atexit(ku_winsock_runtime_shutdown)")
            .count(),
        1
    );
    assert!(runtime.contains("LOBYTE(data.wVersion) != 2"));
    assert!(runtime.contains("HIBYTE(data.wVersion) != 2"));
    assert!(
        runtime.contains("return TRUE;"),
        "once callback must cache failures"
    );
    assert!(runtime.contains("KU_WINSOCK_RUNTIME_FAILED"));
    assert!(
        function(&generated, "static int ku_net_socket_startup(void)")
            .contains("return ku_winsock_runtime_startup()")
    );
    assert!(function(&generated, "static int ku_redis_ensure_wsa(void)")
        .contains("return ku_winsock_runtime_startup()"));
    let net_close = function(
        &generated,
        "static uint8_t ku_net_close(KuNetClient* client)",
    );
    let redis_close = function(
        &generated,
        "static uint8_t ku_redis_close(KuRedisClient* client)",
    );
    assert!(!net_close.contains("WSACleanup"));
    assert!(!redis_close.contains("WSACleanup"));
}

#[test]
fn native_winsock_success_and_failures_are_balanced_and_not_retried() {
    let directory = TempDir::new("shared-winsock-runtime");
    let generated = emit_c(directory.path(), fixture());
    let runtime = winsock_runtime(&generated);
    let net_start = function(&generated, "static int ku_net_socket_startup(void)");
    let redis_start = function(&generated, "static int ku_redis_ensure_wsa(void)");
    let source = directory.path().join("shared-winsock-runtime.c");
    fs::write(
        &source,
        format!(
            r#"#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define _WIN32 1
#define CALLBACK
#define TRUE 1
#define FALSE 0
typedef int BOOL;
typedef void* PVOID;
typedef struct KuTestInitOnce {{ int complete; }} INIT_ONCE;
typedef INIT_ONCE* PINIT_ONCE;
#define INIT_ONCE_STATIC_INIT {{0}}
typedef struct KuTestWsaData {{ uint16_t wVersion; }} WSADATA;
#define MAKEWORD(low, high) ((uint16_t)(((uint16_t)(low)) | ((uint16_t)(high) << 8)))
#define LOBYTE(word) ((uint8_t)((word) & 0xffU))
#define HIBYTE(word) ((uint8_t)(((word) >> 8) & 0xffU))

static int startup_calls = 0;
static int cleanup_calls = 0;
static int init_once_calls = 0;
static int startup_result = 0;
static uint16_t negotiated_version = 0;
static int atexit_result = 0;
static void (*registered_cleanup)(void) = NULL;

static int ku_test_wsa_startup(uint16_t requested, WSADATA* data) {{
  startup_calls++;
  if (requested != MAKEWORD(2, 2) || !data) return 98;
  data->wVersion = negotiated_version;
  return startup_result;
}}
static int ku_test_wsa_cleanup(void) {{ cleanup_calls++; return 0; }}
static int ku_test_atexit(void (*callback)(void)) {{
  if (atexit_result != 0) return atexit_result;
  if (registered_cleanup) return 97;
  registered_cleanup = callback;
  return 0;
}}
static BOOL InitOnceExecuteOnce(
    PINIT_ONCE once,
    BOOL (CALLBACK *callback)(PINIT_ONCE, PVOID, PVOID*),
    PVOID parameter,
    PVOID* context) {{
  init_once_calls++;
  if (once->complete) return TRUE;
  BOOL result = callback(once, parameter, context);
  if (result) once->complete = 1;
  return result;
}}

#define WSAStartup ku_test_wsa_startup
#define WSACleanup ku_test_wsa_cleanup
#define atexit ku_test_atexit

{runtime}
{net_start}
{redis_start}

int main(int argc, char** argv) {{
  if (argc != 2) return 64;
  negotiated_version = MAKEWORD(2, 2);
  if (strcmp(argv[1], "startup-failure") == 0) startup_result = 1;
  else if (strcmp(argv[1], "bad-version") == 0) negotiated_version = MAKEWORD(2, 1);
  else if (strcmp(argv[1], "atexit-failure") == 0) atexit_result = 1;
  else if (strcmp(argv[1], "success") != 0) return 65;

  int expected = strcmp(argv[1], "success") == 0 ? 0 : -1;
  if (ku_net_socket_startup() != expected
      || ku_redis_ensure_wsa() != expected
      || ku_net_socket_startup() != expected
      || ku_redis_ensure_wsa() != expected) return 66;
  if (startup_calls != 1 || init_once_calls != 4) return 67;

  if (strcmp(argv[1], "success") == 0) {{
    if (cleanup_calls != 0 || !registered_cleanup) return 68;
    registered_cleanup();
    registered_cleanup();
    if (cleanup_calls != 1) return 69;
  }} else if (strcmp(argv[1], "startup-failure") == 0) {{
    if (cleanup_calls != 0 || registered_cleanup) return 70;
  }} else {{
    if (cleanup_calls != 1 || registered_cleanup) return 71;
  }}
  puts(argv[1]);
  return 0;
}}
"#
        ),
    )
    .expect("write shared Winsock runtime harness");
    let Some(executable) = compile_harness(directory.path(), &source, "shared-winsock-runtime")
    else {
        eprintln!("skip: no C compiler available for shared Winsock runtime test");
        return;
    };

    for case in [
        "success",
        "startup-failure",
        "bad-version",
        "atexit-failure",
    ] {
        let mut command = Command::new(&executable);
        command.current_dir(directory.path()).arg(case);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .unwrap_or_else(|error| panic!("Winsock case {case} was not bounded: {error}"));
        assert!(
            output.status.success(),
            "Winsock case {case} failed ({:?}):\n{}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace('\r', ""),
            format!("{case}\n")
        );
        assert!(output.stderr.is_empty());
    }
}
