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
//!   * 504 cooperative handler timeout                              — asserted
//!
//! When no C compiler is present every test skips cleanly instead of failing.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_c_harness;
use native_c_harness::bounded_process;

use std::env;
use std::fs;
#[cfg(not(unix))]
use std::io::Read;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bounded_process::service::{spawn_bounded_service, BoundedService};
use bounded_process::{run_bounded, BoundedOutput, OutputLimits};

// HTTP native tests bind real ports; serialize them so concurrent servers do
// not fight over the admission-control probes.
static HTTP_TEST_LOCK: Mutex<()> = Mutex::new(());
const MAX_HTTP_TEST_RESPONSE_BYTES: usize = 1024 * 1024;
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
    spawn_native_server_inner(name, source, address, false, None)
}

// Only the lexical positive currently opts into checked completion. Other
// HTTP fixtures retain their old watchdog behavior and are not covered by its
// stderr/exit receipt. This does not add a graceful shutdown API to Ku.
fn spawn_observed_native_server(
    name: &str,
    source: &str,
    address: &str,
) -> Option<NativeHttpServer> {
    spawn_native_server_inner(name, source, address, true, None)
}

fn spawn_finite_native_server(
    name: &str,
    source: &str,
    address: &str,
    max_requests: usize,
) -> Option<NativeHttpServer> {
    spawn_native_server_inner(name, source, address, false, Some(max_requests))
}

fn spawn_native_server_inner(
    name: &str,
    source: &str,
    address: &str,
    observed: bool,
    max_requests: Option<usize>,
) -> Option<NativeHttpServer> {
    spawn_native_server_settings(name, source, address, observed, max_requests, &[])
}

fn spawn_native_server_settings(
    name: &str,
    source: &str,
    address: &str,
    observed: bool,
    max_requests: Option<usize>,
    settings: &[(&str, &str)],
) -> Option<NativeHttpServer> {
    let dir = unique_temp_dir(name);
    let entry = "server.ku";
    fs::write(dir.join(entry), source.replace("__ADDRESS__", address)).expect("write ku source");
    let out = exe_name("server");
    let mut command = Command::new(ku_binary());
    if let Some(required) = env::var_os("KU_NATIVE_REQUIRE_TSAN") {
        assert_eq!(required, "1", "KU_NATIVE_REQUIRE_TSAN must be exactly 1");
        let compiler = env::var("KU_CC").expect("HTTP TSan gate requires KU_CC");
        assert!(
            !compiler.is_empty() && !compiler.chars().any(char::is_whitespace),
            "HTTP TSan gate requires one compiler executable without flags"
        );
        // The CLI invokes the C compiler itself, whereas the controlled C
        // harness adds these flags directly. Both must instrument actual C.
        command.env(
            "KU_CC",
            format!("{compiler} -fsanitize=thread -O1 -fno-omit-frame-pointer -g"),
        );
    }
    command
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &out]);
    let build = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native HTTP server build was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    if !build.status.success() {
        if combined.contains("C compiler not found") {
            eprintln!("skip: no C compiler available for native HTTP e2e test");
            fs::remove_dir_all(&dir).ok();
            return None;
        }
        fs::remove_dir_all(&dir).ok();
        panic!("ku build --native failed for {name}:\n{combined}");
    }
    let c_source = combined
        .lines()
        .find_map(|line| line.strip_prefix("native c ok: "))
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| panic!("native HTTP build did not report C output:\n{combined}"));
    let exe = dir.join(&out);
    if observed {
        let mut command = Command::new(&exe);
        command.current_dir(&dir);
        command.envs(settings.iter().copied());
        let service = match spawn_bounded_service(command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS) {
            Ok(service) => service,
            Err(error) => {
                fs::remove_dir_all(&dir).ok();
                panic!("spawn native HTTP supervisor: {error}");
            }
        };
        return Some(NativeHttpServer {
            child: None,
            service: Some(service),
            dir: Some(dir),
            c_source,
        });
    }
    let mut command = Command::new(&exe);
    command.current_dir(&dir);
    command.envs(settings.iter().copied());
    if let Some(max_requests) = max_requests {
        command.env("KU_HTTP_MAX_REQUESTS", max_requests.to_string());
    }
    let child = match command
        // The long-lived server is supervised by `NativeServerWatchdog` and
        // never consumes process output. Null streams prevent an unbounded
        // pipe backlog from stalling the server under a diagnostic flood.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            fs::remove_dir_all(&dir).ok();
            panic!("spawn native http server: {error}");
        }
    };
    Some(NativeHttpServer {
        child: Some(child),
        service: None,
        dir: Some(dir),
        c_source,
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
    let mut command = Command::new(ku_binary());
    command
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &out]);
    let build = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native HTTP compile check was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    if !build.status.success() && combined.contains("C compiler not found") {
        eprintln!("skip: no C compiler available");
        fs::remove_dir_all(&dir).ok();
        return None;
    }
    let built = build.status.success() && dir.join(&out).exists();
    if !built {
        eprintln!("build failed for {name}:\n{combined}");
    }
    fs::remove_dir_all(&dir).ok();
    Some(built)
}

/// Compile and run a short native program that is expected to terminate on its
/// own. Returns `None` only when the native C compiler is unavailable.
fn native_run_output(name: &str, source: &str) -> Option<BoundedOutput> {
    let dir = unique_temp_dir(name);
    let entry = "prog.ku";
    fs::write(dir.join(entry), source).expect("write ku source");
    let out = exe_name("prog");
    let mut command = Command::new(ku_binary());
    command
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &out]);
    let build = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("short native HTTP build was not bounded: {error}"));
    let build_text = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    if !build.status.success() && build_text.contains("C compiler not found") {
        eprintln!("skip: no C compiler available");
        fs::remove_dir_all(&dir).ok();
        return None;
    }
    assert!(
        build.status.success(),
        "ku build --native failed for {name}:\n{build_text}"
    );
    let executable = dir.join(&out);
    let mut command = Command::new(&executable);
    command.current_dir(&dir);
    let output =
        run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!(
                "short native HTTP program {} was not bounded: {error}",
                executable.display()
            )
        });
    fs::remove_dir_all(&dir).ok();
    Some(output)
}

fn interpreter_run_output(name: &str, source: &str) -> BoundedOutput {
    let dir = unique_temp_dir(name);
    let entry = "prog.ku";
    fs::write(dir.join(entry), source).expect("write ku source");
    let mut command = Command::new(ku_binary());
    command.current_dir(&dir).arg(entry);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("interpreted HTTP program was not bounded: {error}"));
    fs::remove_dir_all(&dir).ok();
    output
}

struct NativeHttpServer {
    child: Option<Child>,
    service: Option<BoundedService>,
    dir: Option<PathBuf>,
    c_source: PathBuf,
}

struct NativeServerWatchdog {
    cancel: Option<mpsc::Sender<()>>,
    timed_out: Arc<AtomicBool>,
    natural_exit: Arc<Mutex<Option<ExitStatus>>>,
    worker: Option<thread::JoinHandle<()>>,
    dir: Option<PathBuf>,
}

impl NativeServerWatchdog {
    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }

    fn wait_for_natural_exit(&self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = *self.natural_exit.lock().expect("native exit receipt") {
                return status;
            }
            assert!(
                !self.timed_out(),
                "native server required its external watchdog"
            );
            assert!(
                Instant::now() < deadline,
                "finite native server did not exit naturally"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for NativeServerWatchdog {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(dir) = self.dir.take() {
            fs::remove_dir_all(dir).ok();
        }
    }
}

impl NativeHttpServer {
    fn finish_checked(mut self) {
        let receipt = self
            .service
            .take()
            .expect("observed native service")
            .finish_checked()
            .unwrap_or_else(|error| panic!("native HTTP checked stop failed: {error}"));
        assert!(receipt.output.stderr.is_empty());
        assert!(
            !receipt.output.status.success(),
            "forced stop is not natural success"
        );
    }

    fn dir(&self) -> &std::path::Path {
        self.dir.as_deref().expect("native server directory")
    }

    fn c_source(&self) -> &std::path::Path {
        &self.c_source
    }

    /// Transfer ownership of the external server process to a watchdog. If the
    /// request path under test wedges, the watchdog kills and reaps the process,
    /// which closes its sockets and guarantees that the Rust test can unwind.
    fn arm_kill_watchdog(&mut self, timeout: Duration) -> NativeServerWatchdog {
        let mut child = self.child.take().expect("native server process");
        let dir = self.dir.take().expect("native server directory");
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let timed_out = Arc::new(AtomicBool::new(false));
        let worker_timed_out = Arc::clone(&timed_out);
        let natural_exit = Arc::new(Mutex::new(None));
        let worker_natural_exit = Arc::clone(&natural_exit);
        let worker = thread::spawn(move || {
            let started = Instant::now();
            loop {
                if let Ok(Some(status)) = child.try_wait() {
                    *worker_natural_exit
                        .lock()
                        .expect("record native exit receipt") = Some(status);
                    return;
                }

                let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                    worker_timed_out.store(true, Ordering::Release);
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                };
                match cancel_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
        NativeServerWatchdog {
            cancel: Some(cancel_tx),
            timed_out,
            natural_exit,
            worker: Some(worker),
            dir: Some(dir),
        }
    }
}

impl Drop for NativeHttpServer {
    fn drop(&mut self) {
        // Drop the bounded service before attempting artifact cleanup, also on
        // request-assertion panic. Only finish_checked validates its receipt.
        drop(self.service.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(dir) = self.dir.take() {
            fs::remove_dir_all(dir).ok();
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

fn read_http_stream_bytes_until(
    stream: &mut TcpStream,
    deadline: Instant,
) -> std::io::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut buffer = [0u8; 1024];
    let mut header_parsed = false;
    let mut expected_total = None;
    loop {
        // A per-read timeout is retryable, but the shared absolute deadline is
        // not. Keep this check outside the retrying read-result match.
        http_test_read_timeout(deadline)?;
        match read_http_chunk_until(stream, &mut buffer, deadline) {
            Ok(0) => return Ok(response),
            Ok(read) => {
                let previous_len = response.len();
                if response.len().saturating_add(read) > MAX_HTTP_TEST_RESPONSE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("HTTP test response exceeded {MAX_HTTP_TEST_RESPONSE_BYTES} bytes"),
                    ));
                }
                response.extend_from_slice(&buffer[..read]);
                if !header_parsed {
                    let search_start = previous_len.saturating_sub(3);
                    if let Some(relative) = response[search_start..]
                        .windows(4)
                        .position(|part| part == b"\r\n\r\n")
                    {
                        let header_offset = search_start + relative;
                        expected_total = http_response_expected_total(&response, header_offset)?;
                        header_parsed = true;
                    }
                }
                if expected_total.is_some_and(|total| response.len() >= total) {
                    return Ok(response);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn http_test_read_timeout(deadline: Instant) -> std::io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = remaining.min(Duration::from_millis(200));
    if timeout < Duration::from_millis(1) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "HTTP test response exceeded its absolute deadline",
        ));
    }
    Ok(timeout)
}

#[cfg(not(unix))]
fn read_http_chunk_until(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    stream
        .set_read_timeout(Some(http_test_read_timeout(deadline)?))
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("HTTP test read timeout setup: {error}"),
            )
        })?;
    stream.read(buffer).map_err(|error| {
        std::io::Error::new(error.kind(), format!("HTTP test response read: {error}"))
    })
}

#[cfg(unix)]
fn poll_http_response(fd: std::os::fd::RawFd, timeout: Duration) -> std::io::Result<libc::c_short> {
    // Floor rather than round up: waiting never gains time beyond the existing
    // absolute budget. The caller rejects remaining durations below one ms.
    let timeout_ms = i32::try_from(timeout.as_millis()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "HTTP test poll timeout overflow",
        )
    })?;
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized descriptor is live for the entire call. poll
    // neither owns nor closes fd; the caller retains its TcpStream.
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        // EINTR is retried against the absolute deadline. Other poll errors,
        // including allocation-related EAGAIN, must not become busy retries.
        let kind = if error.kind() == std::io::ErrorKind::Interrupted {
            error.kind()
        } else {
            std::io::ErrorKind::Other
        };
        return Err(std::io::Error::new(
            kind,
            format!("HTTP test readiness poll: {error}"),
        ));
    }
    if ready == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "HTTP test readiness wait expired",
        ));
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "HTTP test readiness poll reported an invalid descriptor",
        ));
    }
    if descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
        return Err(std::io::Error::other(
            "HTTP test readiness poll returned no readable event",
        ));
    }
    Ok(descriptor.revents)
}

#[cfg(unix)]
fn read_http_chunk_until(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;

    // Darwin rejects setsockopt once both halves are shut down, even when the
    // receive buffer still contains an HTTP response. Readiness + per-call
    // nonblocking recv needs no socket-option mutation on a disconnected peer.
    let events = poll_http_response(stream.as_raw_fd(), http_test_read_timeout(deadline)?)?;
    http_test_read_timeout(deadline)?;
    // SAFETY: the stream remains owned, buffer is exclusively borrowed and its
    // exact capacity is passed to recv. MSG_DONTWAIT also bounds spurious wakes.
    let read = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if read < 0 {
        let error = std::io::Error::last_os_error();
        let kind = if error.kind() == std::io::ErrorKind::WouldBlock
            && events & (libc::POLLHUP | libc::POLLERR) != 0
        {
            // A sticky terminal readiness event without readable data or EOF
            // must fail, not spin until the deadline. Preserve the OS evidence.
            std::io::ErrorKind::Other
        } else {
            error.kind()
        };
        return Err(std::io::Error::new(
            kind,
            format!("HTTP test response recv: {error}"),
        ));
    }
    usize::try_from(read)
        .ok()
        .filter(|read| *read <= buffer.len())
        .ok_or_else(|| std::io::Error::other("HTTP test response recv returned an invalid length"))
}

fn http_response_expected_total(
    response: &[u8],
    header_offset: usize,
) -> std::io::Result<Option<usize>> {
    let header_end = header_offset.checked_add(4).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "HTTP test response header length overflowed",
        )
    })?;
    let header = std::str::from_utf8(&response[..header_offset]).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "HTTP test response header is not UTF-8",
        )
    })?;
    let mut lines = header.split("\r\n");
    let status = lines.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "HTTP test response has no status line",
        )
    })?;
    let status_code = status
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP test response has an invalid status line",
            )
        })?;
    if matches!(status_code, 204 | 304) {
        return Ok(Some(header_end));
    }
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP test response has an invalid header field",
            ));
        };
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP test response has duplicate Content-Length",
                ));
            }
            content_length = Some(value.trim().parse::<usize>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP test response has an invalid Content-Length",
                )
            })?);
        }
    }
    content_length
        .map(|length| {
            header_end.checked_add(length).map(Some).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP test response body length overflowed",
                )
            })
        })
        .unwrap_or(Ok(None))
}

fn read_http_stream(mut stream: TcpStream, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let response = read_http_stream_bytes_until(&mut stream, deadline)
        .unwrap_or_else(|error| panic!("failed to read bounded HTTP response: {error}"));
    String::from_utf8_lossy(&response).into_owned()
}

/// Connect (retrying until the server is up), send one complete HTTP request,
/// then read the bounded response. Keep the write side open while reading:
/// HTTP request framing does not require a half-close, and it can combine with
/// peer shutdown to prohibit later socket-option changes on macOS.
fn http_response(address: &str, request: &str, timeout: Duration) -> String {
    http_response_bytes(address, request.as_bytes(), timeout)
}

fn http_response_bytes(address: &str, request: &[u8], timeout: Duration) -> String {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match TcpStream::connect(address) {
            Ok(mut stream) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining < Duration::from_millis(1) {
                    break;
                }
                stream
                    .set_read_timeout(Some(remaining.min(Duration::from_millis(700))))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(remaining.min(Duration::from_millis(700))))
                    .expect("write timeout");
                if stream.write_all(request).is_err() {
                    thread::sleep(Duration::from_millis(30));
                    continue;
                }
                match read_http_stream_bytes_until(&mut stream, deadline) {
                    Ok(response) if !response.is_empty() => {
                        return String::from_utf8_lossy(&response).into_owned();
                    }
                    Ok(_) => last = "empty response".to_string(),
                    Err(error) => last = error.to_string(),
                }
            }
            Err(err) => last = err.to_string(),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::sleep(remaining.min(Duration::from_millis(30)));
        }
    }
    panic!(
        "native http server did not respond within {timeout:?} (elapsed {:?}): {last}",
        started.elapsed()
    );
}

#[test]
fn native_http_test_reader_bounds_drip_time_and_response_size() {
    let drip_listener = TcpListener::bind("127.0.0.1:0").expect("bind drip listener");
    let drip_address = drip_listener.local_addr().expect("drip listener address");
    let drip_server = thread::spawn(move || {
        let (mut stream, _) = drip_listener.accept().expect("accept drip client");
        stream
            .set_write_timeout(Some(Duration::from_millis(200)))
            .expect("bound drip writes");
        for _ in 0..20 {
            if stream.write_all(b"x").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut drip_client = TcpStream::connect(drip_address).expect("connect drip client");
    let started = Instant::now();
    let error =
        read_http_stream_bytes_until(&mut drip_client, started + Duration::from_millis(120))
            .expect_err("continuous drip output must not extend the absolute deadline");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "drip response deadline was not enforced"
    );
    drop(drip_client);
    drip_server.join().expect("join drip server");

    let size_listener = TcpListener::bind("127.0.0.1:0").expect("bind size listener");
    let size_address = size_listener.local_addr().expect("size listener address");
    let size_server = thread::spawn(move || {
        let (mut stream, _) = size_listener.accept().expect("accept size client");
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .expect("bound size writes");
        let oversized = vec![b'x'; MAX_HTTP_TEST_RESPONSE_BYTES + 1];
        let _ = stream.write_all(&oversized);
    });
    let mut size_client = TcpStream::connect(size_address).expect("connect size client");
    let error =
        read_http_stream_bytes_until(&mut size_client, Instant::now() + Duration::from_secs(2))
            .expect_err("oversized test response must fail before unbounded allocation");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    drop(size_client);
    size_server.join().expect("join size server");
}

#[cfg(unix)]
#[test]
fn native_http_test_reader_drains_response_after_peer_shutdown() {
    use std::os::fd::AsRawFd;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind shutdown fixture");
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect fixture");
    let (mut peer, _) = listener.accept().expect("accept shutdown fixture");
    peer.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("finish request side");
    let expected =
        b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbye";
    peer.write_all(expected)
        .expect("buffer complete timeout response");
    peer.shutdown(std::net::Shutdown::Write)
        .expect("finish response side");
    drop(peer);

    // Wait for the actual TCP shutdown event, not a scheduling delay, without
    // consuming the buffered response. Darwin's poll(events=0) does not install
    // a read filter, so use its EOF event with a low-water mark above our data.
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::{FromRawFd, OwnedFd};
        // SAFETY: kqueue has no arguments and returns a fresh owned descriptor.
        let raw_queue = unsafe { libc::kqueue() };
        assert!(raw_queue >= 0, "create shutdown event queue");
        // SAFETY: the successful kqueue call transfers this sole fd ownership.
        let queue = unsafe { OwnedFd::from_raw_fd(raw_queue) };
        let change = libc::kevent {
            ident: client.as_raw_fd() as usize,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_LOWAT,
            data: (expected.len() + 1) as isize,
            udata: std::ptr::null_mut(),
        };
        let mut event = libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let wait = libc::timespec {
            tv_sec: 2,
            tv_nsec: 0,
        };
        // SAFETY: all descriptors and single-element event buffers remain live;
        // the timeout bounds waiting and EOF is reported even with unread data.
        let ready = unsafe { libc::kevent(queue.as_raw_fd(), &change, 1, &mut event, 1, &wait) };
        assert_eq!(ready, 1, "peer shutdown was not observed");
        let flags = event.flags;
        assert_eq!(
            flags & libc::EV_ERROR,
            0,
            "shutdown event registration failed"
        );
        assert_ne!(flags & libc::EV_EOF, 0, "expected peer EOF");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut descriptor = libc::pollfd {
            fd: client.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        // SAFETY: descriptor and the owning client remain live; the wait is bounded.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 2000) };
        assert_eq!(ready, 1, "peer shutdown was not observed");
        assert_ne!(
            descriptor.revents & libc::POLLHUP,
            0,
            "expected peer hangup"
        );
    }

    #[cfg(target_os = "macos")]
    {
        // This proves the original reader's failure point on Darwin rather
        // than accepting EINVAL as a successful or empty HTTP response.
        let error = client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect_err("Darwin rejects socket options after both halves close");
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }
    let actual = read_http_stream_bytes_until(&mut client, Instant::now() + Duration::from_secs(2))
        .expect("read buffered HTTP after peer shutdown without changing socket options");
    assert_eq!(actual, expected);
}

#[test]
fn native_http_test_reader_rejects_expired_and_submillisecond_deadlines() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind deadline fixture");
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect fixture");
    let (_peer, _) = listener.accept().expect("accept deadline fixture");
    for remaining in [Duration::ZERO, Duration::from_micros(500)] {
        let error = read_http_stream_bytes_until(&mut client, Instant::now() + remaining)
            .expect_err("less than one ms must never become an unbounded socket wait");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
}

#[cfg(unix)]
#[test]
fn native_http_test_reader_rejects_invalid_poll_descriptor() {
    let error = poll_http_response(i32::MAX, Duration::from_millis(1))
        .expect_err("POLLNVAL must fail rather than become a readiness retry");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("invalid descriptor"));
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
fn native_http_bind_fails_early_as_interpreter_only() {
    let dir = unique_temp_dir("bind-interpreter-only");
    let entry = "server.ku";
    fs::write(
        dir.join(entry),
        r#"import "std.http"
fn main(): null! {
    app = http.service()
    listener = app.bind(":0")?
    listener.close()?
    return ok(null)
}
"#,
    )
    .expect("write ku source");
    let out = exe_name("server");
    let mut command = Command::new(ku_binary());
    command
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &out]);
    let build =
        run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!("native HTTP bind rejection build was not bounded: {error}")
        });
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    fs::remove_dir_all(&dir).ok();
    assert!(!build.status.success(), "native bind unexpectedly built");
    assert!(
        combined.contains("bind/listener run/close are interpreter-only"),
        "native bind should fail with a capability-specific diagnostic:\n{combined}"
    );
}

#[test]
fn native_http_routing_matches_interpreter() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(server) = spawn_native_server("routing", ROUTING_SOURCE, &address) else {
        return;
    };
    let generated = fs::read_to_string(server.c_source()).expect("read generated native HTTP C");
    assert!(generated.contains("#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1"));
    assert!(generated.contains("typedef SOCKET KuHttpSocket;"));
    assert!(generated.contains("typedef int KuHttpSocket;"));
    assert!(generated.contains("poll(items, (nfds_t)count, timeout_ms)"));
    assert!(generated.contains("WSAPoll(items, (ULONG)count, timeout_ms)"));
    assert!(generated.contains("ku_http_socket_nonblocking(cli)"));
    assert!(generated.contains("send(socket_value, data, chunk, MSG_NOSIGNAL)"));
    assert!(generated.contains("setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE"));
    assert!(generated.contains("pthread_create(&worker, &attributes, ku_http_worker, &ctx)"));
    assert!(generated.contains("pthread_join(workers[w], NULL)"));
    assert!(generated.contains("#define KU_HTTP_ACCEPT_PEER_BACKOFF_CAP_MS 8"));
    assert!(generated.contains("#define KU_HTTP_ACCEPT_RESOURCE_RETRY_CAP 8"));
    assert!(generated.contains("error == WSAECONNRESET"));
    assert!(generated.contains("error == EINTR || error == ECONNABORTED"));
    assert!(generated.contains("error == EMFILE || error == ENFILE"));
    assert!(generated.contains("if (peer_accept_errors < KU_HTTP_ACCEPT_PEER_BACKOFF_CAP_MS)"));
    assert!(generated.contains("resource_accept_errors < KU_HTTP_ACCEPT_RESOURCE_RETRY_CAP"));
    assert!(generated.contains("ku_http_queue_close(&queue, listen_failure == NULL)"));
    assert!(!generated.contains("native backend is Winsock-only"));

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
    assert!(!gone.to_ascii_lowercase().contains("content-length:"));
    assert!(gone.ends_with("\r\n\r\n"));
}

#[cfg(unix)]
#[test]
fn native_http_peer_close_during_response_does_not_sigpipe_the_server() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let source = r#"
import "std.http"
fn main(): null! {
    app = http.server({ max_body_bytes: 1000000, write_timeout_ms: 1000 })
    app.post("/echo", fn(req) { return http.text(req.body.clone()) })
    app.get("/ok", fn() { return http.text("ok") })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let Some(_server) = spawn_native_server("peer-close", source, &address) else {
        return;
    };

    let mut stream = connect_with_retry(&address, Duration::from_secs(3));
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("set peer-close write timeout");
    stream
        .write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n")
        .expect("write peer-close headers");
    stream
        .write_all(&vec![b'x'; 1_000_000])
        .expect("write peer-close body");
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
    thread::sleep(Duration::from_millis(250));

    let response = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
}

const ROUTE_PARAM_NAMES_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.service()
    app.get("/users/{id}", fn(req) {
        return http.text("get:" + req.params.id)
    })
    app.post("/users/{name}", fn(req) {
        return http.text("post:" + req.params.name)
    })
    app.get("/orgs/{org_id}/users/{user_id}", fn(req) {
        return http.text("users:" + req.params.org_id + ":" + req.params.user_id)
    })
    app.get("/orgs/{slug}/posts/{post_id}", fn(req) {
        return http.text("posts:" + req.params.slug + ":" + req.params.post_id)
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_param_names_belong_to_selected_route_handler() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) =
        spawn_native_server("route-param-names", ROUTE_PARAM_NAMES_SOURCE, &address)
    else {
        return;
    };

    for (request, expected) in [
        (
            "GET /users/42 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "get:42",
        ),
        (
            "POST /users/alice HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            "post:alice",
        ),
        (
            "GET /orgs/acme/users/7 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "users:acme:7",
        ),
        (
            "GET /orgs/example/posts/9 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "posts:example:9",
        ),
    ] {
        let response = http_response(&address, request, Duration::from_secs(5));
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(
            response.ends_with(&format!("\r\n\r\n{expected}")),
            "selected route used the wrong parameter names:\n{response}"
        );
    }
}

const CAPTURED_ROUTE_SOURCE: &str = r#"
import "std.http"

fn make_app() {
    app = http.service()
    captured = "captured-route"
    app.get("/captured", fn() {
        return http.text(captured.clone())
    })
    return app
}

fn main(): null! {
    app = make_app()
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_route_retains_captured_handler_environment() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(_server) = spawn_native_server("captured-route", CAPTURED_ROUTE_SOURCE, &address)
    else {
        return;
    };
    let response = http_response(
        &address,
        "GET /captured HTTP/1.1\r\nHost: localhost\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("\r\n\r\ncaptured-route"), "{response}");
}

const LEXICAL_CAPTURE_OWNED_RESPONSE_SOURCE: &str = r#"
import "std.http"

fn Echo(count: int): int { return count }

fn main(): null! {
    count = 0
    read_count = () => { return count }
    ordinary = () => { return 0 }
    app = http.service({
        max_connections: 4,
        max_active_requests: 1,
        max_pending_requests: 2
    })
    app.post("/capture", fn(req) {
        count: str = req.body
        observed = read_count() + Echo(0)
        if (observed == 7) { return http.text(count) }
        return http.text("wrong-lexical-capture")
    })
    ordinary = () => { return 7 }
    count = ordinary()
    handler = () => { return http.text("registered-direct-value") }
    app.get("/direct", handler)
    handler = () => { return http.text("unregistered-replacement") }
    template_outer = 41
    template_text = `{((x: int) => x + template_outer)(1)}`
    if (template_text != "42") { fail "template closure lost its captured cell" }
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_lexical_capture_replay_preserves_local_owned_response() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(server) = spawn_observed_native_server(
        "lexical-capture-owned-response",
        LEXICAL_CAPTURE_OWNED_RESPONSE_SOURCE,
        &address,
    ) else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real native compiler"
        );
        return;
    };
    // A different owned request body on each call must reach the response
    // without clone. That branch also requires the helper's outer int capture,
    // not the same-named handler-local string. This is not a soak/leak proof.
    for body in ["first-owned", "second-owned", "third-owned"] {
        let request = format!(
            "POST /capture HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let response = http_response(&address, &request, Duration::from_secs(5));
        assert_status(&response, "HTTP/1.1 200 OK");
        assert_eq!(
            response.split_once("\r\n\r\n").expect("HTTP headers").1,
            body,
            "helper must read outer count=7 and move only the per-request string"
        );
    }
    // Registration retains the current invoke/env value, not the caller's
    // handler variable cell. Its later replacement must not change this route.
    let response = http_response(
        &address,
        "GET /direct HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
    assert_eq!(
        response.split_once("\r\n\r\n").expect("HTTP headers").1,
        "registered-direct-value",
        "direct handler values must not become aliases of the caller's variable cell"
    );
    server.finish_checked();
}

#[test]
fn native_http_lexical_capture_replay_rejects_shadowed_write_before_c_emission() {
    let dir = unique_temp_dir("lexical-capture-write-rejected");
    let entry = "server.ku";
    fs::write(
        dir.join(entry),
        r#"
import "std.http"
fn main(): null! {
    count = 0
    mutate = () => { count += 1; return null }
    app = http.service()
    app.get("/", fn() {
        count: int = 0
        mutate()
        return http.text("bad")
    })
    return ok(null)
}
"#,
    )
    .expect("write rejected HTTP source");
    let mut command = Command::new(ku_binary());
    // No -o: the compatibility command stops at checker/C emission, without
    // linking an executable or starting the rejected handler.
    command.current_dir(&dir).args(["build", "--native", entry]);
    let output = run_bounded(&mut command, RUN_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("HTTP rejection check was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "shadowed outer write was accepted: {combined}"
    );
    assert!(
        combined.contains("http handler cannot modify captured variable 'count'"),
        "expected lexical capture rejection, got: {combined}"
    );
    assert!(
        !dir.join("server.c").exists(),
        "rejected HTTP source emitted C in {}",
        dir.display()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_http_shared_callable_writes_reject_before_c_emission() {
    for (label, body) in [
        (
            "direct",
            r#"
    app.get("/", fn() { render(); return http.text("ok") })
    render = () => { count += 1; return null }
"#,
        ),
        (
            "alias-setter",
            r#"
    setter = () => { render = Other; return null }
    alias = setter.clone()
    app.get("/", fn() { render(); return http.text("ok") })
    alias()
"#,
        ),
        (
            "loop",
            r#"
    turn = 0
    while (turn < 2) {
        render = Other
        if (turn == 0) {
            app.get("/", fn() { render(); return http.text("ok") })
        }
        turn += 1
    }
"#,
        ),
        (
            "template-hidden-setter",
            r#"
    setter = () => { render = Other; return 0 }
    hidden = () => { print(`{setter()}`); return null }
    app.get("/", fn() { render(); return http.text("ok") })
    hidden()
"#,
        ),
        (
            "installed-mutator",
            r#"
    fn Set(next: fn(): null): null { render = next.clone(); return null }
    render = Noop
    mutator = () => { count += 1; return null }
    Set(mutator.clone())
    app.get("/", fn() { render(); return http.text("ok") })
"#,
        ),
        (
            "factory-stale-body",
            r#"
    route_handler = () => { return http.text("safe") }
    bad = () => { count += 1; return http.text("bad") }
    install = () => {
        route_handler = bad.clone()
        marker = 0
        return null
    }
    // Restore the initial value after definition-time checking. The marker
    // keeps install outside the exact assignment/literal-null transfer rule.
    route_handler = () => { return http.text("safe") }
    app.get("/", After(install(), route_handler.clone()))
"#,
        ),
        (
            "destructure-stale-body",
            r#"
    route_handler = () => { return http.text("safe") }
    bad = () => { count += 1; return http.text("bad") }
    install = () => {
        route_handler = bad.clone()
        marker = 0
        return null
    }
    route_handler = () => { return http.text("safe") }
    _, chosen = install(), route_handler.clone()
    app.get("/", chosen)
"#,
        ),
        (
            "destructure-hidden-setter",
            r#"
    op = Noop
    setter = () => { render = Other; return null }
    install = () => {
        op = setter.clone()
        marker = 0
        return null
    }
    op = Noop
    app.get("/", fn() { render(); return http.text("ok") })
    _, selected = install(), op.clone()
    selected()
"#,
        ),
        (
            "for-backedge-setter",
            r#"
    setter = () => { render = Other; return null }
    for turn in [0, 1] {
        setter()
        if (turn == 0) {
            app.get("/", fn() { render(); return http.text("ok") })
        }
    }
"#,
        ),
        (
            "for-continue-setter",
            r#"
    setter = () => { render = Other; return null }
    for turn in [0, 1] {
        setter()
        if (turn == 0) {
            app.get("/", fn() { render(); return http.text("ok") })
            continue
        }
        break
    }
"#,
        ),
        (
            "service-move-freeze",
            r#"
    app.get("/", fn() { render(); return http.text("ok") })
    moved = app
    render = Other
"#,
        ),
        (
            "service-scope-freeze",
            r#"
    if (true) {
        app.get("/", fn() { render(); return http.text("ok") })
        scoped = app
    }
    render = Other
"#,
        ),
        (
            "listener-close-freeze",
            r#"
    app.get("/", fn() { render(); return http.text("ok") })
    listener = app.bind("127.0.0.1:0")?
    listener.close()?
    render = Other
"#,
        ),
        (
            "for-iterable-stale-body",
            r#"
    route_handler = () => { return http.text("safe") }
    bad = () => { count += 1; return http.text("bad") }
    install = () => {
        route_handler = bad.clone()
        marker = 0
        return null
    }
    // Reset after definition-time checking, retaining the non-exact installer.
    route_handler = () => { return http.text("safe") }
    for chosen in [After(install(), route_handler.clone())] {
        app.get("/", chosen)
    }
"#,
        ),
    ] {
        let source = format!(
            r#"
import "std.http"
fn Noop(): null {{ return null }}
fn Other(): null {{ return null }}
fn After<T>(ignored: null, value: T): T {{ return value }}
fn Before<T>(value: T, ignored: null): T {{ return value }}
fn main(): null! {{
    count = 0
    render = Noop
    app = http.service()
    {body}
    return ok(null)
}}
"#
        );
        // Overall parser acceptance alone does not parse template expressions.
        // Validate that call separately; neither preflight executes any source.
        if label == "template-hidden-setter" {
            let tokens = ku::lexer::Lexer::new("setter()")
                .lex()
                .expect("hidden setter expression must lex");
            ku::parser::Parser::new(tokens)
                .parse_expression_only()
                .expect("hidden setter expression must parse");
        }
        let tokens = ku::lexer::Lexer::with_file(label, &source)
            .lex()
            .unwrap_or_else(|error| panic!("{label} must lex: {}", error.message));
        ku::parser::Parser::new(tokens)
            .parse_program()
            .unwrap_or_else(|error| panic!("{label} must parse: {}", error.message));

        // Remove only registration, preserving setter effects and lifecycle.
        // These are checker-only controls: bind/close is not run or native-built.
        if matches!(
            label,
            "for-backedge-setter"
                | "for-continue-setter"
                | "service-move-freeze"
                | "service-scope-freeze"
                | "listener-close-freeze"
        ) {
            let registration = r#"app.get("/", fn() { render(); return http.text("ok") })"#;
            assert_eq!(source.matches(registration).count(), 1);
            let control = source.replace(registration, "");
            ku::cli::check_source(&format!("control-{label}"), &control).unwrap_or_else(|error| {
                panic!("{label}: no-registration control rejected: {error}\n{control}")
            });
        }

        // Destructuring and iterable rows get these paired checker preflights.
        // Keep the installation and all evaluations in the no-route control.
        // The reversed-order source still registers a route, but only the old
        // safe value was copied; check_source never executes either program.
        let evaluated_value_controls = match label {
            "destructure-stale-body" => Some((
                r#"app.get("/", chosen)"#,
                "_, chosen = install(), route_handler.clone()",
                "chosen, _ = route_handler.clone(), install()",
            )),
            "destructure-hidden-setter" => Some((
                r#"app.get("/", fn() { render(); return http.text("ok") })"#,
                "_, selected = install(), op.clone()",
                "selected, _ = op.clone(), install()",
            )),
            "for-iterable-stale-body" => Some((
                r#"app.get("/", chosen)"#,
                "After(install(), route_handler.clone())",
                "Before(route_handler.clone(), install())",
            )),
            _ => None,
        };
        if let Some((registration, later, earlier)) = evaluated_value_controls {
            assert_eq!(source.matches(registration).count(), 1);
            let control = source.replace(registration, "");
            ku::cli::check_source(&format!("control-{label}"), &control).unwrap_or_else(|error| {
                panic!("{label}: no-registration control rejected: {error}\n{control}")
            });
            assert_eq!(source.matches(later).count(), 1);
            let reversed = source.replace(later, earlier);
            assert_eq!(reversed.matches(registration).count(), 1);
            ku::cli::check_source(&format!("reversed-{label}"), &reversed).unwrap_or_else(
                |error| {
                    panic!("{label}: old evaluated value must remain legal: {error}\n{reversed}")
                },
            );
        }

        let dir = unique_temp_dir(&format!("shared-callable-{label}"));
        fs::write(dir.join("server.ku"), &source).expect("write rejected HTTP source");
        for (entry, args) in [
            ("check-json", &["check", "--json", "server.ku"][..]),
            ("native", &["build", "--native", "server.ku"][..]),
            ("backend-c", &["build", "--backend", "c", "server.ku"][..]),
        ] {
            let mut command = Command::new(ku_binary());
            // No -o. --native is C-only, but --backend c normally links too.
            // Empty KU_CC forbids compiler discovery if rejection regresses;
            // that fallback error cannot satisfy the safety assertions below.
            command.current_dir(&dir).env("KU_CC", "").args(args);
            let output = run_bounded(&mut command, RUN_TIMEOUT, BUILD_OUTPUT_LIMITS)
                .unwrap_or_else(|error| {
                    panic!("{label}/{entry}: HTTP rejection was not bounded: {error}")
                });
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !output.status.success(),
                "{label}/{entry}: unsafe shared callable was accepted: {combined}"
            );
            // Assert both the compatibility artifact and the backend's actual
            // default host/debug artifact, before interpreting diagnostics.
            for artifact in ["server.c", ".ku/build/debug/c/server.c"] {
                assert!(
                    !dir.join(artifact).exists(),
                    "{label}/{entry}: rejected HTTP source emitted {artifact} in {}",
                    dir.display()
                );
            }
            let diagnostic = if entry == "check-json" {
                use ku::value::Value;

                let stdout = std::str::from_utf8(&output.stdout).expect("JSON stdout is UTF-8");
                let stderr = std::str::from_utf8(&output.stderr).expect("JSON stderr is UTF-8");
                let lines = stdout
                    .lines()
                    .chain(stderr.lines())
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>();
                assert_eq!(
                    lines.len(),
                    1,
                    "{label}/{entry}: expected exactly one JSON diagnostic: {combined}"
                );
                let parsed = ku::stdlib::json::eval(
                    "parse",
                    &[Value::String(lines[0].to_string())],
                    ku::span::Span::default(),
                )
                .expect("parse JSON diagnostic with the existing JSON parser");
                let Some(Value::Result { ok: true, value }) = parsed else {
                    panic!("{label}/{entry}: invalid JSON diagnostic: {combined}");
                };
                let Value::Object(fields) = *value else {
                    panic!("{label}/{entry}: diagnostic must be a JSON object: {combined}");
                };
                let Some(Value::String(code)) = fields.get("code") else {
                    panic!("{label}/{entry}: missing diagnostic code: {combined}");
                };
                let Some(Value::String(message)) = fields.get("message") else {
                    panic!("{label}/{entry}: missing diagnostic message: {combined}");
                };
                assert!(
                    matches!(fields.get("level"), Some(Value::String(level)) if level == "error"),
                    "{label}/{entry}: expected error level: {combined}"
                );
                assert!(
                    code == "E0704"
                        || (matches!(
                            label,
                            "installed-mutator"
                                | "factory-stale-body"
                                | "destructure-stale-body"
                                | "for-iterable-stale-body"
                        ) && code == "E0703"),
                    "{label}/{entry}: wrong structured diagnostic code: {combined}"
                );
                // Do not let notes or embedded source text satisfy the reason.
                format!("{code}: {message}")
            } else {
                combined.clone()
            };
            let shared_reassignment = diagnostic.contains("E0704")
                && diagnostic.contains("cannot reassign HTTP-shared function binding 'render'");
            let unproved_shared_effect = diagnostic.contains("E0704")
                && diagnostic.contains("cannot prove")
                && diagnostic.contains("HTTP-shared");
            let expected_reason = match label {
                // Incomplete transitive/template/alias effects must fail closed,
                // not be accepted because no concrete write could be recovered.
                "template-hidden-setter" | "destructure-hidden-setter" => {
                    shared_reassignment || unproved_shared_effect
                }
                // This write precedes registration: E0703 must describe the
                // actual count mutation; E0704 must explicitly lack proof.
                "installed-mutator"
                | "factory-stale-body"
                | "destructure-stale-body"
                | "for-iterable-stale-body" => {
                    (diagnostic.contains("E0703")
                        && diagnostic
                            .contains("http handler cannot modify captured variable 'count'"))
                        || unproved_shared_effect
                }
                _ => shared_reassignment,
            };
            assert!(
                expected_reason,
                "{label}/{entry}: expected an HTTP shared-state safety rejection, not a syntax/type error: {combined}"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }
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
    app.get("/unknown-status", fn() {
        return http.text(418, "teapot")
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

    // Header and body are written together. A chunked header reader may receive
    // bytes beyond \r\n\r\n in that same recv and must preserve them as the body
    // prefix instead of dropping them or waiting for bytes that already arrived.
    let coalesced_body = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\nsame",
        Duration::from_secs(3),
    );
    assert_status(&coalesced_body, "HTTP/1.1 200 OK");
    assert!(
        coalesced_body.ends_with("\r\n\r\nsame"),
        "header/body coalescing lost or changed the body prefix:\n{coalesced_body}"
    );

    // Force the final CRLF delimiter across recv calls, then coalesce its final
    // LF with the body. The strict CRLF state must survive chunk boundaries.
    let mut fragmented = connect_with_retry(&address, Duration::from_secs(2));
    fragmented.set_nodelay(true).expect("set TCP_NODELAY");
    fragmented
        .write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r")
        .expect("write fragmented header prefix");
    thread::sleep(Duration::from_millis(30));
    fragmented
        .write_all(b"\nfrag")
        .expect("write fragmented delimiter and body");
    let fragmented_response = read_http_stream(fragmented, Duration::from_secs(2));
    assert_status(&fragmented_response, "HTTP/1.1 200 OK");
    assert!(
        fragmented_response.ends_with("\r\n\r\nfrag"),
        "fragmented delimiter lost or changed the body prefix:\n{fragmented_response}"
    );

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

    // The block reader still enforces the configured wire-byte limit before it
    // can accept a delimiter found later in the same recv.
    let too_large_header = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nX-Long: 12345678901234567890123456789012345678901234567890123456789012345678901234567890\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(
        &too_large_header,
        "HTTP/1.1 431 Request Header Fields Too Large",
    );

    // Content-Length uses strict RFC decimal syntax. Signs, duplicates and
    // overflow are all rejected before a body allocation or route dispatch.
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

    let signed = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: +2\r\nConnection: close\r\n\r\nab",
        Duration::from_secs(3),
    );
    assert_status(&signed, "HTTP/1.1 400 Bad Request");

    // Bare-LF framing is rejected before parsing either the request line or a
    // header field, matching the interpreter's strict CRLF-only framing.
    let lf_only = http_response(
        &address,
        "GET /ok HTTP/1.1\nHost: localhost\n\n",
        Duration::from_secs(3),
    );
    assert_status(&lf_only, "HTTP/1.1 400 Bad Request");

    // A bare LF inside a CRLF-framed field is obs-fold ambiguity and must be
    // rejected rather than folded into a value.
    let embedded_lf = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\nX-Extra: v\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&embedded_lf, "HTTP/1.1 400 Bad Request");

    for (label, request) in [
        (
            "missing Host",
            "GET /ok HTTP/1.1\r\nConnection: close\r\n\r\n",
        ),
        (
            "duplicate Host",
            "GET /ok HTTP/1.1\r\nHost: one\r\nHost: two\r\n\r\n",
        ),
        (
            "Transfer-Encoding",
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ),
        (
            "duplicate Content-Length",
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
        ),
        (
            "whitespace before colon",
            "GET /ok HTTP/1.1\r\nHost : localhost\r\n\r\n",
        ),
        (
            "obs-fold",
            "GET /ok HTTP/1.1\r\nHost: localhost\r\n folded\r\n\r\n",
        ),
        (
            "wrong HTTP version",
            "GET /ok HTTP/1.0\r\nHost: localhost\r\n\r\n",
        ),
        (
            "invalid method token",
            "GE[T /ok HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        (
            "NUL request target",
            "GET /ok\0hidden HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        (
            "malformed percent escape",
            "GET /ok%2 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        (
            "non-hex percent escape",
            "GET /ok%GG HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        (
            "backslash request target",
            "GET /ok\\hidden HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        (
            "fragment request target",
            "GET /ok#fragment HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
    ] {
        let response = http_response(&address, request, Duration::from_secs(3));
        assert_status(&response, "HTTP/1.1 400 Bad Request");
        assert!(
            !response.contains("exact"),
            "invalid {label} request reached a handler: {response}"
        );
    }

    let expect = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&expect, "HTTP/1.1 417 Expectation Failed");

    let unknown = http_response(
        &address,
        "GET /unknown-status HTTP/1.1\r\nHost: localhost\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&unknown, "HTTP/1.1 418 Unknown");

    let mut invalid_utf8 =
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n".to_vec();
    invalid_utf8.extend_from_slice(&[0xc3, 0x28]);
    let response = http_response_bytes(&address, &invalid_utf8, Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 400 Bad Request");

    let mut invalid_header_value = b"GET /ok HTTP/1.1\r\nHost: localhost\r\nX-Invalid: ".to_vec();
    invalid_header_value.push(0xff);
    invalid_header_value.extend_from_slice(b"\r\n\r\n");
    let response = http_response_bytes(&address, &invalid_header_value, Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 400 Bad Request");
    assert!(
        !response.ends_with("\r\n\r\nok"),
        "invalid UTF-8 header value reached the handler:\n{response}"
    );
}

// Idle read timeout (408) via the config-object form. The handler-timeout (504)
// path is intentionally NOT covered: native cannot preempt a compiled handler.
const IDLE_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.server({
        idle_timeout_ms: 150,
        read_header_timeout_ms: 500,
        read_body_timeout_ms: 500,
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

    // An incomplete, slowly delivered header still receives a structured timeout;
    // receiving chunks must not turn the total header deadline into an idle timer.
    let mut drip = connect_with_retry(&address, Duration::from_secs(2));
    for (index, byte) in b"GET ".iter().enumerate() {
        if drip.write_all(&[*byte]).is_err() {
            break;
        }
        if index + 1 < b"GET ".len() {
            thread::sleep(Duration::from_millis(80));
        }
    }
    let drip_response = read_http_stream(drip, Duration::from_secs(2));
    assert_status(&drip_response, "HTTP/1.1 408 Request Timeout");

    let mut partial_body = connect_with_retry(&address, Duration::from_secs(2));
    partial_body
        .write_all(b"POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\na")
        .expect("write partial body");
    let body_response = read_http_stream(partial_body, Duration::from_secs(2));
    assert_status(&body_response, "HTTP/1.1 408 Request Timeout");
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

// Reading a socket must not consume one of the capped handler workers. The
// 64 incomplete bodies precede the healthy request in accept/queue order; the
// old 64-worker socket queue cannot read that request before a body is released.
const SHARED_IO_SOURCE: &str = r#"
import "std.http"

fn main(): null! {
    app = http.server({
        idle_timeout_ms: 5000,
        read_header_timeout_ms: 10000,
        read_body_timeout_ms: 10000,
        write_timeout_ms: 2000,
        handler_timeout_ms: 5000,
        max_connections: 192,
        max_active_requests: 128,
        max_pending_requests: 128
    })
    app.post("/echo/{id}", fn(req) {
        return http.text(req.params.id + ":" + req.query.q + ":" + req.headers.host + ":" + req.body)
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_http_partial_bodies_leave_handler_workers_available() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    // No readiness connection or retry after a successful connect: exactly 64
    // partial requests, one healthy request and one recovery request are accepted.
    let Some(mut server) =
        spawn_finite_native_server("shared-io-partial-bodies", SHARED_IO_SOURCE, &address, 66)
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real native compiler"
        );
        return;
    };
    let directory = server.dir().to_path_buf();
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let request = |id: &str, query: &str, body: &str| {
        format!(
            "POST /echo/{id}?q={query} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let check_response = |response: &[u8], expected: &str| {
        let response = std::str::from_utf8(response).expect("native HTTP response UTF-8");
        assert_status(response, "HTTP/1.1 200 OK");
        assert_eq!(
            response.split_once("\r\n\r\n").expect("HTTP headers").1,
            expected,
            "native handler must use this request's params, query, headers and owned body"
        );
    };
    let mut partials = Vec::with_capacity(64);
    for index in 0..64 {
        let id = format!("partial-{index}");
        let query = format!("held-{index}");
        let body = format!("正文-{index}-界🙂\0!");
        let bytes = request(&id, &query, &body).into_bytes();
        let mut stream = connect_with_retry(&address, Duration::from_secs(2));
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("partial write timeout");
        stream
            .write_all(&bytes[..bytes.len() - 1])
            .expect("hold final body byte");
        partials.push((stream, format!("{id}:{query}:localhost:{body}")));
    }

    // This is a progress requirement while all bodies are incomplete, not a
    // short scheduling race or a thread-count assertion. The configured body
    // D remains 10s; only the external healthy-response observation is 2s.
    let healthy_body = "健康-界🙂\0!";
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut healthy = connect_with_retry(&address, Duration::from_secs(2));
    healthy
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("healthy write timeout");
    healthy
        .write_all(request("healthy", "progress", healthy_body).as_bytes())
        .expect("write healthy request");
    let response = read_http_stream_bytes_until(&mut healthy, deadline)
        .expect("incomplete socket bodies must leave a handler worker available");
    check_response(
        &response,
        &format!("healthy:progress:localhost:{healthy_body}"),
    );
    drop(healthy);

    // Release every retained input before reading responses, then apply one
    // shared external deadline to the entire drain, rather than 64 fresh waits.
    let drain_deadline = Instant::now() + Duration::from_secs(5);
    for (stream, _) in &mut partials {
        stream.write_all(b"!").expect("release final body byte");
    }
    for (stream, expected) in &mut partials {
        let response = read_http_stream_bytes_until(stream, drain_deadline)
            .expect("released request must complete within the shared drain deadline");
        check_response(&response, expected);
    }
    drop(partials);

    let recovered_body = "恢复-界🙂\0!";
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut recovered = connect_with_retry(&address, Duration::from_secs(2));
    recovered
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("recovery write timeout");
    recovered
        .write_all(request("recovered", "after-drain", recovered_body).as_bytes())
        .expect("write recovery request");
    let response = read_http_stream_bytes_until(&mut recovered, deadline)
        .expect("native HTTP service must recover after releasing partial bodies");
    check_response(
        &response,
        &format!("recovered:after-drain:localhost:{recovered_body}"),
    );
    drop(recovered);

    // The finite accepted-connection bound must drain accepted work, join the
    // runtime threads and return success. A watchdog kill never records this
    // receipt and cannot satisfy the natural-exit assertion.
    let status = watchdog.wait_for_natural_exit(Duration::from_secs(3));
    assert!(
        status.success(),
        "finite native HTTP server failed: {status}"
    );
    assert!(!watchdog.timed_out());
    drop(watchdog);
    assert!(
        !directory.exists(),
        "native HTTP temporary artifacts remain"
    );
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
    app.get("/__SEGMENTS__", fn() {
        return http.text("segment-route")
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
    let segments = (0..64).map(|_| "s").collect::<Vec<_>>().join("/");
    let source = TARGET_SOURCE
        .replace("__A__", &shared)
        .replace("__SEGMENTS__", &segments);
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

    let exact_segments = http_response(
        &address,
        &format!("GET /{segments} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        Duration::from_secs(3),
    );
    assert!(exact_segments.contains("\r\n\r\nsegment-route"));

    let extra_segment = http_response(
        &address,
        &format!("GET /{segments}/extra HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        Duration::from_secs(3),
    );
    assert_status(&extra_segment, "HTTP/1.1 414 URI Too Long");
    assert!(!extra_segment.contains("segment-route"));
}

#[test]
fn native_http_route_registration_rejects_unreachable_unsafe_and_duplicate_shapes() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let segments = (0..65).map(|_| "s").collect::<Vec<_>>().join("/");
    let too_many = format!(
        "import \"std.http\"\nfn handler() {{ return http.text(\"ok\") }}\nfn main(): null! {{\n app = http.service()\n app.get(\"/{segments}\", handler)\n app.listen(\"127.0.0.1:0\")?\n return ok(null)\n}}"
    );
    let Some(output) = native_run_output("route-segments", &too_many) else {
        return;
    };
    assert!(!output.status.success(), "65-segment route was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("at most 64 segments"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let nul_route = "import \"std.http\"\nfn handler() { return http.text(\"ok\") }\nfn main(): null! {\n app = http.service()\n app.get(\"/bad\0tail\", handler)\n app.listen(\"127.0.0.1:0\")?\n return ok(null)\n}";
    let Some(output) = native_run_output("route-nul", nul_route) else {
        return;
    };
    assert!(!output.status.success(), "NUL route was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid http route path"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let colon_route = r#"
import "std.http"
fn handler() { return http.text("ok") }
fn main() {
    app = http.service()
    app.get("/user/:id", handler)
}
"#;
    let Some(output) = native_run_output("route-colon-param", colon_route) else {
        return;
    };
    assert!(
        !output.status.success(),
        "native accepted Express-style :id route params"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid http route path"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let duplicate = r#"
import "std.http"
fn handler() { return http.text("ok") }
fn main(): null! {
    app = http.service()
    app.get("/users/{id}", handler)
    app.get("/users/{name}", handler)
    app.listen("127.0.0.1:0")?
    return ok(null)
}
"#;
    let Some(output) = native_run_output("route-duplicate", duplicate) else {
        return;
    };
    assert!(
        !output.status.success(),
        "duplicate route shape was accepted"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("duplicate http route"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let duplicate_param = r#"
import "std.http"
fn handler() { return http.text("ok") }
fn main() {
    app = http.service()
    app.get("/users/{id}/posts/{id}", handler)
}
"#;
    let interpreted = interpreter_run_output("route-duplicate-param-interpreter", duplicate_param);
    assert!(
        !interpreted.status.success(),
        "interpreter accepted a duplicate route parameter"
    );
    let interpreted_stderr = String::from_utf8_lossy(&interpreted.stderr);
    assert!(
        interpreted_stderr.contains("duplicate http route param 'id'"),
        "unexpected interpreter stderr: {interpreted_stderr}"
    );

    let Some(native) = native_run_output("route-duplicate-param-native", duplicate_param) else {
        return;
    };
    assert!(
        !native.status.success(),
        "native accepted a duplicate route parameter"
    );
    let native_stderr = String::from_utf8_lossy(&native.stderr);
    assert!(
        native_stderr.contains("duplicate http route param 'id'"),
        "unexpected native stderr: {native_stderr}"
    );
}

#[test]
fn native_http_listen_rejects_invalid_addresses_without_binding_fallback() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import "std.http"

fn reject(address: str): null! {
    app = http.service()
    try {
        app.listen(address)?
        panic("invalid address unexpectedly listened")
    } catch (err) {
        if (err.code != "listen_failed") {
            panic("wrong listen error")
        }
    }
    return ok(null)
}

fn main(): null! {
    reject("unknown.invalid:8080")?
    reject("127.0.0.1:-1")?
    reject("127.0.0.1:65536")?
    reject("127.0.0.1:12x")?
    reject("127.0.0.1:80__NUL__hidden")?
    return ok(null)
}
"#
    .replace("__NUL__", "\0");
    let Some(output) = native_run_output("invalid-listen-address", &source) else {
        return;
    };
    assert!(
        output.status.success(),
        "invalid addresses were not rejected cleanly:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_http_config_limits_reject_before_integer_narrowing() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (name, source, maximum) in [
        (
            "constructor-limit",
            r#"import "std.http"
fn main(): null! {
    app = http.service({ max_body_bytes: 3000000000 })
    app.listen("127.0.0.1:0")?
    return ok(null)
}"#,
            "16777216",
        ),
        (
            "assigned-limit",
            r#"import "std.http"
fn main(): null! {
    app = http.service()
    app.max_connections = 4097
    app.listen("127.0.0.1:0")?
    return ok(null)
}"#,
            "4096",
        ),
    ] {
        let Some(output) = native_run_output(name, source) else {
            return;
        };
        assert!(!output.status.success(), "over-limit config was accepted");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(maximum),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
    app.get("/bad", fn() {
        return http.redirect("/dest\r\nx-injected: yes")
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

    let injected = http_response(
        &address,
        "GET /bad HTTP/1.1\r\nHost: localhost\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&injected, "HTTP/1.1 500 Internal Server Error");
    assert!(!injected.contains("x-injected"));
}

// Native cancellation is cooperative: loop back-edges and returns from Ku calls
// poll the worker-local deadline, unwind through ordinary return/finally cleanup,
// and leave the worker as the sole socket owner that emits 504.
const SLOW_SOURCE: &str = r#"
import "std.http"
import fs from "std.fs"

fn Finish(path: str, content: str): null! {
    fs.write(path, content)?
    return ok(null)
}

fn spin(): int {
    while (true) {
    }
    return 0
}

fn slow_handler() {
    fs.write("slow-entered.txt", "slow-route")
    owned = "owned-" + "payload"
    try {
        spin()
    } finally {
        // The real cleanup grace can expire during filesystem I/O. The exact
        // helper-and-following-statement contract within D is checked with a
        // controlled clock in native_cleanup_catch_test.rs.
        try {
            Finish("finally-one.txt", owned.clone())?
            fs.write("finally-two.txt", "after-helper")?
        } catch (err) {
            panic("finally write failed: " + err.message)
        }
    }
    return http.text("unreachable")
}

fn main(): null! {
    app = http.server({
        handler_timeout_ms: 100,
        read_header_timeout_ms: 2000,
        max_connections: 16,
        max_active_requests: 1,
        max_pending_requests: 4
    })
    app.get("/slow", slow_handler)
    app.get("/map", fn() {
        fs.write("map-entered.txt", "map-route")
        values = [1, 2, 3]
        mapped = values.map(fn(value) {
            owned = "mapper-" + "payload"
            while (true) {
            }
            return value + 1
        })
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
fn native_cooperative_handler_timeout_returns_504_and_server_stays_up() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(server) = spawn_native_server("slow", SLOW_SOURCE, &address) else {
        return;
    };

    let ready = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&ready, "HTTP/1.1 200 OK");

    let started = Instant::now();
    let slow = http_response(
        &address,
        "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&slow, "HTTP/1.1 504 Gateway Timeout");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cooperative handler timeout took too long: {:?}",
        started.elapsed()
    );
    assert_eq!(
        fs::read_to_string(server.dir().join("slow-entered.txt")).expect("slow route entered"),
        "slow-route"
    );
    // This is the real-clock HTTP recovery gate. Both ordered finally writes
    // remain mandatory in native_cleanup_catch_test.rs, where the clock proves
    // the helper returns before the original cleanup D; OS I/O latency here
    // cannot make that within-D assumption unconditional.

    // max_active_requests=1 means this can only succeed if the timed-out handler
    // returned, released its connection permit, and made the same worker reusable.
    let ok = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&ok, "HTTP/1.1 200 OK");

    // This proves the /map route entered and released the sole worker after
    // a real timeout. Its pre-invocation poll may expire before the mapper runs;
    // mapper execution/ownership is checked by the controlled-clock map gates.
    let mapped = http_response(
        &address,
        "GET /map HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&mapped, "HTTP/1.1 504 Gateway Timeout");
    assert_eq!(
        fs::read_to_string(server.dir().join("map-entered.txt")).expect("map route entered"),
        "map-route"
    );

    let after_map = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&after_map, "HTTP/1.1 200 OK");
}

// A timed-out handler enters `finally`, but cleanup code is still user code and
// can itself fail to terminate. The native runtime must bound that cleanup so a
// single request cannot pin the only active-request permit forever.
const INFINITE_FINALLY_SOURCE: &str = r#"
import "std.http"

fn spin(): int {
    while (true) {
    }
    return 0
}

fn stuck_handler() {
    try {
        spin()
    } finally {
        while (true) {
        }
    }
    return http.text("unreachable")
}

fn main(): null! {
    app = http.server({
        handler_timeout_ms: 100,
        read_header_timeout_ms: 2000,
        max_connections: 8,
        max_active_requests: 1,
        max_pending_requests: 2
    })
    app.get("/stuck", stuck_handler)
    app.get("/ok", fn() {
        return http.text("ok")
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_handler_timeout_bounds_infinite_finally_and_releases_worker() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let Some(mut server) =
        spawn_native_server("infinite-finally", INFINITE_FINALLY_SOURCE, &address)
    else {
        return;
    };

    let ready = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_status(&ready, "HTTP/1.1 200 OK");

    // The watchdog owns the external server process. Its deadline is longer
    // than the runtime's expected cleanup bound but shorter than the socket
    // read timeout, so a regression is force-killed instead of hanging cargo.
    let watchdog = server.arm_kill_watchdog(Duration::from_secs(10));
    let started = Instant::now();
    let mut stuck = connect_with_retry(&address, Duration::from_secs(2));
    stuck
        .write_all(b"GET /stuck HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write infinite-finally request");
    let stuck_response = read_http_stream(stuck, Duration::from_secs(12));
    assert!(
        !watchdog.timed_out(),
        "native server was killed by the 10s test watchdog; timed-out finally did not terminate"
    );
    assert_status(&stuck_response, "HTTP/1.1 504 Gateway Timeout");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "infinite finally cleanup was not bounded promptly: {:?}",
        started.elapsed()
    );

    // With max_active_requests=1, success proves the first handler released its
    // permit and did not leave the sole worker stuck in its finally loop.
    let recovered = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&recovered, "HTTP/1.1 200 OK");
    assert!(!watchdog.timed_out(), "test watchdog fired after recovery");
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
        assert!(
            built,
            "HTTP program '{name}' must compile and link natively"
        );
    }
}

// Synchronous arithmetic through all four native HTTP dispatcher shapes.
// Reuses the real compiler, socket reader, lock and child-process watchdog.
const SYNC_ARITHMETIC_HTTP_SOURCE: &str = r#"
import "std.http"
import fs from "std.fs"

fn Divide(left: int, right: int): int { return left / right }
fn RecoverableShape(): int! {
    owned = "fatal-" + "owner"
    try {
        return ok(Divide(7, 0))
    } catch (err) {
        fs.write("BAD-catch.txt", owned.clone())
        return ok(99)
    } finally {
        fs.write("BAD-finally.txt", "ordinary fatal must skip user finally")
    }
    return ok(0)
}
fn Mark(): null! {
    fs.write("cleanup-outer.txt", "healthy-after-failed-callee")?
    return ok(null)
}
fn Timed() {
    owner = "timed-" + "owner"
    try { while (true) {} }
    finally {
        try {
            value = Divide(9, 0)
            fs.write("BAD-after-math.txt", str(value))
        } finally {
            Mark()
            fs.write("cleanup-after-helper.txt", owner.clone())
        }
    }
    return http.text("BAD timeout response")
}
fn main(): null! {
    app = http.server({
        handler_timeout_ms: 100,
        read_header_timeout_ms: 2000,
        max_connections: 8,
        max_active_requests: 1,
        max_pending_requests: 2
    })
    app.get("/plain-zero", fn() {
        value = Divide(7, 0)
        return http.text(str(value))
    })
    app.post("/plain-one", fn(req) {
        owned = req.body.clone()
        value = Divide(7, 0)
        return http.text(owned + str(value))
    })
    app.get("/result-zero", fn() {
        try {
            value = RecoverableShape()?
            return ok(http.text(str(value)))
        } catch (err) { fs.write("BAD-handler-catch.txt", err.message.clone()) }
        return ok(http.text("BAD-handler-fallback"))
    })
    app.post("/result-one", fn(req) {
        owned = req.body.clone()
        try {
            value = RecoverableShape()?
            return ok(http.text(owned + str(value)))
        } catch (err) { fs.write("BAD-handler-catch.txt", err.message.clone()) }
        return ok(http.text("BAD-handler-fallback"))
    })
    app.get("/timed", Timed)
    app.get("/ok", fn() { return http.text("clean-worker") })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;

#[test]
fn native_sync_arithmetic_http_failure_isolated_and_timeout_keeps_504() {
    let _guard = HTTP_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let address = unused_local_address();
    let Some(mut server) =
        spawn_native_server("sync-arithmetic", SYNC_ARITHMETIC_HTTP_SOURCE, &address)
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real native compiler"
        );
        return;
    };
    let generated = fs::read_to_string(server.c_source()).expect("generated C");
    // Reject the old unchecked artifact before sending any arithmetic request.
    assert_eq!(generated.matches("static uint32_t ku_int_div(").count(), 1);
    let divide_body = generated
        .split_once("int64_t Divide(int64_t left, int64_t right) {")
        .expect("actual Divide definition")
        .1
        .split("\n}")
        .next()
        .unwrap();
    assert!(
        divide_body.contains("ku_int_div("),
        "actual Divide must call checked helper before any request"
    );
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let directory = server.dir().to_path_buf();
    // Existing HTTP harness has already started the server. This only verifies
    // live source removal; cold-start source independence is a separate gate.
    fs::remove_file(directory.join("server.ku")).unwrap();
    fs::remove_file(server.c_source()).unwrap();
    let watchdog = server.arm_kill_watchdog(Duration::from_secs(20));
    let good_request = "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let ready = http_response(&address, good_request, Duration::from_secs(5));
    assert_status(&ready, "HTTP/1.1 200 OK");
    assert!(ready.ends_with("clean-worker"));
    for _ in 0..3 {
        for (method, path, body) in [
            ("GET", "/plain-zero", ""),
            ("POST", "/plain-one", "owned request"),
            ("GET", "/result-zero", ""),
            ("POST", "/result-one", "owned request"),
        ] {
            let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let failed = http_response(&address, &request, Duration::from_secs(3));
            assert_status(&failed, "HTTP/1.1 500 Internal Server Error");
            assert_eq!(
                failed.split_once("\r\n\r\n").expect("HTTP headers").1,
                "Internal Server Error"
            );
            let recovered = http_response(&address, good_request, Duration::from_secs(3));
            assert_status(&recovered, "HTTP/1.1 200 OK");
            assert!(recovered.ends_with("clean-worker"));
            assert!(!watchdog.timed_out());
        }
    }
    let timed = http_response(
        &address,
        "GET /timed HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&timed, "HTTP/1.1 504 Gateway Timeout");
    assert_eq!(
        fs::read_to_string(directory.join("cleanup-outer.txt")).unwrap(),
        "healthy-after-failed-callee"
    );
    assert_eq!(
        fs::read_to_string(directory.join("cleanup-after-helper.txt")).unwrap(),
        "timed-owner"
    );
    for marker in [
        "BAD-catch.txt",
        "BAD-finally.txt",
        "BAD-after-math.txt",
        "BAD-handler-catch.txt",
    ] {
        assert!(
            !directory.join(marker).exists(),
            "unexpected continuation: {marker}"
        );
    }
    let recovered = http_response(&address, good_request, Duration::from_secs(3));
    assert_status(&recovered, "HTTP/1.1 200 OK");
    assert!(recovered.ends_with("clean-worker"));
    assert!(!watchdog.timed_out());
    // max_active_requests=1 exercises one execution thread reused across roots
    // (including the legacy inline acceptor fallback if worker creation fails).
    // Marker writes prove actions occurred, not exactly-once counts or D values.
    // This is not a multi-worker, allocation-ledger or production-soak claim.
}

#[test]
fn native_http_gather_write_preserves_partial_vectors_errors_and_turn_budgets() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = include_str!("../src/backend/c_http_runtime.c").replace("\r\n", "\n");
    let region = |start: &str, end: &str| {
        format!(
            "{start}{}",
            runtime
                .split_once(start)
                .expect("production HTTP region starts")
                .1
                .split_once(end)
                .expect("production HTTP region ends")
                .0
        )
    };
    // Compile the actual production output layout, scatter/gather syscall and
    // cursor-update loop. Only the OS syscall and connection lifecycle edges
    // are controlled; no scheduling delay or TCP stack timing is an assertion.
    let output = region(
        "typedef struct { const char* bytes; size_t length; } KuHttpPart;",
        "\nenum {\n  KU_HTTP_IO_FREE",
    );
    let write = region(
        "static void ku_http_connection_write(",
        "static void ku_http_connection_observe_job(",
    );
    let header = r#"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#if defined(_WIN32)
#include <winsock2.h>
typedef SOCKET KuHttpSocket;
#define FIXTURE_INTERRUPTED WSAEINTR
#define FIXTURE_WOULD_BLOCK WSAEWOULDBLOCK
static int fixture_WSASend(SOCKET,LPWSABUF,DWORD,LPDWORD,DWORD,LPWSAOVERLAPPED,LPWSAOVERLAPPED_COMPLETION_ROUTINE);
#define WSASend fixture_WSASend
#else
#include <sys/socket.h>
#include <sys/uio.h>
typedef int KuHttpSocket;
#define FIXTURE_INTERRUPTED EINTR
#define FIXTURE_WOULD_BLOCK EAGAIN
static ssize_t fixture_sendmsg(int,const struct msghdr*,int);
#define sendmsg fixture_sendmsg
#endif
typedef struct { int unused; } KuStruct___ku_http_response;
#define CHECK(condition) do { if (!(condition)) { fprintf(stderr,"gather check failed at %d: %s\n",__LINE__,#condition); return 97; } } while (0)
static int fixture_mode, fixture_error, fixture_completed, fixture_closed, fixture_close_reason;
static size_t fixture_calls, fixture_emitted, fixture_expected_length, fixture_max_bytes, fixture_max_vectors;
static unsigned long long fixture_now;
static unsigned char fixture_wire[300032], fixture_expected[300032], fixture_body[300000];
"#;
    let edges = r#"
typedef struct { KuHttpSocket socket_value; unsigned long long deadline; KuHttpOutput output; } KuHttpConnection;
typedef struct { int unused; } KuHttpServer;
enum { KU_HTTP_CLOSE_WRITE_DEADLINE=1, KU_HTTP_CLOSE_PEER_WRITE=2 };
static unsigned long long ku_http_now_ms(void) { return fixture_now; }
static int ku_http_socket_last_error(void) { return fixture_error; }
static int ku_http_socket_error_interrupted(int error) { return error==FIXTURE_INTERRUPTED; }
static int ku_http_socket_would_block(int error) { return error==FIXTURE_WOULD_BLOCK; }
static void ku_http_count_transport_close(KuHttpConnection* connection,int reason) { (void)connection; fixture_close_reason=reason; }
static void ku_http_connection_close(KuHttpConnection* connection) { (void)connection; fixture_closed++; }
static void ku_http_connection_next_request(KuHttpConnection* connection,KuHttpServer* server) {
  (void)connection; (void)server; fixture_completed++;
}
static int fixture_transfer(const char* const* buffers,const size_t* lengths,size_t count) {
  if (!count || count>9) abort();
  size_t total=0;
  for (size_t i=0;i<count;i++) {
    if (!lengths[i] || lengths[i]>65536u || total>65536u-lengths[i]) abort();
    if (fixture_emitted+total+lengths[i]>fixture_expected_length) abort();
    if (memcmp(buffers[i],fixture_expected+fixture_emitted+total,lengths[i])) abort();
    total+=lengths[i];
  }
  if (total>fixture_max_bytes) fixture_max_bytes=total;
  if (count>fixture_max_vectors) fixture_max_vectors=count;
  size_t call=fixture_calls++;
  if (fixture_mode==2 || (fixture_mode==1 && (call==0 || call==4))) {
    fixture_error=FIXTURE_INTERRUPTED; return -1;
  }
  if (fixture_mode==1 && (call==2 || call==5)) {
    fixture_error=FIXTURE_WOULD_BLOCK; return -1;
  }
  if (fixture_mode==3) return 0;
  if (fixture_mode==4) { fixture_error=12345; return -1; }
  size_t accepted=fixture_mode==1 && call==1 ? 4u : fixture_mode==1 && call==3 ? 8u : total;
  if (accepted>total) abort();
  size_t remaining=accepted;
  for (size_t i=0;i<count && remaining;i++) {
    size_t bytes=lengths[i]<remaining ? lengths[i] : remaining;
    memcpy(fixture_wire+fixture_emitted,buffers[i],bytes);
    fixture_emitted+=bytes; remaining-=bytes;
  }
  return (int)accepted;
}
#if defined(_WIN32)
static int fixture_WSASend(SOCKET socket_value,LPWSABUF vectors,DWORD count,LPDWORD sent,DWORD flags,
    LPWSAOVERLAPPED overlapped,LPWSAOVERLAPPED_COMPLETION_ROUTINE completion) {
  if (socket_value!=(KuHttpSocket)42 || flags || overlapped || completion || count>9) abort();
  const char* buffers[9]; size_t lengths[9];
  for (size_t i=0;i<count;i++) { buffers[i]=vectors[i].buf; lengths[i]=vectors[i].len; }
  int result=fixture_transfer(buffers,lengths,(size_t)count);
  *sent=result<0 ? 0 : (DWORD)result;
  return result<0 ? SOCKET_ERROR : 0;
}
#else
static ssize_t fixture_sendmsg(int socket_value,const struct msghdr* message,int flags) {
  if (socket_value!=42 || message->msg_iovlen>9) abort();
#if defined(__APPLE__)
  if (flags) abort();
#else
  if (flags!=MSG_NOSIGNAL) abort();
#endif
  const char* buffers[9]; size_t lengths[9];
  for (size_t i=0;i<(size_t)message->msg_iovlen;i++) { buffers[i]=message->msg_iov[i].iov_base; lengths[i]=message->msg_iov[i].iov_len; }
  return (ssize_t)fixture_transfer(buffers,lengths,(size_t)message->msg_iovlen);
}
#endif
static void fixture_reset(KuHttpConnection* connection,int mode) {
  memset(connection,0,sizeof(*connection));
  connection->socket_value=(KuHttpSocket)42; connection->deadline=1000;
  fixture_now=100; fixture_mode=mode; fixture_error=0; fixture_completed=fixture_closed=fixture_close_reason=0;
  fixture_calls=fixture_emitted=fixture_max_bytes=fixture_max_vectors=0;
  fixture_expected_length=0;
}
static void fixture_part(KuHttpConnection* connection,const void* bytes,size_t length) {
  KuHttpOutput* output=&connection->output;
  if (output->count>=9 || fixture_expected_length+length>sizeof(fixture_expected)) abort();
  output->parts[output->count++]=(KuHttpPart){(const char*)bytes,length};
  memcpy(fixture_expected+fixture_expected_length,bytes,length);
  fixture_expected_length+=length;
}
"#;
    let checks = r#"
int main(void) {
  KuHttpConnection connection; KuHttpServer server={0};
  static const unsigned char fixture_prefix_bytes[9]={'H',0,'I','a','b','c','d','e','f'};
  fixture_reset(&connection,0);
  for (size_t i=0;i<9;i++) fixture_part(&connection,fixture_prefix_bytes+i,1u);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_calls==1 && fixture_max_vectors==9 && fixture_emitted==9);
  CHECK(fixture_completed==1 && !fixture_closed && !memcmp(fixture_wire,fixture_prefix_bytes,9));
  CHECK(connection.output.part==9 && !connection.output.offset && connection.deadline==1000);

  for (size_t i=0;i<sizeof(fixture_body);i++) fixture_body[i]=(unsigned char)(i%251u);
  fixture_reset(&connection,1);
  fixture_part(&connection,fixture_prefix_bytes,3); fixture_part(&connection,"header:",7);
  fixture_part(&connection,"J",1); fixture_part(&connection,fixture_body,sizeof(fixture_body));
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_calls==3 && fixture_emitted==4 && !fixture_completed && !fixture_closed);
  CHECK(connection.output.part==1 && connection.output.offset==1 && connection.deadline==1000);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_calls==6 && fixture_emitted==12 && !fixture_completed && !fixture_closed);
  CHECK(connection.output.part==3 && connection.output.offset==1 && connection.deadline==1000);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_emitted==12u+262144u && !fixture_completed && !fixture_closed);
  CHECK(connection.output.part==3 && connection.output.offset==262145u);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_completed==1 && !fixture_closed && fixture_emitted==fixture_expected_length);
  CHECK(connection.output.part==4 && !connection.output.offset && connection.deadline==1000);
  CHECK(fixture_max_bytes==65536u && !memcmp(fixture_wire,fixture_expected,fixture_expected_length));

  fixture_reset(&connection,2); fixture_part(&connection,fixture_prefix_bytes,9);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_calls==32 && !fixture_emitted && !connection.output.part && !connection.output.offset);
  CHECK(!fixture_completed && !fixture_closed && connection.deadline==1000);
  for (int mode=3;mode<=4;mode++) {
    fixture_reset(&connection,mode); fixture_part(&connection,fixture_prefix_bytes,9);
    ku_http_connection_write(&connection,&server);
    CHECK(fixture_calls==1 && fixture_closed==1 && fixture_close_reason==KU_HTTP_CLOSE_PEER_WRITE);
    CHECK(!fixture_emitted && !fixture_completed && !connection.output.part && !connection.output.offset);
  }
  fixture_reset(&connection,0); fixture_part(&connection,fixture_prefix_bytes,9); fixture_now=1000;
  ku_http_connection_write(&connection,&server);
  CHECK(!fixture_calls && fixture_closed==1 && fixture_close_reason==KU_HTTP_CLOSE_WRITE_DEADLINE);
  CHECK(!fixture_emitted && !fixture_completed && connection.deadline==1000);
  puts("gather-write-ok"); return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-gather-write");
    let path = directory.path().join("gather.c");
    fs::write(&path, format!("{header}{output}{edges}{write}{checks}")).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "gather")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires the deterministic native write regression"
        );
        return;
    };
    let output = run_bounded(
        &mut Command::new(&executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .expect("bounded native gather write regression");
    assert!(
        output.status.success(),
        "native gather write regression failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("gather-write-ok"));
}

#[test]
fn native_http_reuses_framed_requests_and_yields_idle_connections() {
    use std::io::Read;
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let source = r#"
import "std.http"
fn main(): null! {
    app = http.server({max_connections: 8, max_active_requests: 1, max_pending_requests: 4,
        idle_timeout_ms: 1000, read_header_timeout_ms: 200, read_body_timeout_ms: 1000,
        write_timeout_ms: 2000, handler_timeout_ms: 2000})
    app.post("/echo/{id}", fn(req) { return http.text(req.params.id + ":" + req.body) })
    app.get("/ok", fn() { return http.text("ok") })
    app.del("/empty", fn() { return http.empty() })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let management_address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "keep-alive",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "1"),
            ("KU_SERVICE_CONTROL_TOKEN", "keepalive-regression"),
            ("KU_SERVICE_PROBE_ADDR", &management_address),
        ],
    ) else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real native compiler"
        );
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let mut stream = connect_with_retry(&address, Duration::from_secs(3));
    // Read exactly one frame; a pipelined successor must remain in the socket.
    let read_frame = |stream: &mut TcpStream| {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(
                Instant::now() < deadline && bytes.len() < 65536,
                "bounded response header"
            );
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        let head = String::from_utf8(bytes.clone()).unwrap();
        let length = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(length < 65536);
        let start = bytes.len();
        bytes.resize(start + length, 0);
        stream.read_exact(&mut bytes[start..]).unwrap();
        String::from_utf8(bytes).unwrap()
    };
    // The first read contains both a body and a complete following request.
    stream.write_all(b"POST /echo/first HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nonePOST /echo/second HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\ntwo").unwrap();
    for body in ["first:one", "second:two"] {
        let response = read_frame(&mut stream);
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(response
            .to_ascii_lowercase()
            .contains("connection: keep-alive\r\n"));
        assert!(response.ends_with(body), "{response}");
    }
    stream
        .write_all(b"DELETE /empty HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert_status(&read_frame(&mut stream), "HTTP/1.1 204 No Content");
    stream.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive, ClOsE\r\nConnection: keep-alive\r\n\r\nGET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let response = read_frame(&mut stream);
    assert!(response
        .to_ascii_lowercase()
        .contains("connection: close\r\n"));
    assert_eq!(
        stream.read(&mut [0; 1]).unwrap(),
        0,
        "close token wins across duplicate fields"
    );

    let mut idle = connect_with_retry(&address, Duration::from_secs(3));
    idle.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert_status(&read_frame(&mut idle), "HTTP/1.1 200 OK");
    let response = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_millis(700),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
    assert_eq!(
        idle.read(&mut [0; 1]).unwrap(),
        0,
        "idle reuse yields its active slot"
    );

    // Fill the pending FIFO while one body is incomplete. Observing the
    // over-limit 503 is a real admission barrier, without scheduling sleeps.
    let mut pipelined = connect_with_retry(&address, Duration::from_secs(3));
    pipelined
        .write_all(b"POST /echo/pressure HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\n")
        .unwrap();
    let mut pending = Vec::new();
    for _ in 0..4 {
        let mut peer = connect_with_retry(&address, Duration::from_secs(3));
        peer.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        pending.push(peer);
    }
    let rejected = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_millis(700),
    );
    assert_status(&rejected, "HTTP/1.1 503 Service Unavailable");
    pipelined
        .write_all(b"oneGET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = read_frame(&mut pipelined);
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("pressure:one"));
    // Closing with unconsumed pipeline bytes can report TCP reset rather than
    // FIN. Neither a timeout nor another response counts as yielding the slot.
    match pipelined.read(&mut [0; 1]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        outcome => panic!("pending peers must win at response boundary: {outcome:?}"),
    }
    for mut peer in pending {
        assert_status(&read_frame(&mut peer), "HTTP/1.1 200 OK");
    }
    // The controlled sequence witnessed a real idle-pressure close, while
    // every pending peer received a complete response. Queue admission was
    // not the cause: capacity pressure does not close the worker queue.
    let metrics = http_response(&management_address,
        "GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: keepalive-regression\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3));
    assert_status(&metrics, "HTTP/1.1 200 OK");
    let closes = metrics
        .split_once("\"transport_closes\":[")
        .unwrap()
        .1
        .split_once(']')
        .unwrap()
        .0;
    let count = |name: &str| -> u64 {
        closes
            .split_once(&format!("\"{name}\":"))
            .unwrap()
            .1
            .split(|character: char| !character.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(count("idle_pressure") >= 1);
    assert!(count("response_policy") >= 1);
    assert_eq!(count("queue_not_admitted"), 0);

    let mut partial = connect_with_retry(&address, Duration::from_secs(3));
    partial
        .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert_status(&read_frame(&mut partial), "HTTP/1.1 200 OK");
    partial
        .write_all(b"POST /echo/timeout HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\nx")
        .unwrap();
    let response = read_frame(&mut partial);
    assert_status(&response, "HTTP/1.1 408 Request Timeout");
    assert!(response
        .to_ascii_lowercase()
        .contains("connection: close\r\n"));
    assert_eq!(partial.read(&mut [0; 1]).unwrap(), 0);
    drop(watchdog);
    let finite_address = unused_local_address();
    let Some(mut finite) =
        spawn_finite_native_server("keep-alive-finite", source, &finite_address, 1)
    else {
        panic!("compiler already available")
    };
    let finite_watchdog = finite.arm_kill_watchdog(RUN_TIMEOUT);
    let mut peer = connect_with_retry(&finite_address, Duration::from_secs(3));
    peer.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert_status(&read_frame(&mut peer), "HTTP/1.1 200 OK");
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    assert!(finite_watchdog
        .wait_for_natural_exit(Duration::from_secs(3))
        .success());
    assert!(!finite_watchdog.timed_out());
}

#[test]
fn native_http_buffer_quota_rejects_aggregate_bodies_and_large_responses_then_recovers() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let address = unused_local_address();
    let source = r#"
import "std.http"
fn main(): null! {
    app = http.server({max_connections: 16, max_active_requests: 8, max_pending_requests: 8,
        max_buffer_bytes: 5000, max_body_bytes: 10000, idle_timeout_ms: 2000,
        read_header_timeout_ms: 2000, read_body_timeout_ms: 2000,
        write_timeout_ms: 2000, handler_timeout_ms: 2000})
    app.post("/echo", fn(req) { return http.text(req.body) })
    app.post("/double", fn(req) { return http.text(req.body + req.body) })
    app.get("/large", fn() { return http.text("__LARGE__" + "!") })
    app.get("/ok", fn() { return http.text("ok") })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let source = source.replace("__LARGE__", &"x".repeat(6000));
    let Some(mut server) = spawn_native_server("buffer-quota", &source, &address) else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real native compiler"
        );
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let mut retained = Vec::new();
    for _ in 0..3 {
        let mut stream = connect_with_retry(&address, Duration::from_secs(3));
        stream.write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1700\r\nConnection: close\r\n\r\nx").unwrap();
        stream.set_nonblocking(true).unwrap();
        retained.push(stream);
    }
    // Socket writes do not acknowledge the reactor's admission order. Observe
    // which partial request was actually rejected before completing the others.
    let deadline = Instant::now() + Duration::from_secs(1);
    let rejected = 'observe: loop {
        assert!(
            Instant::now() < deadline,
            "buffer quota did not reject a request"
        );
        for (index, stream) in retained.iter().enumerate() {
            match stream.peek(&mut [0_u8; 1]) {
                Ok(0) => panic!("buffer quota response closed before its status"),
                Ok(_) => break 'observe index,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("failed to observe buffer quota response: {error}"),
            }
        }
        thread::yield_now();
    };
    let mut rejected = retained.remove(rejected);
    rejected.set_nonblocking(false).unwrap();
    let response =
        String::from_utf8(read_http_stream_bytes_until(&mut rejected, deadline).unwrap()).unwrap();
    assert_status(&response, "HTTP/1.1 503 Service Unavailable");
    assert!(response
        .to_ascii_lowercase()
        .contains("connection: close\r\n"));
    // The budget includes the projected request maps and copied body, as well
    // as network input. Completing both retained bodies under this deliberately
    // tiny budget is no longer a valid success assumption. Disconnect them,
    // then observe actual reclamation with a fresh, smaller request.
    drop(retained);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let request = format!("POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 600\r\nConnection: close\r\n\r\n{}", "x".repeat(600));
        let response = http_response(&address, &request, Duration::from_secs(3));
        if response.starts_with("HTTP/1.1 200 OK") {
            assert_eq!(response.split_once("\r\n\r\n").unwrap().1.len(), 600);
            break;
        }
        assert_status(&response, "HTTP/1.1 503 Service Unavailable");
        assert!(
            Instant::now() < deadline,
            "disconnected input budget was not reclaimed"
        );
        thread::yield_now();
    }
    let request = format!("POST /double HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3000\r\nConnection: close\r\n\r\n{}", "x".repeat(3000));
    let response = http_response(&address, &request, Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 503 Service Unavailable");
    let response = http_response(
        &address,
        "GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&response, "HTTP/1.1 503 Service Unavailable");
    let response = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("ok"));
    drop(watchdog);
}

#[test]
fn native_http_fixed_loops_share_limits_authenticate_control_and_drain() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import "std.http"
fn main(): null! {
    app = http.server({max_active_requests: 16, max_pending_requests: 16,
        max_connections: 64, max_buffer_bytes: 1048576})
    app.get("/ok", fn() { return http.text("ready") })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    for count in [1, 2, 4] {
        let address = unused_local_address();
        let loops = count.to_string();
        let Some(mut server) = spawn_native_server_settings(
            "fixed-loops-control",
            source,
            &address,
            false,
            None,
            &[
                ("KU_EVENT_LOOPS", &loops),
                ("KU_BLOCKING_THREADS", "4"),
                ("KU_SERVICE_CONTROL_TOKEN", "regression-token"),
            ],
        ) else {
            return;
        };
        let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
        for _ in 0..count * 4 {
            let response = http_response(
                &address,
                "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                Duration::from_secs(3),
            );
            assert_status(&response, "HTTP/1.1 200 OK");
            assert!(response.ends_with("ready"));
        }
        let forbidden = http_response(&address,
            "POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", Duration::from_secs(3));
        assert_status(&forbidden, "HTTP/1.1 403 Forbidden");
        let metrics = http_response(&address,
            "GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: regression-token\r\nConnection: close\r\n\r\n", Duration::from_secs(3));
        assert_status(&metrics, "HTTP/1.1 200 OK");
        for index in 0..count {
            assert!(metrics.contains(&format!("\"id\":{index},")), "{metrics}");
        }
        assert!(
            !metrics.contains("\"requests\":0,"),
            "every loop must execute business work: {metrics}"
        );
        let ready = http_response(&address,
            "GET /__ku/health/ready HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: regression-token\r\nConnection: close\r\n\r\n", Duration::from_secs(3));
        assert_status(&ready, "HTTP/1.1 200 OK");
        assert!(ready.ends_with("regression-token"));
        let mut persistent = connect_with_retry(&address, Duration::from_secs(3));
        persistent
            .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
            .unwrap();
        let frame =
            read_http_stream_bytes_until(&mut persistent, Instant::now() + Duration::from_secs(3))
                .unwrap();
        assert_status(&String::from_utf8(frame).unwrap(), "HTTP/1.1 200 OK");
        let drain = http_response(&address,
            "POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: regression-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", Duration::from_secs(3));
        assert_status(&drain, "HTTP/1.1 200 OK");
        // A completed persistent connection cannot admit a new request after
        // the management drain acknowledgement, even on an old poll result.
        let _ = persistent.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let status = watchdog.wait_for_natural_exit(Duration::from_secs(5));
        assert!(status.success(), "drain exit: {status}");
        assert!(!watchdog.timed_out());
        match persistent.peek(&mut [0u8; 1]) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::NotConnected
                ) => {}
            other => {
                panic!("drained keep-alive must close without serving another request: {other:?}")
            }
        }
    }
}

#[test]
#[cfg(target_os = "macos")]
fn native_http_macos_zero_poll_preserves_real_socket_events_and_fallbacks() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = include_str!("../src/backend/c_http_runtime.c");
    let poll = format!(
        "static int ku_http_poll({}",
        runtime
            .rsplit_once("static int ku_http_poll(")
            .expect("POSIX HTTP poll")
            .1
            .split_once("static int ku_http_socket_nonblocking(")
            .unwrap()
            .0
    );
    // Both thin counters call the real OS. Positive revents come from the
    // very poll call made by the helper, avoiding a second readiness snapshot.
    let header = r#"
#include <sys/types.h>
#include <sys/socket.h>
#include <sys/select.h>
#include <sys/resource.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
typedef struct pollfd KuHttpPollFd;
static int poll_calls,select_calls,polled_result,polled_errno;
static short polled_revents[3];
static int counted_poll(struct pollfd* f,nfds_t n,int timeout) {
 poll_calls++; polled_result=poll(f,n,timeout); polled_errno=errno;
 for(nfds_t i=0;i<n && i<3;i++) polled_revents[i]=f[i].revents;
 return polled_result;
}
static int counted_select(int n,fd_set* r,fd_set* w,fd_set* e,struct timeval* t) {
 select_calls++; return select(n,r,w,e,t);
}
#define poll counted_poll
#define select counted_select
"#;
    let checks = r#"
#undef poll
#undef select
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"zero-poll check line %d: %s (errno %d)\n",__LINE__,#c,errno); return 97; } } while(0)
/* mode: 0=negative fast return; 1=select then original poll;
 * 2=unsupported input bypasses select; 3=negative fd may be ignored/fallback;
 * 4=readiness may change between the saturation probe and this call. */
static int check_items(struct pollfd* f,nfds_t n,int timeout,int mode) {
 for(nfds_t i=0;i<n;i++) f[i].revents=0x7fff;
 poll_calls=select_calls=0; errno=EDOM;
 int result=ku_http_poll(f,n,timeout),saved=errno;
 if(mode==0) { CHECK(!result && !poll_calls && select_calls==1 && saved==EDOM); }
 if(mode==1) { CHECK(poll_calls==1 && select_calls==1); }
 if(mode==2) { CHECK(poll_calls==1 && !select_calls); }
 if(mode==4) { CHECK(select_calls==1); }
 if(poll_calls) {
  CHECK(result==polled_result && saved==polled_errno);
  for(nfds_t i=0;i<n;i++) CHECK(f[i].revents==polled_revents[i]);
 } else {
  CHECK(!result && saved==EDOM); for(nfds_t i=0;i<n;i++) CHECK(!f[i].revents);
  if(mode!=4) {
   struct pollfd reference[3]; memcpy(reference,f,n*sizeof(*f));
   CHECK(poll(reference,n,0)==0); /* stable empty reads only */
  }
 }
 return 0;
}
static int check_fd(int fd,short events,int timeout,int mode) {
 struct pollfd item={fd,events,0}; return check_items(&item,1,timeout,mode);
}
static int nonblocking(int fd) {
 int flags=fcntl(fd,F_GETFL,0); return flags<0 ? -1 : fcntl(fd,F_SETFL,flags|O_NONBLOCK);
}
static int bound_socket(int type,struct sockaddr_in* address) {
 int fd=socket(AF_INET,type,0); if(fd<0)return -1;
 memset(address,0,sizeof(*address)); address->sin_family=AF_INET;
 address->sin_addr.s_addr=htonl(INADDR_LOOPBACK);
 if(bind(fd,(struct sockaddr*)address,sizeof(*address))) {close(fd);return -1;}
 socklen_t size=sizeof(*address);
 if(getsockname(fd,(struct sockaddr*)address,&size) || nonblocking(fd)) {close(fd);return -1;}
 return fd;
}
static int wait_ready(int fd,short events) {
 struct pollfd item={fd,events,0}; return poll(&item,1,2000)>0 ? 0 : -1;
}
static int tcp_pair(int pair[2]) {
 struct sockaddr_in address; int listener=bound_socket(SOCK_STREAM,&address);
 CHECK(listener>=0 && listen(listener,4)==0);
 pair[0]=socket(AF_INET,SOCK_STREAM,0); CHECK(pair[0]>=0 && !nonblocking(pair[0]));
 int connected=connect(pair[0],(struct sockaddr*)&address,sizeof(address));
 CHECK(!connected || errno==EINPROGRESS);
 CHECK(!wait_ready(listener,POLLIN)); pair[1]=accept(listener,NULL,NULL); close(listener);
 CHECK(pair[1]>=0 && !nonblocking(pair[1]));
 int yes=1; CHECK(!setsockopt(pair[0],SOL_SOCKET,SO_NOSIGPIPE,&yes,sizeof(yes)));
 CHECK(!setsockopt(pair[1],SOL_SOCKET,SO_NOSIGPIPE,&yes,sizeof(yes))); return 0;
}
int main(void) {
 int pair[2]; char byte;
 CHECK(!tcp_pair(pair));
 CHECK(!check_fd(pair[0],POLLIN,0,0));
 CHECK(!check_fd(pair[0],POLLOUT,0,1));
 CHECK(send(pair[1],"K",1,0)==1 && !wait_ready(pair[0],POLLIN));
 CHECK(!check_fd(pair[0],POLLIN,0,1)); CHECK(recv(pair[0],&byte,1,0)==1 && byte=='K');
 CHECK(!shutdown(pair[1],SHUT_WR) && !wait_ready(pair[0],POLLIN));
 CHECK(!check_fd(pair[0],POLLIN,0,1)); CHECK(recv(pair[0],&byte,1,0)==0);
 close(pair[1]); CHECK(!check_fd(pair[0],POLLIN|POLLOUT,0,1));
 CHECK(!check_fd(pair[0],POLLOUT,0,1)); close(pair[0]);

 /* A bounded full TCP sender remains watched for write/error/hangup. */
 CHECK(!tcp_pair(pair)); int small_buffer=4096;
 CHECK(!setsockopt(pair[0],SOL_SOCKET,SO_SNDBUF,&small_buffer,sizeof(small_buffer)));
 CHECK(!setsockopt(pair[1],SOL_SOCKET,SO_RCVBUF,&small_buffer,sizeof(small_buffer)));
 char payload[8192]={0}; size_t sent=0; int saturated=0;
 for(int turn=0;turn<4096 && sent<8388608u;turn++) {
  ssize_t count=send(pair[0],payload,sizeof(payload),0);
  if(count>0) {sent+=(size_t)count;continue;}
  CHECK(count<0 && (errno==EAGAIN || errno==EWOULDBLOCK));
  struct pollfd item={pair[0],POLLOUT,0};
  if(poll(&item,1,5)==0) {saturated=1;break;}
 }
 CHECK(saturated && sent>0); CHECK(!check_fd(pair[0],POLLOUT,0,4));
 close(pair[1]); CHECK(!wait_ready(pair[0],POLLIN|POLLOUT));
 CHECK(!check_fd(pair[0],POLLIN|POLLOUT,0,1)); close(pair[0]);

 /* UDP wake receiver, and listener readiness without consuming accept. */
 struct sockaddr_in address; int receiver=bound_socket(SOCK_DGRAM,&address);
 int sender=socket(AF_INET,SOCK_DGRAM,0); CHECK(receiver>=0 && sender>=0);
 CHECK(!connect(sender,(struct sockaddr*)&address,sizeof(address)));
 CHECK(!check_fd(receiver,POLLIN,0,0)); CHECK(send(sender,"K",1,0)==1);
 CHECK(!wait_ready(receiver,POLLIN) && !check_fd(receiver,POLLIN,0,1));
 CHECK(recv(receiver,&byte,1,0)==1); CHECK(!check_fd(receiver,POLLIN,0,0));
 int listener=bound_socket(SOCK_STREAM,&address); CHECK(listener>=0 && !listen(listener,4));
 CHECK(!check_fd(listener,POLLIN,0,0)); int client=socket(AF_INET,SOCK_STREAM,0);
 CHECK(client>=0 && !nonblocking(client)); int result=connect(client,(struct sockaddr*)&address,sizeof(address));
 CHECK(!result || errno==EINPROGRESS); CHECK(!wait_ready(listener,POLLIN));
 CHECK(!check_fd(listener,POLLIN,0,1)); int accepted=accept(listener,NULL,NULL);
 CHECK(accepted>=0); close(accepted); close(client); close(listener);

 /* Close a fresh loopback port before connecting: Darwin can keep SYNs
  * pending while a bound socket has not yet called listen. */
 int reservation=bound_socket(SOCK_STREAM,&address); CHECK(reservation>=0);
 close(reservation);
 client=socket(AF_INET,SOCK_STREAM,0); CHECK(client>=0 && !nonblocking(client));
 result=connect(client,(struct sockaddr*)&address,sizeof(address));
 CHECK(result<0 && (errno==EINPROGRESS || errno==ECONNREFUSED));
 int in_progress=errno==EINPROGRESS;
 CHECK(!wait_ready(client,POLLOUT)); CHECK(!check_fd(client,POLLOUT,0,1));
 int error=0; socklen_t length=sizeof(error);
 CHECK(!getsockopt(client,SOL_SOCKET,SO_ERROR,&error,&length));
 CHECK(in_progress ? error==ECONNREFUSED : (error==0 || error==ECONNREFUSED));
 close(client);

 CHECK(!check_fd(receiver,0,0,2)); CHECK(!check_fd(receiver,POLLPRI,0,2));
 CHECK(!check_fd(receiver,POLLIN,1,2)); CHECK(!check_fd(receiver,POLLIN|POLLPRI,0,2));
 int invalid=dup(receiver); CHECK(invalid>=0); close(invalid);
 CHECK(!check_fd(invalid,POLLIN,0,1)); CHECK(!check_fd(-1,POLLIN,0,3));
 struct pollfd mixed[2]={{receiver,POLLIN,0},{-1,POLLOUT,0}};
 CHECK(!check_items(mixed,2,0,0)); CHECK(!check_items(NULL,0,0,2));
 struct rlimit original_limit,raised;
 CHECK(!getrlimit(RLIMIT_NOFILE,&original_limit)); raised=original_limit;
 if(raised.rlim_cur<=FD_SETSIZE) {raised.rlim_cur=FD_SETSIZE+1u;CHECK(raised.rlim_cur<=raised.rlim_max && !setrlimit(RLIMIT_NOFILE,&raised));}
 int high=fcntl(receiver,F_DUPFD,FD_SETSIZE); CHECK(high>=FD_SETSIZE);
 CHECK(!check_fd(high,POLLIN,0,2)); close(high);
 CHECK(!setrlimit(RLIMIT_NOFILE,&original_limit));
 close(receiver); close(sender); puts("macos-zero-poll-ok"); return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-macos-zero-poll");
    let path = directory.path().join("poll.c");
    fs::write(&path, format!("{header}{poll}{checks}")).unwrap();
    let executable = native_c_harness::compile_harness(directory.path(), &path, "poll")
        .expect("macOS socket regression requires the native C compiler");
    let output = run_bounded(
        &mut Command::new(&executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "macOS poll regression failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("macos-zero-poll-ok"));
}

#[test]
fn native_http_live_task_readiness_preserves_completion_cancel_and_owned_lease() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let bridge = include_str!("../src/backend/c_task_http.rs");
    let control = include_str!("../src/backend/c_task_control.rs");
    let host = include_str!("../src/backend/c_task_host.rs");
    let frame = include_str!("../src/backend/c_task.rs");
    let region = |source: &str, start: &str, end: &str| {
        format!(
            "{start}{}",
            source
                .split_once(start)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0
        )
    };
    // Exercise the production bridge, lease validation and storage guard. Only
    // driver/result edges are deterministic stubs; no timing is an assertion.
    let control_types = region(
        control,
        "typedef struct KuTaskControlV1 KuTaskControlV1;",
        "static int ku_task_control_is_payload_terminal(",
    );
    let lease_check = region(
        control,
        "static uint32_t ku_task_control_check_lease(",
        "static uint32_t ku_task_control_init(",
    );
    let storage_check = region(
        frame,
        "static int ku_task_frame_storage_valid(",
        "static int ku_task_frame_ranges_overlap(",
    );
    let value_type = region(
        host,
        "typedef struct KuTaskValueV1 {",
        "typedef struct KuTaskAdapterOutcomeV1 {",
    );
    let handle_type = region(
        bridge,
        "typedef struct KuHttpTaskHandle {",
        "typedef struct KuHttpTaskRuntime {",
    );
    let poll = region(
        bridge,
        "static uint32_t ku_http_task_poll(",
        "static uint32_t ku_http_task_runtime_close(",
    );
    let header = r#"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#if defined(_MSC_VER)
#define KU_TASK_FRAME_ALIGNOF(T) __alignof(T)
#else
#define KU_TASK_FRAME_ALIGNOF(T) _Alignof(T)
#endif
#define KU_TASK_CONTROL_ABI_VERSION 2u
enum { KU_TASK_CONTROL_OK=0, KU_TASK_CONTROL_PENDING=1, KU_TASK_CONTROL_COMPLETED=2,
 KU_TASK_CONTROL_INVALID_ARGUMENT=7, KU_TASK_CONTROL_ABI_MISMATCH=8,
 KU_TASK_CONTROL_LIVE=16, KU_TASK_CONTROL_REQUESTED_CANCEL=19, KU_TASK_CONTROL_COMMITTING=21 };
enum { KU_TASK_DRIVER_OK=0, KU_TASK_DRIVER_PENDING=1, KU_TASK_DRIVER_WAIT_READY=2,
 KU_TASK_DRIVER_INVALID_ARGUMENT=3, KU_TASK_DRIVER_INTERNAL=4, KU_TASK_DRIVER_STALE=5 };
enum { KU_TASK_VALUE_EMPTY=0, KU_TASK_VALUE_LIVE=1, KU_TASK_VALUE_INLINE_FAILED=2 };
typedef size_t KuAtomicRefcount;
typedef uint64_t KuTaskControlDeadlineV1;
typedef struct { int payload; } KuStruct___ku_http_response;
typedef struct KuTaskDriverV1 { void* event_context; } KuTaskDriverV1;
typedef struct { KuTaskDriverV1* driver; size_t slot; uint64_t generation; } KuTaskDriverTicketV1;
static int locks,unlocks,phase_reads,takes,drops,releases,adopts,refunds,outcome_drops;
static uint32_t ready_status,drop_status;
static int slot_exists;
static size_t ku_task_control_atomic_load(KuAtomicRefcount* value) { phase_reads++; return *value; }
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"readiness check line %d: %s\n",__LINE__,#c); return 97; } } while(0)
"#;
    let edges = r#"
typedef struct { KuTaskDriverV1 driver; KuHttpTaskHandle* handles; size_t capacity; } KuHttpTaskRuntime;
typedef struct { uint32_t result_kind; union { KuStruct___ku_http_response http_response; } value; } KuTaskAdapterOutcomeV1;
typedef struct { KuTaskAdapterOutcomeV1* output; void* parent; size_t retained_bytes,http_result; } KuTaskAdapterTakeRequestV1;
typedef struct { int unused; } KuTaskDriverSlotV1;
static KuTaskDriverSlotV1 fixture_slot;
static int ku_task_driver_lock(KuTaskDriverV1* d) { (void)d; locks++; return 0; }
static int ku_task_driver_unlock(KuTaskDriverV1* d) { (void)d; unlocks++; return 0; }
static KuTaskDriverSlotV1* ku_task_driver_find(const KuTaskDriverTicketV1* t) { (void)t; return slot_exists ? &fixture_slot : NULL; }
static uint32_t ku_task_driver_wait_ready_locked(KuTaskDriverSlotV1* s) { (void)s; return ready_status; }
static uint32_t ku_task_value_drop(KuTaskValueV1* v,uint64_t deadline) {
 (void)deadline; drops++; if(drop_status==KU_TASK_DRIVER_OK) { *v=(KuTaskValueV1){0}; slot_exists=0; } return drop_status;
}
static uint32_t ku_task_value_take(KuTaskValueV1* v,void* parent,KuTaskAdapterOutcomeV1* out) {
 (void)parent; takes++; out->result_kind=8; out->value.http_response.payload=42; *v=(KuTaskValueV1){0}; return 0;
}
static uint32_t ku_task_driver_take_result(const KuTaskDriverTicketV1* t,const KuTaskControlLeaseV1* l,KuTaskAdapterTakeRequestV1* r) {
 (void)t; (void)l; takes++; r->output->result_kind=8; r->output->value.http_response.payload=42; r->retained_bytes=7; return 0;
}
static KuStruct___ku_http_response ku_result_move_struct___ku_http_response(KuStruct___ku_http_response* r) { KuStruct___ku_http_response v=*r; *r=(KuStruct___ku_http_response){0}; return v; }
static KuStruct___ku_http_response ku_http_response_from_result(KuStruct___ku_http_response r) { return r; }
static KuStruct___ku_http_response ku_move_struct___ku_http_response(KuStruct___ku_http_response* r) { return ku_result_move_struct___ku_http_response(r); }
static void ku_task_outcome_drop(KuTaskAdapterOutcomeV1* r) { (void)r; outcome_drops++; }
static void ku_drop_struct___ku_http_response(KuStruct___ku_http_response* r) { r->payload=0; }
static size_t ku_http_task_response_admit(void* d,KuStruct___ku_http_response* r) { (void)d; (void)r; adopts++; return 17; }
static uint32_t ku_task_driver_scratch_refund(KuTaskDriverV1* d,size_t n) { (void)d; refunds+=(int)n; return 0; }
static void ku_http_task_handle_release(KuHttpTaskRuntime* r,KuHttpTaskHandle* h) { (void)r; releases++; h->used=0; }
"#;
    let checks = r#"
static KuTaskControlV1 control;
static KuHttpTaskHandle handle;
static KuHttpTaskRuntime runtime;
static void reset(void) {
 memset(&control,0,sizeof(control)); memset(&handle,0,sizeof(handle)); memset(&runtime,0,sizeof(runtime));
 control.abi_version=KU_TASK_CONTROL_ABI_VERSION; control.storage_size=sizeof(control); control.phase=KU_TASK_CONTROL_LIVE;
 runtime.handles=&handle; runtime.capacity=1; handle.used=1; handle.value.tag=KU_TASK_VALUE_LIVE;
 handle.value.owner.lease.control=&control; handle.value.ticket.driver=&runtime.driver;
 locks=unlocks=phase_reads=takes=drops=releases=adopts=refunds=outcome_drops=0;
 ready_status=KU_TASK_DRIVER_PENDING; drop_status=KU_TASK_DRIVER_OK; slot_exists=1;
}
int main(void) {
 void* task; KuStruct___ku_http_response response={0}; size_t charge;
 reset(); task=&handle; KuTaskValueV1 before=handle.value;
 for(int i=0;i<128;i++) { charge=99; CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_PENDING); CHECK(charge==0); }
 CHECK(phase_reads==128 && !locks && !unlocks && !takes && !drops && !releases && !adopts);
 CHECK(task==&handle && !memcmp(&before,&handle.value,sizeof(before)));
 control.phase=KU_TASK_CONTROL_COMPLETED; ready_status=KU_TASK_DRIVER_WAIT_READY;
 CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_OK);
 CHECK(!task && response.payload==42 && charge==17 && locks==2 && unlocks==2);
 CHECK(takes==1 && drops==1 && releases==1 && adopts==1 && refunds==7 && outcome_drops==1);
 CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_INVALID_ARGUMENT);
 CHECK(takes==1 && drops==1 && releases==1);
 reset(); task=&handle; drop_status=KU_TASK_DRIVER_PENDING;
 CHECK(ku_http_task_poll(&runtime,&task,1,123,&response,&charge)==KU_TASK_DRIVER_PENDING);
 CHECK(handle.cancelled && drops==1 && !phase_reads && !takes && !releases);
 CHECK(handle.value.owner.lease.control==&control);
 drop_status=KU_TASK_DRIVER_OK;
 CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_OK);
 CHECK(!task && drops==2 && releases==1 && !adopts && !phase_reads);
 reset(); task=&handle; handle.finishing=1;
 CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_OK);
 CHECK(!task && releases==1 && !phase_reads && !takes && !drops);
 reset(); task=&handle; handle.value.tag=KU_TASK_VALUE_INLINE_FAILED;
 CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_OK);
 CHECK(!task && takes==1 && releases==1 && !phase_reads && !locks && !drops);
 for(int mode=0;mode<5;mode++) {
  reset(); task=&handle;
  if(mode==0) handle.value.owner.lease.control=NULL;
  if(mode==1) handle.value.owner.lease.control=(KuTaskControlV1*)(uintptr_t)1;
  if(mode==2) control.abi_version++;
  if(mode==3) control.storage_size--;
  if(mode==4) handle.value.ticket.driver=NULL;
  CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_PENDING);
  CHECK(locks==1 && unlocks==1 && !phase_reads && !takes && !drops && !releases);
 }
 for(int mode=0;mode<2;mode++) {
  reset(); task=&handle; control.phase=mode ? KU_TASK_CONTROL_COMMITTING : KU_TASK_CONTROL_REQUESTED_CANCEL;
  CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_PENDING);
  CHECK(locks==1 && unlocks==1 && phase_reads==1 && !takes && !drops);
 }
 reset(); task=&handle; handle.used=0;
 CHECK(ku_http_task_poll(&runtime,&task,0,123,&response,&charge)==KU_TASK_DRIVER_INVALID_ARGUMENT);
 CHECK(!phase_reads && !locks); puts("live-readiness-ok"); return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-live-readiness");
    let path = directory.path().join("readiness.c");
    fs::write(&path, format!("{header}{control_types}{storage_check}{lease_check}{value_type}{handle_type}{edges}{poll}{checks}")).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "readiness")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires the native readiness regression"
        );
        return;
    };
    let output = run_bounded(
        &mut Command::new(&executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "native readiness regression failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("live-readiness-ok"));
}

#[test]
fn native_http_async_handlers_use_owner_tasks_timers_and_retire_after_timeout() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import { http, time, fs } from "std"
async fn Child(): str! { time.sleep(2)? return ok("Ku " + "世界") }
async fn hello(): HttpResponse! {
    text = (await Child())?
    index=0
    while(index<1000) {index=index+1}
    text=text+""
    return ok(http.text(text))
}
async fn echo(req: HttpRequest): HttpResponse! {
    time.sleep(2)?
    return ok(http.text(req.path.clone()))
}
async fn fields(req: HttpRequest): HttpResponse! {
    time.sleep(2)?
    key="host"
    return ok(http.text(req.params.id + ":" + req.query["q"] + ":" + req.headers[key]))
}
async fn slow(): HttpResponse! { time.sleep(2000)? return ok(http.text("late")) }
async fn disk(): HttpResponse! {
    fs.write("async-owner.txt","disk 中文")?
    text = fs.read("async-owner.txt")?
    return ok(http.text(text))
}
fn main(): null! {
    app = http.server({max_active_requests: 16, max_pending_requests: 16,
        max_connections: 64, max_buffer_bytes: 2097152, handler_timeout_ms: 50})
    route=hello
    app.get("/hello", route)
    app.get("/echo", echo)
    app.get("/fields/{id}", fields)
    app.get("/slow", slow)
    app.get("/disk", disk)
    app.get("/sync", fn() { return http.text("sync") })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "async-http-owner",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "2"),
            ("KU_BLOCKING_THREADS", "3"),
            ("KU_SERVICE_CONTROL_TOKEN", "async-regression"),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    for (path, body) in [
        ("/hello", "Ku 世界"),
        ("/echo", "/echo"),
        ("/fields/one?q=value", "one:value:localhost"),
        ("/disk", "disk 中文"),
        ("/sync", "sync"),
    ] {
        let response = http_response(
            &address,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            Duration::from_secs(3),
        );
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(response.ends_with(body), "{response}");
    }
    let response = http_response(
        &address,
        "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&response, "HTTP/1.1 504 Gateway Timeout");
    let recovered = http_response(
        &address,
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&recovered, "HTTP/1.1 200 OK");
    assert!(recovered.ends_with("Ku 世界"));
    let response=http_response(&address,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: async-regression\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 200 OK");
    let exit = watchdog.wait_for_natural_exit(Duration::from_secs(5));
    assert!(exit.success(), "async service drain exit: {exit}");
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_async_tcp_waits_share_owner_loops_without_occupying_blocking_worker() {
    use std::io::Read;
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    const CLIENTS: usize = 24;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let source = format!(
        r#"
import {{http,net,bytes}} from "std"
async fn network(): HttpResponse! {{
    client=net.client({{host:"127.0.0.1",port:{port},tls:false,read_timeout_ms:3000}})?
    request=bytes.from_str("A\0B")?
    client.write(request)?
    reply=client.read(1)?
    code=reply.get(0)?
    client.close()
    if(code!=75) {{ fail "unexpected TCP reply" }}
    return ok(http.text("network"))
}}
async fn timeout(): HttpResponse! {{
    client=net.client({{host:"127.0.0.1",port:{port},tls:false,read_timeout_ms:40}})?
    code="missing_timeout"
    try {{reply=client.read(1)?}} catch(err) {{code=err.code.clone()}}
    client.close()
    return ok(http.text(code))
}}
fn main(): null! {{
    app=http.server({{max_active_requests:64,max_pending_requests:64,max_connections:128,
      max_buffer_bytes:4194304,handler_timeout_ms:4000}})
    app.get("/network",network)
    app.get("/timeout",timeout)
    app.listen("__ADDRESS__")?
    return ok(null)
}}
"#
    )
    .replace("\\0", "\0");
    let address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "async-tcp-owner",
        &source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "2"),
            ("KU_BLOCKING_THREADS", "3"),
            ("KU_SERVICE_CONTROL_TOKEN", "net-regression"),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let peer = std::thread::spawn(move || {
        let accept_next = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        return stream;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("bounded TCP accept: {error}"),
                }
            }
        };
        for _ in 0..2 {
            let mut streams = Vec::new();
            for _ in 0..CLIENTS {
                let mut stream = accept_next();
                let mut message = [0; 3];
                stream.read_exact(&mut message).unwrap();
                assert_eq!(&message, b"A\0B");
                streams.push(stream);
            }
            // No reply until every request is waiting. A blocking read on the
            // single DNS worker would prevent the other clients entering.
            for mut stream in streams {
                stream.write_all(b"K").unwrap();
            }
        }
        let mut timed_out = accept_next();
        let mut byte = [0];
        assert_eq!(
            timed_out.read(&mut byte).unwrap(),
            0,
            "timeout must close the physical socket"
        );
        drop(timed_out);
        let mut recovered = accept_next();
        let mut message = [0; 3];
        recovered.read_exact(&mut message).unwrap();
        assert_eq!(&message, b"A\0B");
        recovered.write_all(b"K").unwrap();
    });
    for _ in 0..2 {
        let requests: Vec<_> = (0..CLIENTS)
            .map(|_| {
                let address = address.clone();
                std::thread::spawn(move || {
                    let response = http_response(
                        &address,
                        "GET /network HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n",
                        Duration::from_secs(6),
                    );
                    assert_status(&response, "HTTP/1.1 200 OK");
                    assert!(response.ends_with("network"), "{response}");
                })
            })
            .collect();
        for request in requests {
            request.join().unwrap();
        }
    }
    let timed_out = http_response(
        &address,
        "GET /timeout HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&timed_out, "HTTP/1.1 200 OK");
    assert!(timed_out.ends_with("read_timeout"), "{timed_out}");
    let recovered = http_response(
        &address,
        "GET /network HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&recovered, "HTTP/1.1 200 OK");
    assert!(recovered.ends_with("network"));
    peer.join().unwrap();
    let response=http_response(&address,"POST /__ku/drain HTTP/1.1\r\nHost: local\r\nX-Ku-Control-Token: net-regression\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_async_handler_signature_stubs_cannot_execute_as_sync_calls() {
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let directory = Cleanup(unique_temp_dir("async-handler-call-rejection"));
    for (label, operation) in [
        ("direct", "task=ready()"),
        ("alias", "handler=ready; task=handler()"),
        ("clone", "handler=ready.clone(); task=handler()"),
    ] {
        let source = format!(
            r#"
import http from "std.http"
async fn ready(): HttpResponse! {{return ok(http.text("async"))}}
fn main(): null! {{
  app=http.server()
  {operation}
  app.get("/",ready)
  return ok(null)
}}
"#
        );
        let path = directory.0.join(format!("{label}.ku"));
        fs::write(&path, source).unwrap();
        let artifact = directory.0.join(format!("{label}.c"));
        let mut command = Command::new(ku_binary());
        command
            .current_dir(&directory.0)
            .env("KU_CC", "")
            .args(["build", "--native"])
            .arg(&path)
            .arg("-o")
            .arg(&artifact);
        let output = run_bounded(&mut command, RUN_TIMEOUT, BUILD_OUTPUT_LIMITS).unwrap();
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "{label} executed a signature stub: {message}"
        );
        assert!(!artifact.exists(), "{label} produced a native artifact");
        assert!(
            message.contains("cannot call, capture or pass that handler"),
            "{label}: {message}"
        );
    }
}
