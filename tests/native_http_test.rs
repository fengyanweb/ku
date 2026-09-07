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

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use std::env;
use std::fs;
#[cfg(not(unix))]
use std::io::Read;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    let dir = unique_temp_dir(name);
    let entry = "server.ku";
    fs::write(dir.join(entry), source.replace("__ADDRESS__", address)).expect("write ku source");
    let out = exe_name("server");
    let mut command = Command::new(ku_binary());
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
    let child = match Command::new(&exe)
        .current_dir(&dir)
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
    dir: Option<PathBuf>,
    c_source: PathBuf,
}

struct NativeServerWatchdog {
    cancel: Option<mpsc::Sender<()>>,
    timed_out: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    dir: Option<PathBuf>,
}

impl NativeServerWatchdog {
    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
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
        let worker = thread::spawn(move || {
            let started = Instant::now();
            loop {
                if let Ok(Some(_)) = child.try_wait() {
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
            worker: Some(worker),
            dir: Some(dir),
        }
    }
}

impl Drop for NativeHttpServer {
    fn drop(&mut self) {
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
    assert!(generated.contains("poll(&descriptor, 1, wait_ms)"));
    assert!(generated.contains("send(socket_value, data, chunk, MSG_NOSIGNAL)"));
    assert!(generated.contains("setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE"));
    assert!(generated.contains("pthread_create(&worker, NULL, ku_http_worker, &ctx)"));
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
    owned = "owned-" + "payload"
    try {
        spin()
    } finally {
        // A timeout unwinds into this block. Finish itself returns Result and
        // creates a post-call safepoint; cancellation suppression must let both
        // the helper and the following statement finish.
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
        fs::read_to_string(server.dir().join("finally-one.txt")).expect("first finally marker"),
        "owned-payload",
        "timeout must run the helper call in finally before unwinding the handler"
    );
    assert_eq!(
        fs::read_to_string(server.dir().join("finally-two.txt")).expect("second finally marker"),
        "after-helper",
        "a helper post-call safepoint must not truncate the rest of finally"
    );

    // max_active_requests=1 means this can only succeed if the timed-out handler
    // returned, released its connection permit, and made the same worker reusable.
    let ok = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&ok, "HTTP/1.1 200 OK");

    // array.map owns a generated loop around a Ku closure invocation. A mapper
    // timeout must drop the partial result and unwind through the caller rather
    // than pinning the only worker.
    let mapped = http_response(
        &address,
        "GET /map HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&mapped, "HTTP/1.1 504 Gateway Timeout");

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
