use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
struct RunResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

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
    let candidates = [
        target_dir.join("debug").join(exe),
        target_dir.join("release").join(exe),
        repo_root().join("target").join("debug").join(exe),
        repo_root().join("target").join("release").join(exe),
    ];

    candidates
        .iter()
        .find(|path| path.exists())
        .cloned()
        .expect("ku binary not found; set KU_BIN or build the ku binary first")
}

fn example(name: &str) -> PathBuf {
    repo_root().join("examples").join(name)
}

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-cli-{name}-{}.ku",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should work")
            .as_nanos()
    ))
}

fn unused_local_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind temporary port");
    let address = listener
        .local_addr()
        .expect("temporary listener should have an address");
    drop(listener);
    address.to_string()
}

fn connect_with_retry(address: &str, timeout: Duration) -> TcpStream {
    let started = Instant::now();
    let mut last_error = None;
    while started.elapsed() < timeout {
        match TcpStream::connect(address) {
            Ok(stream) => return stream,
            Err(err) => last_error = Some(err),
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "http server did not accept connections within {timeout:?}: {}",
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "no connection attempt".to_string())
    );
}

fn read_http_stream(mut stream: TcpStream, timeout: Duration) -> String {
    stream
        .set_read_timeout(Some(timeout))
        .expect("set http test read timeout");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&buffer[..read]),
            Err(_) if !response.is_empty() => break,
            Err(err) => panic!("read http test response: {err}"),
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}

fn run_with_timeout(bin: &Path, args: &[&str], timeout: Duration) -> RunResult {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(repo_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn ku binary");

    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .expect("failed to poll ku process")
            .is_some()
        {
            let output = child
                .wait_with_output()
                .expect("failed to collect ku output");
            return RunResult {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: false,
            };
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed-out ku output");
            return RunResult {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: true,
            };
        }

        thread::sleep(Duration::from_millis(20));
    }
}

fn run_ku(args: &[&str]) -> RunResult {
    let bin = ku_binary();
    run_with_timeout(&bin, args, Duration::from_secs(2))
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn wait_for_http_response(
    address: &str,
    request: &str,
    timeout: Duration,
) -> Result<String, String> {
    let started = Instant::now();
    let mut last_error = String::new();
    while started.elapsed() < timeout {
        match TcpStream::connect(address) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .expect("set read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_millis(500)))
                    .expect("set write timeout");
                if let Err(err) = stream.write_all(request.as_bytes()) {
                    last_error = err.to_string();
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
                        Err(err) if !response.is_empty() => {
                            last_error = err.to_string();
                            break;
                        }
                        Err(err) => {
                            last_error = err.to_string();
                            break;
                        }
                    }
                }
                if !response.is_empty() {
                    return Ok(String::from_utf8_lossy(&response).into_owned());
                }
            }
            Err(err) => {
                last_error = err.to_string();
            }
        }
        thread::sleep(Duration::from_millis(30));
    }
    Err(format!(
        "http server did not respond within {timeout:?}: {last_error}"
    ))
}

fn assert_http_status(response: &str, status: &str) {
    assert!(
        response.starts_with(status),
        "expected {status}, got response:\n{response}"
    );
}

fn http_response_or_stop(
    server: &mut Option<KuServerProcess>,
    address: &str,
    request: &str,
) -> String {
    match wait_for_http_response(address, request, Duration::from_secs(3)) {
        Ok(response) => response,
        Err(message) => {
            let output = server.take().expect("server should exist").stop();
            panic!(
                "{message}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

struct KuServerProcess {
    child: Option<Child>,
    source_path: PathBuf,
}

impl KuServerProcess {
    fn spawn(source: String) -> Self {
        let path = unique_temp_path("http-service");
        fs::write(&path, source).expect("write temporary Ku source");
        let path_text = path_arg(&path);
        let child = Command::new(ku_binary())
            .args(["run", &path_text])
            .current_dir(repo_root())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn ku http server");
        Self {
            child: Some(child),
            source_path: path,
        }
    }

    fn stop(mut self) -> Output {
        let mut child = self.child.take().expect("server process should exist");
        let _ = child.kill();
        let output = child
            .wait_with_output()
            .expect("failed to collect ku http server output");
        let _ = fs::remove_file(&self.source_path);
        output
    }
}

impl Drop for KuServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(&self.source_path);
    }
}

#[test]
fn run_hello_prints_expected_greeting() {
    let path = path_arg(&example("hello.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "hello example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "hello example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.contains("Hello Ku"),
        "hello output should contain greeting\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_print_does_not_append_newline_and_println_does() {
    let path = unique_temp_path("print-newline");
    fs::write(
        &path,
        r#"
fn main() {
    print("A")
    print("B")
    println("C")
}
"#,
    )
    .expect("write print semantics source");

    let path_arg = path_arg(&path);
    let result = run_ku(&["run", &path_arg]);
    let _ = fs::remove_file(&path);

    assert_eq!(
        result.code,
        Some(0),
        "print semantics program failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert_eq!(
        result.stdout, "ABC\n",
        "print should not add a newline, println should\nstderr:\n{}",
        result.stderr
    );
}

#[test]
fn run_http_service_handles_local_request() {
    let _guard = HTTP_TEST_LOCK.lock().expect("http test lock poisoned");
    let address = unused_local_address();
    let source = r#"
import "std.http"

fn exact(req, res) {
    return http.text("exact")
}

fn main(): null! {
    app = http.service()
    app.get("/user/me", exact)
    app.get("/fn", fn(req, res) {
        return http.text(req.path.clone())
    })
    app.get("/user/{id}", (req, res) => {
        return http.text(req.params.id + ":" + req.query.q + ":" + req.headers.host)
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &address);
    let mut server = Some(KuServerProcess::spawn(source));

    let exact = http_response_or_stop(
        &mut server,
        &address,
        "GET /user/me HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        exact.contains("\r\n\r\nexact"),
        "compiled router should prefer exact route:\n{exact}"
    );

    let anonymous = http_response_or_stop(
        &mut server,
        &address,
        "GET /fn HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        anonymous.contains("\r\n\r\n/fn"),
        "anonymous fn handler should run:\n{anonymous}"
    );

    let request = "GET /user/42?q=ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_response_or_stop(&mut server, &address, request);

    let output = server.take().expect("server should exist").stop();

    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected http response:\n{response}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        response.contains("\r\n\r\n42:ok:localhost"),
        "unexpected http response body:\n{response}"
    );
}

#[test]
fn run_http_service_handles_error_statuses_and_limits() {
    let _guard = HTTP_TEST_LOCK.lock().expect("http test lock poisoned");
    let address = unused_local_address();
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    app.max_body_bytes = 4
    app.max_header_bytes = 128
    app.read_header_timeout_ms = 500
    app.read_body_timeout_ms = 500
    app.write_timeout_ms = 500
    app.get("/ok", (req, res) => http.text("ok"))
    app.post("/echo", (req, res) => http.text(req.body))
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &address);
    let mut server = Some(KuServerProcess::spawn(source));

    let missing = http_response_or_stop(
        &mut server,
        &address,
        "GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&missing, "HTTP/1.1 404 Not Found");

    let wrong_method = http_response_or_stop(
        &mut server,
        &address,
        "POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&wrong_method, "HTTP/1.1 405 Method Not Allowed");

    let too_large = http_response_or_stop(
        &mut server,
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345",
    );
    assert_http_status(&too_large, "HTTP/1.1 413 Content Too Large");

    let bad_header = http_response_or_stop(
        &mut server,
        &address,
        "GET /ok HTTP/1.1\r\nBrokenHeader\r\n\r\n",
    );
    assert_http_status(&bad_header, "HTTP/1.1 400 Bad Request");

    let too_large_header = http_response_or_stop(
        &mut server,
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nX-Long: 12345678901234567890123456789012345678901234567890123456789012345678901234567890\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(
        &too_large_header,
        "HTTP/1.1 431 Request Header Fields Too Large",
    );

    let _ = server.take().expect("server should exist").stop();
}

#[test]
fn run_http_service_enforces_idle_and_handler_timeouts() {
    let _guard = HTTP_TEST_LOCK.lock().expect("http test lock poisoned");
    let address = unused_local_address();
    let source = r#"
import "std.http"

fn main(): null! {
    app = http.server({
        idle_timeout_ms: 150,
        handler_timeout_ms: 100,
        read_header_timeout_ms: 500,
        max_connections: 8,
        max_active_requests: 2,
        max_pending_requests: 4
    })
    app.get("/slow", (req, res) => {
        while (true) {
        }
        return http.text("unreachable")
    })
    app.get("/ok", (req, res) => http.text("ok"))
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &address);
    let mut server = Some(KuServerProcess::spawn(source));

    let ready = http_response_or_stop(
        &mut server,
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&ready, "HTTP/1.1 200 OK");

    let idle = connect_with_retry(&address, Duration::from_secs(2));
    let idle_response = read_http_stream(idle, Duration::from_secs(2));
    assert_http_status(&idle_response, "HTTP/1.1 408 Request Timeout");

    let slow = http_response_or_stop(
        &mut server,
        &address,
        "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&slow, "HTTP/1.1 504 Gateway Timeout");

    let recovered = http_response_or_stop(
        &mut server,
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&recovered, "HTTP/1.1 200 OK");
    let _ = server.take().expect("server should exist").stop();
}

#[test]
fn run_http_service_bounds_active_pending_and_connections() {
    let _guard = HTTP_TEST_LOCK.lock().expect("http test lock poisoned");
    let address = unused_local_address();
    let source = r#"
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
    app.get("/ok", (req, res) => http.text("ok"))
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &address);
    let mut server = Some(KuServerProcess::spawn(source));

    let ready = http_response_or_stop(
        &mut server,
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&ready, "HTTP/1.1 200 OK");

    let mut active = connect_with_retry(&address, Duration::from_secs(2));
    active
        .write_all(
            b"POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
        )
        .expect("occupy active request slot");
    thread::sleep(Duration::from_millis(300));
    let mut pending = connect_with_retry(&address, Duration::from_secs(2));
    pending
        .write_all(
            b"POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
        )
        .expect("occupy pending request slot");
    thread::sleep(Duration::from_millis(300));
    let mut rejected = connect_with_retry(&address, Duration::from_secs(2));
    rejected
        .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write rejected probe");
    let _ = rejected.shutdown(Shutdown::Write);
    let rejected_response = read_http_stream(rejected, Duration::from_secs(2));
    assert_http_status(&rejected_response, "HTTP/1.1 503 Service Unavailable");

    drop(active);
    drop(pending);
    thread::sleep(Duration::from_millis(100));

    let recovered = http_response_or_stop(
        &mut server,
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_http_status(&recovered, "HTTP/1.1 200 OK");
    let _ = server.take().expect("server should exist").stop();
}

#[test]
fn run_fib_prints_tenth_fibonacci_number() {
    let path = path_arg(&example("fib.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "fib example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "fib example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.lines().any(|line| line.trim() == "55"),
        "fib(10) should print 55\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_function_prints_return_value() {
    let path = path_arg(&example("function.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "function example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "function example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.lines().any(|line| line.trim() == "30"),
        "add(10, 20) should print 30\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_loop_terminates_and_prints_expected_sequence() {
    let path = path_arg(&example("loop.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "loop example did not terminate within timeout\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "loop example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );

    let values: Vec<_> = result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        values,
        ["0", "1", "2", "3", "4"],
        "loop should print 0 through 4 once\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn check_error_reports_clear_type_error() {
    let path = path_arg(&example("error.ku"));
    let result = run_ku(&["check", &path]);

    assert!(
        !result.timed_out,
        "check error example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_ne!(
        result.code,
        Some(0),
        "type error should fail check\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );

    let combined = format!("{}\n{}", result.stdout, result.stderr).to_lowercase();
    assert!(
        combined.contains("type") || combined.contains("类型") || combined.contains("error"),
        "error output should clearly identify the failure\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}
