//! Native HTTP end-to-end tests: compile the cli_v001 HTTP scenarios with the
//! real C toolchain (MSVC cl.exe via vcvars, or zig/clang/gcc) and drive the
//! produced binary over real sockets, asserting the SAME status codes the
//! interpreter produces in `cli_v001_test.rs`. This is the Stage 8e
//! "run cli_v001 HTTP cases against the native binary" check.
//!
//! Coverage vs the interpreter:
//!   * routing (exact/param/anon-fn/named-fn handler, 204)          — asserted
//!   * error statuses 404 / 405 / 413 / 400 / 431                   — asserted
//!   * request limits via both `http.server({...})` and `app.x = v` — asserted
//!   * 408 idle read timeout                                        — asserted
//!   * 503 backpressure (max_connections/active/pending)            — asserted
//!   * 504 handler timeout                                          — NOT asserted:
//!     native cannot safely preempt a compiled, compute-bound handler, so it is
//!     a documented native limitation (see `native_slow_handler_stays_up`, which
//!     only checks the server keeps serving other requests).
//!
//! When no C compiler is present every test skips cleanly instead of failing.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// HTTP native tests bind real ports; serialize them so concurrent servers do
// not fight over the admission-control probes.
static HTTP_TEST_LOCK: Mutex<()> = Mutex::new(());

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
        "ku-native-http-{name}-{}",
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

fn unused_local_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind temporary port");
    let address = listener.local_addr().expect("temp addr").to_string();
    drop(listener);
    address
}

/// Compile `source` (with `__ADDRESS__` replaced) to a native binary and spawn
/// it. Returns `None` when no C compiler is available (skip the test).
fn spawn_native_server(name: &str, source: &str, address: &str) -> Option<NativeHttpServer> {
    let dir = unique_temp_dir(name);
    let entry = "server.ku";
    fs::write(dir.join(entry), source.replace("__ADDRESS__", address)).expect("write ku source");
    let out = exe_name("server");
    let build = Command::new(ku_binary())
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &out])
        .output()
        .expect("spawn ku build --native");
    if !build.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
        if combined.contains("C compiler not found") {
            eprintln!("skip: no C compiler available for native HTTP e2e test");
            return None;
        }
        panic!("ku build --native failed for {name}:\n{combined}");
    }
    let exe = dir.join(&out);
    let child = Command::new(&exe)
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn native http server");
    Some(NativeHttpServer {
        child: Some(child),
        _dir: dir,
    })
}

/// Compile `source` to a native binary and return whether it built. Used by the
/// tests that only need to confirm codegen/linking succeeds (no live server).
/// Returns `None` when no C compiler is available (skip).
fn native_builds(name: &str, source: &str) -> Option<bool> {
    let dir = unique_temp_dir(name);
    let entry = "prog.ku";
    fs::write(dir.join(entry), source).expect("write ku source");
    let out = exe_name("prog");
    let build = Command::new(ku_binary())
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &out])
        .output()
        .expect("spawn ku build --native");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    if !build.status.success() && combined.contains("C compiler not found") {
        eprintln!("skip: no C compiler available");
        return None;
    }
    let built = build.status.success() && dir.join(&out).exists();
    if !built {
        eprintln!("build failed for {name}:\n{combined}");
    }
    fs::remove_dir_all(&dir).ok();
    Some(built)
}

struct NativeHttpServer {
    child: Option<Child>,
    _dir: PathBuf,
}

impl Drop for NativeHttpServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn connect_with_retry(address: &str, timeout: Duration) -> TcpStream {
    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < timeout {
        match TcpStream::connect(address) {
            Ok(stream) => return stream,
            Err(err) => last = Some(err),
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "native http server did not accept within {timeout:?}: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    );
}

fn read_http_stream(mut stream: TcpStream, timeout: Duration) -> String {
    stream.set_read_timeout(Some(timeout)).expect("set timeout");
    let mut response = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&buffer[..read]),
            Err(_) if !response.is_empty() => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}

/// Same methodology as cli_v001's `wait_for_http_response`: connect (retrying
/// until the server is up), send the request, half-close the write side, then
/// read the whole response until the server closes.
fn http_response(address: &str, request: &str, timeout: Duration) -> String {
    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < timeout {
        match TcpStream::connect(address) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(700)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_millis(700)))
                    .expect("write timeout");
                if stream.write_all(request.as_bytes()).is_err() {
                    thread::sleep(Duration::from_millis(30));
                    continue;
                }
                let _ = stream.shutdown(Shutdown::Write);
                let mut response = Vec::new();
                let mut buffer = [0u8; 1024];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => response.extend_from_slice(&buffer[..read]),
                        Err(_) => break,
                    }
                }
                if !response.is_empty() {
                    return String::from_utf8_lossy(&response).into_owned();
                }
                last = "empty response".to_string();
            }
            Err(err) => last = err.to_string(),
        }
        thread::sleep(Duration::from_millis(30));
    }
    panic!("native http server did not respond within {timeout:?}: {last}");
}

fn assert_status(response: &str, status: &str) {
    assert!(
        response.starts_with(status),
        "expected {status}, got:\n{response}"
    );
}

const ROUTING_SOURCE: &str = r#"
import "std.http"

fn exact() {
    return http.text("exact")
}

fn main(): null! {
    app = http.service()
    app.get("/user/me", exact)
    app.get("/fn", fn(req) {
        return http.text(req.path.clone())
    })
    app.get("/user/{id}", fn(req) {
        return http.text(req.params.id + ":" + req.query.q + ":" + req.headers.host)
    })
    app.del("/gone", fn() {
        return http.empty()
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_routing_matches_interpreter() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("routing", ROUTING_SOURCE, &address) else {
        return;
    };

    let exact = http_response(
        &address,
        "GET /user/me HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&exact, "HTTP/1.1 200 OK");
    assert!(
        exact.contains("\r\n\r\nexact"),
        "named-fn exact handler should win over param route:\n{exact}"
    );

    let anon = http_response(
        &address,
        "GET /fn HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert!(anon.contains("\r\n\r\n/fn"), "anon fn handler:\n{anon}");

    let param = http_response(
        &address,
        "GET /user/42?q=ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&param, "HTTP/1.1 200 OK");
    assert!(
        param.contains("\r\n\r\n42:ok:localhost"),
        "param + query + headers:\n{param}"
    );

    let gone = http_response(
        &address,
        "DELETE /gone HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&gone, "HTTP/1.1 204 No Content");
}

// Field-assignment config form (`app.max_body_bytes = 4`) — the exact scenario 2
// source from cli_v001. Exercises 404 / 405 / 413 / 400 / 431 on native.
const ERRORS_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    app.max_body_bytes = 4
    app.max_header_bytes = 128
    app.read_header_timeout_ms = 500
    app.read_body_timeout_ms = 500
    app.write_timeout_ms = 500
    app.get("/ok", fn() {
        return http.text("ok")
    })
    app.post("/echo", fn(req) {
        return http.text(req.body)
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_error_statuses_match_interpreter() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("errors", ERRORS_SOURCE, &address) else {
        return;
    };

    let missing = http_response(
        &address,
        "GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&missing, "HTTP/1.1 404 Not Found");

    let wrong_method = http_response(
        &address,
        "POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&wrong_method, "HTTP/1.1 405 Method Not Allowed");

    let too_large = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345",
        Duration::from_secs(3),
    );
    assert_status(&too_large, "HTTP/1.1 413 Content Too Large");

    let bad_header = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nBrokenHeader\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&bad_header, "HTTP/1.1 400 Bad Request");

    let too_large_header = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nX-Long: 12345678901234567890123456789012345678901234567890123456789012345678901234567890\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(
        &too_large_header,
        "HTTP/1.1 431 Request Header Fields Too Large",
    );

    // content-length must parse exactly like the interpreter's `parse::<usize>()`.
    // Verified against rustc: "+5" -> Ok(5); "-5", "+", "", "5abc" and anything
    // too large for usize -> Err -> 400. An overflowing value in particular used
    // to wrap a signed accumulator and slip past the 413 check as a 200.
    let overflow = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 99999999999999999999999\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&overflow, "HTTP/1.1 400 Bad Request");

    let negative = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: -5\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&negative, "HTTP/1.1 400 Bad Request");

    let not_a_number = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2abc\r\nConnection: close\r\n\r\nab",
        Duration::from_secs(3),
    );
    assert_status(&not_a_number, "HTTP/1.1 400 Bad Request");

    // A leading '+' is accepted by Rust's integer FromStr, so it must be accepted
    // here too (2 bytes is within this server's max_body_bytes of 4).
    let signed_ok = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: +2\r\nConnection: close\r\n\r\nab",
        Duration::from_secs(3),
    );
    assert_status(&signed_ok, "HTTP/1.1 200 OK");
    assert!(
        signed_ok.contains("\r\n\r\nab"),
        "'+2' content-length should parse as 2 and echo the body:\n{signed_ok}"
    );

    // Bare-LF framing. The interpreter accepts "\n\n" as a header terminator but
    // then splits the header on "\r\n" only, so an LF-only request collapses into
    // a single "first line" that tokenizes to 5 parts -> 400. Native must reach
    // the same answer through the same structure, not serve the request.
    let lf_only = http_response(
        &address,
        "GET /ok HTTP/1.1\nHost: localhost\n\n",
        Duration::from_secs(3),
    );
    assert_status(&lf_only, "HTTP/1.1 400 Bad Request");

    // Conversely, a CRLF-framed request whose header line merely *contains* a bare
    // LF stays valid: the interpreter folds the LF into that header's value.
    let embedded_lf = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\nX-Extra: v\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&embedded_lf, "HTTP/1.1 200 OK");
}

// Idle read timeout (408) via the config-object form. The handler-timeout (504)
// path is intentionally NOT covered: native cannot preempt a compiled handler.
const IDLE_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.server({
        idle_timeout_ms: 150,
        read_header_timeout_ms: 500,
        max_connections: 8,
        max_active_requests: 2,
        max_pending_requests: 4
    })
    app.get("/ok", fn() {
        return http.text("ok")
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_idle_timeout_matches_interpreter() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("idle", IDLE_SOURCE, &address) else {
        return;
    };

    let ready = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&ready, "HTTP/1.1 200 OK");

    // Connect and send nothing: the server must time the idle connection out
    // with 408 within idle_timeout_ms.
    let idle = connect_with_retry(&address, Duration::from_secs(2));
    let idle_response = read_http_stream(idle, Duration::from_secs(2));
    assert_status(&idle_response, "HTTP/1.1 408 Request Timeout");
}

// 503 backpressure: occupy the single active slot and single pending slot, then
// a third connection must be rejected — identical to the interpreter scenario.
const LIMIT_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.server({
        idle_timeout_ms: 5000,
        read_header_timeout_ms: 10000,
        read_body_timeout_ms: 10000,
        write_timeout_ms: 500,
        max_connections: 2,
        max_active_requests: 1,
        max_pending_requests: 1
    })
    app.get("/ok", fn() {
        return http.text("ok")
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_backpressure_matches_interpreter() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("limit", LIMIT_SOURCE, &address) else {
        return;
    };

    let ready = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&ready, "HTTP/1.1 200 OK");

    // Occupy the active slot (send headers, keep the body pending).
    let mut active = connect_with_retry(&address, Duration::from_secs(2));
    active
        .write_all(
            b"POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
        )
        .expect("occupy active");
    thread::sleep(Duration::from_millis(300));
    // Occupy the pending slot.
    let mut pending = connect_with_retry(&address, Duration::from_secs(2));
    pending
        .write_all(
            b"POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
        )
        .expect("occupy pending");
    thread::sleep(Duration::from_millis(300));
    // The next connection is over the connection limit -> 503.
    let mut rejected = connect_with_retry(&address, Duration::from_secs(2));
    rejected
        .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write rejected probe");
    let _ = rejected.shutdown(Shutdown::Write);
    let rejected_response = read_http_stream(rejected, Duration::from_secs(2));
    assert_status(&rejected_response, "HTTP/1.1 503 Service Unavailable");

    drop(active);
    drop(pending);
    thread::sleep(Duration::from_millis(150));

    let recovered = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&recovered, "HTTP/1.1 200 OK");
}

// Request-target limit. `KU_HTTP_MAX_TARGET` (8192) is pinned to the
// interpreter's `MAX_REQUEST_TARGET_BYTES`; an over-long target is 414 and is
// never truncated. `long_a`/`long_b` share a 2100-byte prefix and differ only
// after it -- the old fixed `char target[2048]` truncated both to the same 2047
// bytes, so they collided on one route.
const TARGET_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    app.get("/short", fn() {
        return http.text("short-route")
    })
    app.get("/__A__1", fn() {
        return http.text("route-one")
    })
    app.get("/__A__2", fn() {
        return http.text("route-two")
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_long_request_target_is_bounded_not_truncated() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    // 2100 > the old 2047 cutoff, so a truncating server cannot tell the two apart.
    let shared = "a".repeat(2100);
    let source = TARGET_SOURCE.replace("__A__", &shared);
    let Some(_server) = spawn_native_server("target", &source, &address) else {
        return;
    };

    // Two long paths sharing a 2100-byte prefix must reach their own routes.
    let one = http_response(
        &address,
        &format!("GET /{shared}1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        Duration::from_secs(5),
    );
    assert_status(&one, "HTTP/1.1 200 OK");
    assert!(
        one.contains("\r\n\r\nroute-one"),
        "long path 1 must not collide with long path 2:\n{}",
        &one[..one.len().min(120)]
    );
    let two = http_response(
        &address,
        &format!("GET /{shared}2 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        Duration::from_secs(3),
    );
    assert!(
        two.contains("\r\n\r\nroute-two"),
        "long path 2 must not collide with long path 1:\n{}",
        &two[..two.len().min(120)]
    );

    // Exactly at the limit: allowed through routing (no route matches -> 404, and
    // specifically NOT 414).
    let at_limit = "b".repeat(8191); // + leading '/' = 8192 bytes of target
    let at = http_response(
        &address,
        &format!("GET /{at_limit} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        Duration::from_secs(3),
    );
    assert_status(&at, "HTTP/1.1 404 Not Found");

    // One byte over the limit -> 414, and it must not reach a handler.
    let over = "b".repeat(8192); // + leading '/' = 8193 bytes
    let too_long = http_response(
        &address,
        &format!("GET /{over} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        Duration::from_secs(3),
    );
    assert_status(&too_long, "HTTP/1.1 414 URI Too Long");

    // An over-long target must never truncate down onto an existing short route.
    let padded = "c".repeat(9000);
    let masked = http_response(
        &address,
        &format!("GET /short{padded} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        Duration::from_secs(3),
    );
    assert_status(&masked, "HTTP/1.1 414 URI Too Long");
    assert!(
        !masked.contains("short-route"),
        "an over-long target must not reach the /short handler:\n{masked}"
    );
}

// A header value longer than the response writer's stack scratch buffer must be
// written out intact. Regression test for a stack over-read: the writer used to
// format `location: <value>` through a fixed `char head[1024]` and then send
// snprintf's return value -- which is the length the output WOULD have had, not
// what was written -- so a long value both truncated AND sent adjacent stack
// bytes to the client. The interpreter builds an unbounded String and never
// truncates, so the value must survive byte-for-byte.
const LONG_REDIRECT_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    app.get("/r", fn(req) {
        return http.redirect("/dest?next=" + req.query.u)
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_long_header_value_is_not_truncated_or_padded_with_stack_bytes() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("long-redirect", LONG_REDIRECT_SOURCE, &address) else {
        return;
    };

    let filler = "A".repeat(1200);
    let response = http_response(
        &address,
        &format!("GET /r?u={filler} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        Duration::from_secs(5),
    );

    let expected = format!("location: /dest?next={filler}\r\n");
    assert!(
        response.contains(&expected),
        "long location header must round-trip intact (len {}), got:\n{}",
        expected.len(),
        &response[..response.len().min(200)]
    );
    // Every byte must be something we wrote: a leaked stack byte shows up as a
    // control or non-ASCII character outside the CR/LF used by the framing.
    let leaked: Vec<u8> = response
        .bytes()
        .filter(|b| *b < 9 || (*b > 13 && *b < 32) || *b > 126)
        .collect();
    assert!(
        leaked.is_empty(),
        "response contains {} non-printable byte(s) -- stack memory leaked into the wire",
        leaked.len()
    );
}

// 504 handler timeout is a documented native limitation (a compiled
// compute-bound handler cannot be preempted). This test does not assert 504;
// it only pins the guarantee that a slow handler occupying one worker does not
// take the whole server down — other requests on a free worker still succeed.
const SLOW_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.server({
        handler_timeout_ms: 100,
        read_header_timeout_ms: 2000,
        max_connections: 16,
        max_active_requests: 4,
        max_pending_requests: 8
    })
    app.get("/slow", fn() {
        while (true) {
        }
        return http.text("unreachable")
    })
    app.get("/ok", fn() {
        return http.text("ok")
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_slow_handler_stays_up() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("slow", SLOW_SOURCE, &address) else {
        return;
    };

    let ready = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&ready, "HTTP/1.1 200 OK");

    // Fire a slow request on its own connection; native cannot 504 it, so we do
    // not read its response (it never returns). It occupies one of four workers.
    let mut slow = connect_with_retry(&address, Duration::from_secs(2));
    slow.write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("send slow");
    let _ = slow.shutdown(Shutdown::Write);
    thread::sleep(Duration::from_millis(200));

    // The server must still serve /ok on a free worker.
    let ok = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&ok, "HTTP/1.1 200 OK");
    drop(slow);
}

// Stage 8e item-二: an HTTP program must compile and link regardless of how it
// uses Result. `app.listen(...)` is itself typed `null!`, so every HTTP program
// genuinely uses Result<null> -- the point here is that no *unused* Result payload
// is forced, and that HTTP + a variety of Result payloads all still build.
#[test]
fn native_http_compiles_across_result_usage() {
    // No `Result` written by the user at all (main has no `!` return).
    let no_result = r#"
import "std.http"
fn main() {
    app = http.service()
    app.get("/", fn() {
        return http.text("hi")
    })
    app.listen("127.0.0.1:18991")
}
"#;
    // main(): null! with `?` on listen.
    let null_bang = r#"
import "std.http"
fn main(): null! {
    app = http.service()
    app.get("/", fn() {
        return http.text("hi")
    })
    app.listen("127.0.0.1:18992")?
    return ok(null)
}
"#;
    // A str! payload used via `?` at the top level, alongside the HTTP service.
    let str_payload = r#"
import "std.http"
fn load(bad: bool): str! {
    if (bad) {
        fail { domain: "app", code: "x", message: "y" }
    }
    return ok("loaded")
}
fn main(): null! {
    greeting = load(false)?
    println(greeting)
    app = http.service()
    app.get("/ok", fn() {
        return ok(http.text("h"))
    })
    app.get("/plain", fn() {
        return http.text("p")
    })
    app.listen("127.0.0.1:18993")
    return ok(null)
}
"#;
    for (name, src) in [
        ("no_result", no_result),
        ("null_bang", null_bang),
        ("str_payload", str_payload),
    ] {
        let Some(built) = native_builds(name, src) else {
            return; // no compiler -> skip all
        };
        assert!(built, "HTTP program '{name}' must compile and link natively");
    }
}
