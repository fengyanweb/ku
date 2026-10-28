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

#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
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

#[cfg(any(unix, windows))]
fn reset_http_peer(peer: TcpStream) -> Instant {
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    #[cfg(windows)]
    use std::os::windows::io::AsRawSocket;
    #[cfg(unix)]
    {
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    (&linger as *const libc::linger).cast(),
                    std::mem::size_of_val(&linger) as libc::socklen_t,
                )
            },
            0
        );
    }
    #[cfg(windows)]
    {
        // Winsock LINGER contains two u_short fields, unlike POSIX linger.
        #[repr(C)]
        struct WinLinger {
            onoff: u16,
            seconds: u16,
        }
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn setsockopt(
                socket: std::os::windows::io::RawSocket,
                level: i32,
                name: i32,
                value: *const i8,
                length: i32,
            ) -> i32;
            #[link_name = "WSAGetLastError"]
            fn wsa_get_last_error() -> i32;
        }
        const SOL_SOCKET: i32 = 0xffff;
        const SO_LINGER: i32 = 0x0080;
        let linger = WinLinger {
            onoff: 1,
            seconds: 0,
        };
        let result = unsafe {
            setsockopt(
                peer.as_raw_socket(),
                SOL_SOCKET,
                SO_LINGER,
                (&linger as *const WinLinger).cast(),
                std::mem::size_of_val(&linger) as i32,
            )
        };
        assert_eq!(
            result,
            0,
            "SO_LINGER reset setup failed: WSA error {}",
            unsafe { wsa_get_last_error() }
        );
    }
    let issued = Instant::now();
    drop(peer);
    issued
}

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
    let exe = if settings.iter().any(|(name, _)| {
        matches!(
            *name,
            "KU_NATIVE_HTTP_TEST_PEER_PAUSE_FILE" | "KU_NATIVE_HTTP_TEST_JOB_GATE"
        )
    }) {
        // Observe the real positive MSG_PEEK -> pause transition without reading
        // pipeline bytes or changing any transport decision. This generated-C
        // witness is used only by the reset regression below, never shipped.
        // Retain cleanup ownership even if witness compilation panics.
        let mut artifacts = NativeHttpServer {
            child: None,
            service: None,
            dir: Some(dir.clone()),
            c_source: c_source.clone(),
        };
        // include_str! preserves the runtime file's checkout line endings.
        // Normalize only this temporary witness copy before exact gate matching.
        let generated = fs::read_to_string(&c_source)
            .expect("read native HTTP C")
            .replace("\r\n", "\n");
        let mut witnessed = generated;
        if settings
            .iter()
            .any(|(name, _)| *name == "KU_NATIVE_HTTP_TEST_PEER_PAUSE_FILE")
        {
            let assignment = "connection->peer_read_paused=1;";
            assert_eq!(
                witnessed.matches(assignment).count(),
                1,
                "unique peer pause transition"
            );
            witnessed = witnessed.replacen(
                assignment,
                &format!(
                    r#"int ku_test_was_paused=connection->peer_read_paused; {assignment}
          if(received>0 && !ku_test_was_paused) {{
            const char* witness=getenv("KU_NATIVE_HTTP_TEST_PEER_PAUSE_FILE");
            if(witness) {{
              FILE* file=fopen(witness,"wb");
              if(file) {{ (void)fwrite(&byte,1,1,file); (void)fclose(file); }}
            }}
          }}"#
                ),
                1,
            );
        }
        if settings
            .iter()
            .any(|(name, _)| *name == "KU_NATIVE_HTTP_TEST_JOB_GATE")
        {
            // Gate only the temporary diagnostic native worker's real retained
            // job reference AFTER the actual handler returned. RST cannot
            // short-circuit this gate as it can a cooperative Ku sleep. The
            // release file is the barrier; the finite 3s wait is only a guard.
            let anchor = "  KuHttpFailure failure=ku_http_failure_current;\n  failure.stage=2;";
            assert_eq!(
                witnessed.matches(anchor).count(),
                1,
                "unique worker completion gate"
            );
            witnessed = witnessed.replacen(
                anchor,
                &format!(
                    r#"
  const char* ku_test_gate=getenv("KU_NATIVE_HTTP_TEST_JOB_GATE");
  if(ku_test_gate) {{
    char path[1024];
    int n=snprintf(path,sizeof(path),"%s.started",ku_test_gate);
    if(n<0 || (size_t)n>=sizeof(path)) _Exit(91);
    FILE* file=fopen(path,"wb");if(!file) _Exit(91);
    if(fputs("worker-retains-real-job\n",file)<0 || fclose(file)) _Exit(91);
    n=snprintf(path,sizeof(path),"%s.release",ku_test_gate);
    if(n<0 || (size_t)n>=sizeof(path)) _Exit(91);
    unsigned long long stop=ku_http_deadline_ms(3000u);
    for(;;) {{
      file=fopen(path,"rb");if(file) {{fclose(file);break;}}
      if(ku_http_now_ms()>=stop) _Exit(92);
#if defined(_WIN32)
      Sleep(1);
#else
      struct timespec pause={{0,1000000L}};nanosleep(&pause,NULL);
#endif
    }}
  }}
{anchor}"#
                ),
                1,
            );
            let raw="  item->kind=KU_HTTP_PENDING_RAW;item->socket_value=fd;item->deadline=deadline;pending->raw_count++;";
            assert_eq!(witnessed.matches(raw).count(), 1, "unique raw P insertion");
            witnessed = witnessed.replacen(
                raw,
                &format!(
                    r#"{raw}
  const char* ku_test_gate=getenv("KU_NATIVE_HTTP_TEST_JOB_GATE");
  if(ku_test_gate) {{
    char path[1024];int n=snprintf(path,sizeof(path),"%s.raw",ku_test_gate);
    if(n<0 || (size_t)n>=sizeof(path)) _Exit(91);
    FILE* file=fopen(path,"wb");if(!file) _Exit(91);
    if(fprintf(file,"%llu %llu\n",deadline,ku_http_now_ms())<0 || fclose(file)) _Exit(91);
  }}"#
                ),
                1,
            );
            let raw_peek = "      if(!ku_http_request_observe_start(connection,server)) {";
            assert_eq!(
                witnessed.matches(raw_peek).count(),
                1,
                "unique raw nonconsuming peek"
            );
            witnessed = witnessed.replacen(
                raw_peek,
                r#"
      int ku_test_raw_ready=ku_http_request_observe_start(connection,server);
      int ku_test_raw_error=ku_http_socket_last_error();
      if(!ku_test_raw_ready) {"#,
                1,
            );
            let transfer="        revisit=1;continue;\n      }\n      connection->phase=KU_HTTP_IO_REQUEST_WAIT;";
            assert_eq!(
                witnessed.matches(transfer).count(),
                1,
                "unique empty RAW to C transfer"
            );
            witnessed = witnessed.replacen(
                transfer,
                r#"
        const char* ku_test_gate=getenv("KU_NATIVE_HTTP_TEST_JOB_GATE");
        if(ku_test_gate && connection->phase==KU_HTTP_IO_HEADER
            && connection->socket_value!=KU_HTTP_INVALID_SOCKET) {
          char path[1024];int n=snprintf(path,sizeof(path),"%s.transfer",ku_test_gate);
          if(n<0 || (size_t)n>=sizeof(path)) _Exit(91);
          FILE* file=fopen(path,"wb");if(!file) _Exit(91);
          if(fprintf(file,"%llu %llu %llu %d %d %d\n",connection->request_deadline,
              connection->deadline,ku_http_now_ms(),connection->request_lease,
              connection->header_started,ku_http_socket_would_block(ku_test_raw_error))<0 || fclose(file)) _Exit(91);
        }
        revisit=1;continue;
      }
      connection->phase=KU_HTTP_IO_REQUEST_WAIT;"#,
                1,
            );
            let status="static void ku_http_connection_status(KuHttpConnection* connection, KuHttpServer* server, int status) {";
            assert_eq!(
                witnessed.matches(status).count(),
                1,
                "unique status boundary"
            );
            witnessed=witnessed.replacen(status,&format!(r#"{status}
  const char* ku_test_gate=getenv("KU_NATIVE_HTTP_TEST_JOB_GATE");
  if(ku_test_gate && status==408 && connection->phase==KU_HTTP_IO_HEADER && !connection->request_lease) {{
    char path[1024];int n=snprintf(path,sizeof(path),"%s.timeout",ku_test_gate);
    if(n<0 || (size_t)n>=sizeof(path)) _Exit(91);
    FILE* file=fopen(path,"wb");if(!file) _Exit(91);
    if(fprintf(file,"%llu %llu %llu\n",connection->request_deadline,connection->deadline,
        ku_http_now_ms())<0 || fclose(file)) _Exit(91);
  }}"#),1);
        }
        let witness_source = dir.join("server-peer-pause-witness.c");
        fs::write(&witness_source, witnessed).expect("write temporary peer pause witness");
        let executable =
            native_c_harness::compile_harness(&dir, &witness_source, "server-peer-pause-witness")
                .expect("the native server compiler must also build its read-only witness");
        artifacts.dir = None;
        executable
    } else {
        dir.join(&out)
    };
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
                    last = format!("TCP connect to {address} consumed the response deadline");
                    break;
                }
                stream
                    .set_read_timeout(Some(remaining.min(Duration::from_millis(700))))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(remaining.min(Duration::from_millis(700))))
                    .expect("write timeout");
                if let Err(error) = stream.write_all(request) {
                    last = format!("HTTP request write to {address}: {error}");
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

    for (length, expected) in [
        ("18446744073709551615", "HTTP/1.1 413 Content Too Large"),
        ("18446744073709551616", "HTTP/1.1 400 Bad Request"),
    ] {
        let response = http_response(
            &address,
            &format!("POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"),
            Duration::from_secs(3),
        );
        assert_status(&response, expected);
    }

    let padded_length = http_response(
        &address,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\ncOnTeNt-LeNgTh:\t000000000000000000000004 \t\r\nConnection: close\r\n\r\nsame",
        Duration::from_secs(3),
    );
    assert_status(&padded_length, "HTTP/1.1 200 OK");
    assert!(padded_length.ends_with("\r\n\r\nsame"));

    // Numeric overflow is reported after the complete field validation, while
    // an invalid digit is rejected as soon as its field is encountered.
    for (length, expected) in [
        ("18446744073709551616", "HTTP/1.1 417 Expectation Failed"),
        ("18446744073709551616x", "HTTP/1.1 400 Bad Request"),
    ] {
        let response = http_response(
            &address,
            &format!("POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {length}\r\nExpect: 100-continue\r\n\r\n"),
            Duration::from_secs(3),
        );
        assert_status(&response, expected);
    }

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

    // Preserve target/field error precedence when length parsing shares the
    // first validation pass: pure overflow remains later than target length.
    for (length, expected) in [
        ("18446744073709551616", "HTTP/1.1 414 URI Too Long"),
        ("18446744073709551616x", "HTTP/1.1 400 Bad Request"),
    ] {
        let response = http_response(
            &address,
            &format!("POST /{over} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"),
            Duration::from_secs(3),
        );
        assert_status(&response, expected);
    }

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
    app.max_connections = 32769
    app.listen("127.0.0.1:0")?
    return ok(null)
}"#,
            "32768",
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
fn native_http_dense_slots_preserve_iteration_and_reject_stale_poll_snapshots() {
    let runtime = include_str!("../src/backend/c_http_runtime.c").replace("\r\n", "\n");
    let region = |start: &str, end: &str| {
        format!(
            "{start}{}",
            runtime
                .split_once(start)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0
        )
    };
    let active_type = region(
        "typedef struct KuHttpActiveSet {",
        "typedef struct KuHttpConnection {",
    );
    let functions = region(
        "static int ku_http_free_slot(",
        "enum {\n  KU_HTTP_CLOSE_IDLE_PRESSURE",
    );
    let poll_type = "typedef struct { int slot; uint64_t generation; } KuHttpPollConnection;";
    assert!(runtime.contains(poll_type));
    let header = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stddef.h>
#include <string.h>
typedef int KuHttpSocket;
#define KU_HTTP_INVALID_SOCKET (-1)
"#;
    // These helpers read only the owner index metadata. The existing real HTTP
    // tests exercise socket/job/Task retirement; no synthetic destructor or
    // alternate completion algorithm is supplied by this index regression.
    let connection = r#"
typedef struct KuHttpConnection {
  KuHttpActiveSet* active_set;
  int active_position,free_next,poll_index,socket_value;
  uint64_t connection_generation;
} KuHttpConnection;
"#;
    let checks = r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"dense slot line %d: %s\n",__LINE__,#c);return 1;}}while(0)
static int fixture_zero(const void* raw,size_t length) {
  const unsigned char* bytes=(const unsigned char*)raw;
  for(size_t i=0;i<length;i++) if(bytes[i]) return 0;
  return 1;
}
int main(int argc,char** argv) {
  enum {CAPACITY=4096};
  KuHttpConnection* connections=(KuHttpConnection*)calloc(CAPACITY,sizeof(*connections));
  int* indices=(int*)calloc(CAPACITY,sizeof(int));
  CHECK(connections && indices);
  KuHttpActiveSet active={connections,indices,0,CAPACITY,-1,0,0};
  CHECK(fixture_zero(connections,CAPACITY*sizeof(*connections)));
  CHECK(!active.count && !active.high_water && active.free_head==-1);
  KuHttpPollConnection untouched={0,0};
  CHECK(!ku_http_polled_connection(&active,&untouched,0,0));
  CHECK(ku_http_free_slot(&active)==0 && !active.high_water);
  if(argc>1 && !strcmp(argv[1],"wrong-owner")) {
    KuHttpConnection foreign={0};
    ku_http_active_begin(&active,&foreign);return 94;
  }
  if(argc>1 && !strcmp(argv[1],"skip-fresh-slot")) {
    ku_http_active_begin(&active,&connections[1]);return 94;
  }
  for(int i=0;i<6;i++) {
    CHECK(ku_http_free_slot(&active)==i); ku_http_active_begin(&active,&connections[i]);
    CHECK(connections[i].socket_value==KU_HTTP_INVALID_SOCKET);
    connections[i].connection_generation=10u+(uint64_t)i;
    connections[i].socket_value=100+i; connections[i].poll_index=20+i;
  }
  CHECK(active.count==6 && active.free_head==-1 && active.high_water==6);
  CHECK(fixture_zero(connections+6,(CAPACITY-6)*sizeof(*connections)));
  if(argc>1 && !strcmp(argv[1],"duplicate-admission")) {
    ku_http_active_begin(&active,&connections[0]);return 94;
  }
  /* Descriptor zero is legal once this slot is admitted. Untouched zeroed
     entries remain outside every active/poll/drain traversal. */
  connections[0].socket_value=0;
  KuHttpPollConnection zero_descriptor={0,10u};
  CHECK(ku_http_polled_connection(&active,&zero_descriptor,20,0)==&connections[0]);
  KuHttpPollConnection never_admitted={6,0};
  CHECK(!ku_http_polled_connection(&active,&never_admitted,0,0));
  KuHttpPollConnection snapshot={1,11u};
  CHECK(ku_http_polled_connection(&active,&snapshot,21,101)==&connections[1]);
  CHECK(!ku_http_polled_connection(&active,&snapshot,20,101));
  CHECK(!ku_http_polled_connection(&active,&snapshot,21,102));
  ku_http_active_retire(&connections[1]);
  CHECK(active.count==5 && active.indices[1]==5 && connections[5].active_position==1);
  CHECK(!ku_http_polled_connection(&active,&snapshot,21,101));
  CHECK(active.free_head==1 && ku_http_free_slot(&active)==1);
  ku_http_active_begin(&active,&connections[1]);
  connections[1].connection_generation++;
  CHECK(!ku_http_polled_connection(&active,&snapshot,21,101));
  snapshot.generation++;
  CHECK(ku_http_polled_connection(&active,&snapshot,21,101)==&connections[1]);
  KuHttpActiveSet foreign_owner={0};
  connections[1].active_set=&foreign_owner;
  CHECK(!ku_http_polled_connection(&active,&snapshot,21,101));
  connections[1].active_set=&active;
  KuHttpPollConnection invalid={CAPACITY,0};
  CHECK(!ku_http_polled_connection(&active,&invalid,0,0));
  unsigned seen=0;
  for(int position=active.count;position-->0;) {
    int slot=active.indices[position];
    CHECK(slot>=0 && slot<6 && !(seen&(1u<<slot)));
    seen|=1u<<slot;
    if(slot==1 || slot==3 || slot==0) ku_http_active_retire(&connections[slot]);
  }
  CHECK(seen==63u && active.count==3);
  for(int position=active.count;position-->0;) ku_http_active_retire(&connections[active.indices[position]]);
  CHECK(!active.count && active.high_water==6);
  /* Reusing every slot visits each free entry once, including swapped holes. */
  unsigned char* admitted=(unsigned char*)calloc(CAPACITY,1);
  CHECK(admitted);
  for(int i=0;i<CAPACITY;i++) {
    int slot=ku_http_free_slot(&active); CHECK(slot>=0 && slot<CAPACITY && !admitted[slot]);
    admitted[slot]=1; ku_http_active_begin(&active,&connections[slot]);
  }
  CHECK(active.count==CAPACITY && active.free_head==-1 && active.high_water==CAPACITY);
  CHECK(ku_http_free_slot(&active)==-1);
  for(int position=active.count;position-->0;) ku_http_active_retire(&connections[active.indices[position]]);
  CHECK(!active.count);
  free(admitted);free(indices);free(connections);
  puts("http-dense-slots-poll-generation-ok");return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-dense-slots");
    let path = directory.path().join("dense.c");
    fs::write(
        &path,
        format!("{header}{active_type}{connection}{poll_type}{functions}{checks}"),
    )
    .unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "dense")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    for scenario in ["wrong-owner", "skip-fresh-slot", "duplicate-admission"] {
        let output = run_bounded(
            Command::new(&executable).arg(scenario),
            RUN_TIMEOUT,
            RUN_OUTPUT_LIMITS,
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(1), "{scenario}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("admission invariant failed"),
            "{scenario}: {output:?}"
        );
    }
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "http-dense-slots-poll-generation-ok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_http_sync_body_transfer_preserves_owner_and_budget_handoff() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = include_str!("../src/backend/c_http_runtime.c").replace("\r\n", "\n");
    let input = runtime
        .split_once(
            "typedef struct {\n  KuHttpBufferBudget* budget; size_t body_charge, metadata_charge;",
        )
        .expect("production input layout")
        .1
        .split_once("static KuStruct___ku_http_response ku_http_status_response(")
        .expect("production input lifecycle end")
        .0;
    let execute = runtime
        .split_once("static KuStruct___ku_http_response ku_http_execute_request(")
        .unwrap()
        .1
        .split_once("typedef struct KuHttpQueue")
        .unwrap()
        .0;
    assert!(execute.contains("req.body = ku_http_input_take_body(input);"));
    assert!(!execute.contains("req.body = ku_http_string_copy("));
    let header = r#"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define KU_STRING_OWNED 1
typedef struct { uint8_t* ptr; size_t len,capacity; uint8_t storage; } KuString;
typedef struct { size_t current; } KuHttpBufferBudget;
typedef struct { int unused; } KuObject;
static size_t allocations,body_frees;
static uintptr_t body_identity;
static void* fixture_malloc(size_t bytes) { allocations++; return malloc(bytes); }
static void fixture_free(void* pointer) {
  if(pointer && (uintptr_t)pointer==body_identity && ++body_frees!=1) abort();
  free(pointer);
}
#define malloc fixture_malloc
#define free fixture_free
static void ku_object_drop(KuObject* value) { free(value); }
static void ku_http_buffer_refund(KuHttpBufferBudget* budget,size_t bytes) {
  if(!bytes) return;
  if(!budget || bytes>budget->current) abort();
  budget->current-=bytes;
}
static void fixture_drop_string(KuString* string) {
  if(string->storage==KU_STRING_OWNED) free(string->ptr);
  *string=(KuString){0};
}
#define CHECK(value) do { if(!(value)) { fprintf(stderr,"body handoff check %d: %s\n",__LINE__,#value); return 97; } } while(0)
typedef struct {
  KuHttpBufferBudget* budget; size_t body_charge, metadata_charge;
"#;
    let checks = r#"
int main(void) {
  const unsigned char payload[]={'K',0,0xe4,0xb8,0xad,'!'};
  for(int repeat=0;repeat<16;repeat++) for(int mode=0;mode<3;mode++) {
    allocations=body_frees=0;body_identity=0;
    KuHttpBufferBudget budget={sizeof(payload)*(mode==2 ? 1u : 2u)+13u};
    KuHttpInput input={0};input.budget=&budget;input.metadata_charge=13u;
    input.body=(char*)malloc(sizeof(payload));CHECK(input.body);
    body_identity=(uintptr_t)input.body;memcpy(input.body,payload,sizeof(payload));
    input.body_len=input.body_charge=sizeof(payload);
    if(mode==2) {
      /* Unadmitted or incomplete input has never transferred ownership. */
      ku_http_input_drop(&input);CHECK(body_frees==1 && !budget.current);
    } else {
      KuString request_body=ku_http_input_take_body(&input);
      CHECK((uintptr_t)request_body.ptr==body_identity && !input.body && allocations==1);
      CHECK(request_body.len==sizeof(payload) && request_body.capacity==sizeof(payload));
      CHECK(request_body.storage==KU_STRING_OWNED && !memcmp(request_body.ptr,payload,sizeof(payload)));
      CHECK(input.body_len==sizeof(payload) && input.body_charge==sizeof(payload));
      if(mode==0) {
        /* A successful echo moves the owner into the response. Projection
           stays charged across input refund, then response admission. */
        KuString response=request_body;request_body=(KuString){0};
        ku_http_input_drop(&input);
        CHECK(!body_frees && budget.current==sizeof(payload));
        CHECK(!memcmp(response.ptr,payload,sizeof(payload)));
        budget.current+=response.capacity;
        ku_http_buffer_refund(&budget,sizeof(payload));
        CHECK(budget.current==response.capacity);
        size_t response_charge=response.capacity;fixture_drop_string(&response);
        ku_http_buffer_refund(&budget,response_charge);
      } else {
        /* Handler failure/expiry drops the owned request before input refund. */
        fixture_drop_string(&request_body);CHECK(body_frees==1);
        ku_http_input_drop(&input);CHECK(budget.current==sizeof(payload));
        ku_http_buffer_refund(&budget,sizeof(payload));
      }
      CHECK(body_frees==1 && !budget.current);
    }
    ku_http_input_drop(&input);CHECK(body_frees==1 && !budget.current && allocations==1);
  }
  KuHttpInput empty={0};KuString body=ku_http_input_take_body(&empty);
  CHECK(!body.ptr && !body.len && !body.capacity);fixture_drop_string(&body);ku_http_input_drop(&empty);
  puts("http-sync-body-owner-budget-ok");return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-sync-body-owner");
    let path = directory.path().join("body.c");
    fs::write(&path, format!("{header}{input}{checks}")).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "body")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "http-sync-body-owner-budget-ok\n"
    );
    assert!(output.stderr.is_empty());
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
        "static void ku_http_connection_write_limited(",
        "static void ku_http_connection_observe_job(",
    );
    let phases = region(
        "enum {\n  KU_HTTP_IO_FREE = 0",
        "typedef struct KuHttpActiveSet {",
    );
    let pressure = region(
        "      if (pending.raw_count && ku_http_free_slot(&active)<0 && connection->phase == KU_HTTP_IO_HEADER &&",
        "      if ((connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY || connection->phase == KU_HTTP_IO_REQUEST_WAIT) && now >= connection->deadline)",
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
static int fixture_peeks, fixture_peek_result, fixture_peek_error;
static struct { int count,raw_count; } pending;
static int active;
static int ku_http_free_slot(const int* value) {(void)value;return -1;}
static size_t fixture_calls, fixture_emitted, fixture_expected_length, fixture_max_bytes, fixture_max_vectors;
static unsigned long long fixture_now;
static unsigned char fixture_wire[300032], fixture_expected[300032], fixture_body[300000];
"#;
    let edges = r#"
typedef struct {
  KuHttpSocket socket_value; unsigned long long deadline; KuHttpOutput output;
  int phase,reused,request_lease; size_t buffered,carry_length;
} KuHttpConnection;
typedef struct { int unused; } KuHttpServer;
enum { KU_HTTP_CLOSE_WRITE_DEADLINE=1, KU_HTTP_CLOSE_PEER_WRITE=2, KU_HTTP_CLOSE_IDLE_PRESSURE=3 };
static unsigned long long ku_http_now_ms(void) { return fixture_now; }
static int ku_http_socket_last_error(void) { return fixture_error; }
static int ku_http_socket_error_interrupted(int error) { return error==FIXTURE_INTERRUPTED; }
static int ku_http_socket_would_block(int error) { return error==FIXTURE_WOULD_BLOCK; }
static void ku_http_count_transport_close(KuHttpConnection* connection,int reason) { (void)connection; fixture_close_reason=reason; }
static void ku_http_connection_close(KuHttpConnection* connection) { connection->phase=KU_HTTP_IO_FREE; fixture_closed++; }
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
  fixture_peeks=0; fixture_peek_result=-1; fixture_peek_error=FIXTURE_WOULD_BLOCK; pending.count=pending.raw_count=1;
  connection->phase=KU_HTTP_IO_WRITE; connection->reused=1;
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
    // The same production pressure decision is run between output turns and
    // at each admission state. recv is the only controlled readiness edge;
    // the real-socket keep-alive regression below covers the wire lifecycle.
    let pressure_edges = r#"
static int fixture_recv(KuHttpSocket socket_value,char* byte,size_t length,int flags) {
  if(socket_value!=(KuHttpSocket)42 || length!=1u || flags!=MSG_PEEK) abort();
  fixture_peeks++; fixture_error=fixture_peek_error;
  if(fixture_peek_result>0) *byte='G';
  return fixture_peek_result;
}
#define recv fixture_recv
static void fixture_pressure(KuHttpConnection* connection) {
"#;
    let pressure_end = "}\n#undef recv\n";
    let checks = r#"
int main(void) {
  KuHttpConnection connection; KuHttpServer server={0};
  static const unsigned char fixture_prefix_bytes[9]={'H',0,'I','a','b','c','d','e','f'};
  /* The first-write opportunity is one syscall and16KiB, not an unbounded
     flush. The original readiness cursor/deadline must resume it exactly. */
  for(size_t i=0;i<sizeof(fixture_body);i++) fixture_body[i]=(unsigned char)(i%251u);
  fixture_reset(&connection,0); fixture_part(&connection,fixture_body,sizeof(fixture_body));
  ku_http_connection_write_limited(&connection,&server,1,(size_t)16384);
  CHECK(fixture_calls==1 && fixture_emitted==16384u && !fixture_completed && !fixture_closed);
  CHECK(connection.output.part==0 && connection.output.offset==16384u && connection.deadline==1000);
  ku_http_connection_write(&connection,&server);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_completed==1 && !fixture_closed && fixture_emitted==sizeof(fixture_body));
  CHECK(!memcmp(fixture_wire,fixture_expected,fixture_expected_length));
  fixture_reset(&connection,2); fixture_part(&connection,fixture_prefix_bytes,9);
  ku_http_connection_write_limited(&connection,&server,1,(size_t)16384);
  CHECK(fixture_calls==1 && !fixture_emitted && !fixture_completed && !fixture_closed);
  CHECK(connection.deadline==1000 && !connection.output.part && !connection.output.offset);
  fixture_reset(&connection,1); fixture_part(&connection,fixture_prefix_bytes,9);
  fixture_calls=2; /* Already-writable observation can still race into EAGAIN. */
  ku_http_connection_write_limited(&connection,&server,1,(size_t)16384);
  CHECK(fixture_calls==3 && !fixture_emitted && !fixture_completed && !fixture_closed);
  CHECK(connection.deadline==1000 && !connection.output.part && !connection.output.offset);
  fixture_reset(&connection,0); fixture_part(&connection,fixture_prefix_bytes,9);fixture_now=1000;
  ku_http_connection_write_limited(&connection,&server,1,(size_t)16384);
  CHECK(!fixture_calls && fixture_closed==1 && fixture_close_reason==KU_HTTP_CLOSE_WRITE_DEADLINE);
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
  fixture_pressure(&connection);
  CHECK(!fixture_peeks && !fixture_closed && connection.phase==KU_HTTP_IO_WRITE);
  CHECK(connection.output.part==1 && connection.output.offset==1);
  ku_http_connection_write(&connection,&server);
  CHECK(fixture_calls==6 && fixture_emitted==12 && !fixture_completed && !fixture_closed);
  CHECK(connection.output.part==3 && connection.output.offset==1 && connection.deadline==1000);
  fixture_pressure(&connection);
  CHECK(!fixture_peeks && !fixture_closed && connection.phase==KU_HTTP_IO_WRITE);
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

  /* A fully read previous response may become idle, but a successor already
     queued in the kernel must survive the pressure decision without a read. */
  fixture_reset(&connection,0); connection.phase=KU_HTTP_IO_HEADER; connection.reused=1;
  pending.count=pending.raw_count=0; fixture_pressure(&connection);
  CHECK(!fixture_peeks && !fixture_closed);
  /* A-only request pressure cannot evict an idle C record. */
  pending.count=1; pending.raw_count=0;fixture_pressure(&connection);
  CHECK(!fixture_peeks && !fixture_closed);
  pending.raw_count=1; fixture_peek_result=1; fixture_pressure(&connection);
  CHECK(fixture_peeks==1 && !fixture_closed && connection.phase==KU_HTTP_IO_HEADER);
  CHECK(!connection.buffered && !connection.carry_length && connection.deadline==1000);
  fixture_peek_result=-1; fixture_peek_error=FIXTURE_INTERRUPTED; fixture_pressure(&connection);
  CHECK(fixture_peeks==2 && !fixture_closed);
  fixture_peek_error=FIXTURE_WOULD_BLOCK; fixture_pressure(&connection);
  CHECK(fixture_peeks==3 && fixture_closed==1 && connection.phase==KU_HTTP_IO_FREE);
  CHECK(fixture_close_reason==KU_HTTP_CLOSE_IDLE_PRESSURE && !fixture_completed);
  fixture_reset(&connection,0); connection.phase=KU_HTTP_IO_HEADER; connection.reused=1;
  fixture_peek_result=0; fixture_pressure(&connection);
  CHECK(fixture_peeks==1 && fixture_closed==1 && fixture_close_reason==KU_HTTP_CLOSE_IDLE_PRESSURE);

  /* Already consumed header/carry bytes, a body, admitted work and pending
     output are not idle. The original deadline and output cursor survive. */
  for(int state=0;state<8;state++) {
    fixture_reset(&connection,0); connection.reused=1;
    if(state<2) { connection.phase=KU_HTTP_IO_HEADER; connection.buffered=state==0; connection.carry_length=state==1; }
    else {
      static const int phases[]={KU_HTTP_IO_BODY,KU_HTTP_IO_JOB,KU_HTTP_IO_ASYNC,
        KU_HTTP_IO_WRITE,KU_HTTP_IO_REUSE_WAIT,KU_HTTP_IO_CLOSED_ASYNC};
      connection.phase=phases[state-2];
    }
    connection.output.part=2; connection.output.offset=7;
    int previous=connection.phase;
    fixture_pressure(&connection);
    CHECK(!fixture_peeks && !fixture_closed && connection.phase==previous);
    CHECK(connection.deadline==1000 && connection.output.part==2 && connection.output.offset==7);
  }
  puts("gather-write-ok"); return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-gather-write");
    let path = directory.path().join("gather.c");
    fs::write(
        &path,
        format!("{header}{output}{phases}{edges}{write}{pressure_edges}{pressure}{pressure_end}{checks}"),
    )
    .unwrap();
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
fn native_http_request_leases_and_shared_pending_fifo_preserve_full_identity() {
    let runtime = include_str!("../src/backend/c_http_runtime.c");
    let leases = runtime
        .split_once("static int ku_http_request_lease_take(")
        .unwrap()
        .1
        .split_once("#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)")
        .unwrap()
        .0;
    let pending = runtime
        .split_once("enum { KU_HTTP_PENDING_FREE=0")
        .unwrap()
        .1
        .split_once("/* A single nonconsuming observation")
        .unwrap()
        .0;
    let header = r#"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define KU_HTTP_INVALID_SOCKET (-1)
#define KU_HTTP_PAUSED_PEER_PROBE_MS 25u
#define KU_HTTP_IO_REQUEST_WAIT 10
#define CHECK(c) do {if(!(c)){fprintf(stderr,"request lease/P FIFO line %d: %s\n",__LINE__,#c);return 1;}}while(0)
typedef int KuHttpSocket;
struct KuHttpConnection;
struct KuHttpPending;
typedef struct {
  struct KuHttpConnection* connections;
  struct KuHttpPending* pending;
  int high_water,request_limit,request_active,request_peak;
} KuHttpActiveSet;
typedef struct KuHttpConnection {
  KuHttpActiveSet* active_set;
  int management,request_lease,request_queued,pending_index,active_position,phase;
  uint64_t connection_generation,request_sequence;
  unsigned long long request_deadline,deadline,request_wait_probe_deadline;
} KuHttpConnection;
static unsigned long long ku_http_deadline_ms(unsigned int delta) {return 100u+delta;}
"#;
    let checks = r#"
int main(void) {
  KuHttpConnection connections[4]={{0}};
  KuHttpActiveSet active={.connections=connections,.high_water=4,.request_limit=1};
  for(int i=0;i<4;i++) {
    connections[i].active_set=&active;connections[i].active_position=i;
    connections[i].connection_generation=UINT64_MAX-100u+(uint64_t)i;
    connections[i].request_sequence=UINT64_MAX-200u+(uint64_t)i;
    connections[i].request_deadline=700u;connections[i].deadline=500u;connections[i].pending_index=-1;
  }
  KuHttpPendingItem items[3]={{0}};
  KuHttpPending pending={.items=items,.cap=3};ku_http_pending_init(&pending,&active);
  CHECK(ku_http_request_lease_take(&connections[0]));
  CHECK(ku_http_request_lease_take(&connections[0]) && active.request_active==1);
  CHECK(!ku_http_request_lease_take(&connections[1]) && active.request_peak==1);
  CHECK(ku_http_pending_push_raw(&pending,100,600u));int raw=pending.head;
  CHECK(ku_http_pending_push_request(&pending,&connections[1]));int middle=connections[1].pending_index;
  CHECK(ku_http_pending_push_request(&pending,&connections[2]));int tail=connections[2].pending_index;
  CHECK(pending.count==3 && pending.raw_count==1 && !ku_http_pending_push_raw(&pending,101,800u));
  CHECK(pending.items[raw].next==middle && pending.items[middle].next==tail);
  CHECK(pending.items[middle].deadline==500u && connections[1].request_deadline==700u);
  ku_http_request_cancel_pending(&connections[1]);
  CHECK(pending.count==2 && pending.raw_count==1 && pending.items[raw].next==tail);
  CHECK(pending.items[tail].previous==raw && connections[1].pending_index==-1);
  ku_http_request_cancel_pending(&connections[1]);CHECK(pending.count==2);
  CHECK(ku_http_pending_push_request(&pending,&connections[3]));
  CHECK(connections[3].pending_index==middle && pending.tail==middle && pending.items[tail].next==middle);
  /* A node's old slot cannot refund or identify a recycled generation or a
     successor request. The same full-width IDs, not numeric fd, are required. */
  CHECK(ku_http_pending_connection(&pending,tail)==&connections[2]);
  connections[2].connection_generation++;
  CHECK(!ku_http_pending_connection(&pending,tail));connections[2].connection_generation--;
  connections[2].request_sequence++;
  CHECK(!ku_http_pending_connection(&pending,tail));connections[2].request_sequence--;
  CHECK(ku_http_pending_connection(&pending,tail)==&connections[2]);
  ku_http_pending_unlink(&pending,raw);CHECK(pending.head==tail && pending.raw_count==0 && pending.count==2);
  ku_http_request_cancel_pending(&connections[2]);ku_http_request_cancel_pending(&connections[3]);
  CHECK(!pending.count && pending.head==-1 && pending.tail==-1);
  ku_http_request_lease_release(&connections[0]);ku_http_request_lease_release(&connections[0]);
  CHECK(!active.request_active && active.request_peak==1);
  CHECK(ku_http_request_lease_take(&connections[1]));ku_http_request_lease_release(&connections[1]);
  for(int i=0;i<128;i++) {
    CHECK(ku_http_pending_push_raw(&pending,200+i,900u));int node=pending.head;
    uint64_t generation=pending.items[node].queue_generation;
    CHECK(generation>3u);ku_http_pending_unlink(&pending,node);
    CHECK(!pending.count && !pending.raw_count && pending.free_head==node);
  }
  pending.generation=UINT64_MAX;CHECK(!ku_http_pending_push_raw(&pending,999,900u));
  puts("http-request-lease-shared-pending-identity-ok");return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-request-leases");
    let path = directory.path().join("leases.c");
    fs::write(&path, format!("{header}static int ku_http_request_lease_take({leases}enum {{ KU_HTTP_PENDING_FREE=0{pending}{checks}")).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "leases")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires native request lease regression"
        );
        return;
    };
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("http-request-lease-shared-pending-identity-ok"));
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
        idle_timeout_ms: 3000, read_header_timeout_ms: 2000, read_body_timeout_ms: 1000,
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
    let metrics_request="GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: keepalive-regression\r\nConnection: close\r\n\r\n";
    let snapshot = || http_response(&management_address, metrics_request, Duration::from_secs(3));
    let loop_count = |metrics: &str, key: &str| -> u64 {
        metrics
            .split_once("\"loops\":[")
            .unwrap()
            .1
            .split_once(']')
            .unwrap()
            .0
            .split_once(&format!("\"{key}\":"))
            .unwrap()
            .1
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let wait_counts = |active: u64, pending: u64| {
        let deadline = Instant::now() + Duration::from_millis(700);
        loop {
            let metrics = snapshot();
            if loop_count(&metrics, "active_requests") == active
                && loop_count(&metrics, "pending") == pending
            {
                return metrics;
            }
            assert!(
                Instant::now() < deadline,
                "A/P transition did not complete: {metrics}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    };
    // A=1 must not be consumed by three untouched first-accept C records.
    let mut initial_idle = Vec::new();
    for _ in 0..3 {
        initial_idle.push(connect_with_retry(&address, Duration::from_secs(3)));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let metrics = snapshot();
        if loop_count(&metrics, "connections") == 3 {
            assert_eq!(loop_count(&metrics, "active_requests"), 0, "{metrics}");
            assert_eq!(loop_count(&metrics, "pending"), 0, "{metrics}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "initial idle C records absent: {metrics}"
        );
        thread::sleep(Duration::from_millis(2));
    }
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
    for peer in &mut initial_idle {
        peer.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        assert_status(&read_frame(peer), "HTTP/1.1 200 OK");
    }
    drop(initial_idle);
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
    idle.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert_status(&read_frame(&mut idle), "HTTP/1.1 200 OK");
    drop(idle); // A-only pressure must leave the prior keep-alive usable.

    // Fill the pending FIFO while one body is incomplete. Observing the
    // over-limit 503 is a real admission barrier, without scheduling sleeps.
    let mut pipelined = connect_with_retry(&address, Duration::from_secs(3));
    pipelined
        .write_all(b"POST /echo/pressure HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\n")
        .unwrap();
    wait_counts(1, 0); // Actual partial-body lease, not an accept-only guess.
    let mut pending = Vec::new();
    for ordinal in 0..4 {
        let mut peer = connect_with_retry(&address, Duration::from_secs(3));
        peer.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        pending.push(peer);
        wait_counts(1, ordinal + 1);
    }
    // Remove a middle P node by an actual RST, then reuse its bounded node
    // and C slot. The head and tail requests must retain their identities.
    #[cfg(any(unix, windows))]
    {
        reset_http_peer(pending.remove(1));
        wait_counts(1, 3);
        let mut replacement = connect_with_retry(&address, Duration::from_secs(3));
        replacement
            .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        pending.push(replacement);
        wait_counts(1, 4);
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
    // The four already queued requests keep FIFO priority, including over
    // a pipelined successor. Its bytes survive while that successor waits.
    for mut peer in pending {
        assert_status(&read_frame(&mut peer), "HTTP/1.1 200 OK");
    }
    assert_status(&read_frame(&mut pipelined), "HTTP/1.1 200 OK");
    drop(pipelined);
    // The A limit was never raised to accommodate C-only idle connections.
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
    assert_eq!(count("idle_pressure"), 0);
    assert!(metrics.contains("\"peak_active_requests\":1"), "{metrics}");
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
    // The budget includes projected maps and a conservative body reservation,
    // as well as network input. Completing both retained bodies under this deliberately
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

#[cfg(target_os = "linux")]
#[test]
fn native_http_fd_table_preparation_is_bounded_and_releases_its_duplicate() {
    let runtime = include_str!("../src/backend/c_http_runtime.c");
    let helper = format!(
        "static void ku_http_prepare_fd_table{}",
        runtime
            .split_once("static void ku_http_prepare_fd_table")
            .expect("production descriptor preparation")
            .1
            .split_once("/* CPU consumed")
            .unwrap()
            .0
    );
    let preparation = runtime
        .find("  ku_http_prepare_fd_table(srv,server->max_connections,group.count);")
        .expect("normal listen prepares its actual configured capacity");
    assert!(
        preparation
            < runtime
                .find("  if (!ku_http_mutex_init(&group.lock))")
                .unwrap()
    );
    let header = r#"
#define _GNU_SOURCE
#include <sys/resource.h>
#include <sys/socket.h>
#include <dirent.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdint.h>
#include <limits.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef int KuHttpSocket;
static void ku_http_socket_close(int fd) { (void)close(fd); }
static int calls,last_min,returned_fd,fail_duplicate;
static int fixture_fcntl(int fd,int command,int minimum) {
  calls++; last_min=minimum;
  if (fail_duplicate) { errno=EMFILE; return -1; }
  returned_fd=fcntl(fd,command,minimum);
  return returned_fd;
}
#define fcntl fixture_fcntl
"#;
    let body = r#"
#undef fcntl
#define CHECK(v) do { if (!(v)) { fprintf(stderr,"line %d: %s\n",__LINE__,#v); return 1; } } while(0)
static int count_max_fd;
static int fd_count(void) {
  DIR* d=opendir("/proc/self/fd"); if (!d) return -1;
  int count=0; struct dirent* e; count_max_fd=0;
  while ((e=readdir(d))!=NULL) if (e->d_name[0]!='.') {
    count++; int fd=atoi(e->d_name); if (fd>count_max_fd) count_max_fd=fd;
  }
  closedir(d); return count;
}
static int fd_size(void) {
  FILE* f=fopen("/proc/self/status","r"); if (!f) return -1;
  char line[256]; int size=-1;
  while (fgets(line,sizeof(line),f)) if (sscanf(line,"FDSize: %d",&size)==1) break;
  fclose(f); return size;
}
int main(void) {
  int socket_fd=socket(AF_INET,SOCK_STREAM,0); CHECK(socket_fd>=0);
  /* Exercise the launcher's sparse-descriptor case on every machine. */
  int inherited_high=fcntl(socket_fd,F_DUPFD_CLOEXEC,64); CHECK(inherited_high>=64);
  int count=fd_count(); CHECK(count>0);
  int initial_max_fd=count_max_fd; CHECK(initial_max_fd<INT_MAX-32);
  errno=EDOM; ku_http_prepare_fd_table(socket_fd,257,2);
  CHECK(errno==EDOM && calls==1 && returned_fd>=last_min);
  CHECK(fd_count()==count && fd_size()>last_min);
  CHECK(fcntl(returned_fd,F_GETFD)==-1 && errno==EBADF);
  CHECK(fcntl(socket_fd,F_GETFD)>=0);
  struct rlimit original; CHECK(!getrlimit(RLIMIT_NOFILE,&original));
  /* Bounded-process/CI launchers may inherit a sparse FD above31. Lower the
     soft limit relative to actual live FDs, so preparation really takes the
     clamp branch instead of correctly skipping an already-larger table. */
  struct rlimit limited=original;
  rlim_t small_limit=(rlim_t)initial_max_fd+32;
  if (limited.rlim_cur>small_limit) limited.rlim_cur=small_limit;
  CHECK(!setrlimit(RLIMIT_NOFILE,&limited));
  errno=ERANGE; ku_http_prepare_fd_table(socket_fd,LLONG_MAX,INT_MAX);
  CHECK(errno==ERANGE && calls==2 && last_min==(int)(limited.rlim_cur-1) && fd_count()==count);
  CHECK(fcntl(returned_fd,F_GETFD)==-1 && errno==EBADF);
  fail_duplicate=1;
  errno=EINTR; ku_http_prepare_fd_table(socket_fd,257,2);
  CHECK(errno==EINTR && calls==3 && fd_count()==count);
  ku_http_prepare_fd_table(socket_fd,0,2); CHECK(calls==3);
  limited.rlim_cur=0; CHECK(!setrlimit(RLIMIT_NOFILE,&limited));
  errno=EDOM; ku_http_prepare_fd_table(socket_fd,257,2);
  CHECK(errno==EDOM && calls==3);
  CHECK(!setrlimit(RLIMIT_NOFILE,&original));
  CHECK(fd_count()==count && fcntl(socket_fd,F_GETFD)>=0 && fcntl(inherited_high,F_GETFD)>=0);
  close(inherited_high); close(socket_fd); puts("http-fd-table-preparation-ok"); return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-fd-table");
    let path = directory.path().join("fd-table.c");
    fs::write(&path, format!("{header}\n{helper}\n{body}")).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "fd-table")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires descriptor regression"
        );
        return;
    };
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"http-fd-table-preparation-ok\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn native_http_accept_fd_wait_does_not_hold_admission_lock_and_drain_closes_returned_fd() {
    if !cfg!(unix) {
        return; // The controlled kernel-wait edge uses pthread trylock and fcntl.
    }
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = native_c_harness::TempDir::new("http-accept-lock");
    let address = unused_local_address();
    let port = address.rsplit_once(':').unwrap().1.parse::<u16>().unwrap();
    let source = r#"
import "std.http"
fn main(): null! {
    app = http.server({max_connections:16,max_active_requests:4,max_pending_requests:4})
    app.get("/ok", fn() { return http.text("ready") })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &address);
    fs::write(directory.path().join("accept.ku"), source).unwrap();
    let emission = run_bounded(
        Command::new(ku_binary())
            .current_dir(directory.path())
            .args(["build", "--native", "accept.ku"]),
        BUILD_TIMEOUT,
        BUILD_OUTPUT_LIMITS,
    )
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&emission.stdout),
        String::from_utf8_lossy(&emission.stderr)
    );
    assert!(emission.status.success(), "C emission failed: {text}");
    let emitted = text
        .lines()
        .find_map(|line| line.strip_prefix("native c ok: "))
        .expect("CLI reports emitted native C");
    let emitted = PathBuf::from(emitted);
    let emitted = if emitted.is_absolute() {
        emitted
    } else {
        directory.path().join(emitted)
    };
    let mut generated = fs::read_to_string(emitted).unwrap();
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    generated = generated.replacen(
        "int main(void) {",
        "static int fixture_application_main(void) {",
        1,
    );
    let hooks = r#"
static pthread_mutex_t fixture_lock=PTHREAD_MUTEX_INITIALIZER;
static KuTestEvent fixture_entered,fixture_release;
static KuHttpLoopGroup* fixture_group;
static int fixture_owner,fixture_calls,fixture_inflight,fixture_peak,fixture_gate_failed;
static int fixture_submits,fixture_exits,fixture_returned_fd=-1;
static unsigned long long fixture_accepted,fixture_owner_accepted[2];
static unsigned long long fixture_turns[2];
static size_t fixture_remaining;
static void fixture_note_turn(KuHttpLoopOwner* owner) {
  pthread_mutex_lock(&fixture_lock);fixture_turns[owner->index]++;pthread_mutex_unlock(&fixture_lock);
}
static void fixture_note_submit(void) {
  pthread_mutex_lock(&fixture_lock);fixture_submits++;pthread_mutex_unlock(&fixture_lock);
}
/* The actual generated accept call stays on its real owner OS thread. The
 * call boundary is gated; accept still returns a real connected descriptor. */
static KuHttpSocket fixture_accept(KuHttpLoopGroup* group,KuHttpLoopOwner* owner,KuHttpSocket listener) {
  pthread_mutex_lock(&fixture_lock);
  int first=++fixture_calls==1;
  fixture_inflight++;if(fixture_inflight>fixture_peak)fixture_peak=fixture_inflight;
  if(first) {fixture_group=group;fixture_owner=owner->index;}
  pthread_mutex_unlock(&fixture_lock);
  if(first) {
    if(!ku_test_event_set(&fixture_entered) || !ku_test_event_wait(&fixture_release,2000u)) {
      pthread_mutex_lock(&fixture_lock);fixture_gate_failed=1;pthread_mutex_unlock(&fixture_lock);
    }
  }
  KuHttpSocket result=accept(listener,NULL,NULL);
  int saved_errno=errno;
  pthread_mutex_lock(&fixture_lock);
  fixture_inflight--;if(result!=KU_HTTP_INVALID_SOCKET)fixture_returned_fd=result;
  pthread_mutex_unlock(&fixture_lock);
  errno=saved_errno;return result;
}
/* Called under the original group lock before the group's stack can retire. */
static void fixture_note_exit(KuHttpLoopOwner* owner) {
  pthread_mutex_lock(&fixture_lock);
  fixture_exits++;fixture_accepted=owner->group->accepted;
  fixture_owner_accepted[owner->index]=owner->accepted;
  fixture_remaining+=owner->resident+owner->connections+owner->active_requests;
  pthread_mutex_unlock(&fixture_lock);
}
"#;
    let loop_edge = "static const char* ku_http_loop_run(KuHttpLoopOwner* owner) {";
    assert_eq!(generated.matches(loop_edge).count(), 1);
    generated = generated.replacen(
        loop_edge,
        &format!(
            "{}\n{hooks}\n{loop_edge}",
            native_c_harness::NATIVE_THREAD_LIFECYCLE_HARNESS
        ),
        1,
    );
    let turn_edge = "    owner->heartbeat = ku_http_now_ms();";
    assert_eq!(generated.matches(turn_edge).count(), 1);
    generated = generated.replacen(
        turn_edge,
        &format!("{turn_edge}\n    fixture_note_turn(owner);"),
        1,
    );
    let submit_edge = "static void ku_http_connection_submit(KuHttpConnection* connection, KuHttpServer* server, KuHttpQueue* queue, int workers) {";
    assert_eq!(generated.matches(submit_edge).count(), 1);
    generated = generated.replacen(
        submit_edge,
        &format!("static void fixture_note_submit(void);\n{submit_edge}\n  fixture_note_submit();"),
        1,
    );
    let accept_edge = "KuHttpSocket cli = accept(srv, NULL, NULL);";
    assert_eq!(generated.matches(accept_edge).count(), 1);
    generated = generated.replacen(
        accept_edge,
        "KuHttpSocket cli = fixture_accept(group,owner,srv);",
        1,
    );
    let exit_edge = "owner->ready = 0; owner->exited = 1;";
    assert_eq!(generated.matches(exit_edge).count(), 1);
    generated = generated.replacen(
        exit_edge,
        &format!("{exit_edge}\n  fixture_note_exit(owner);"),
        1,
    );
    generated.push_str(&format!("\n#define FIXTURE_PORT {port}\n"));
    generated.push_str(r#"
#define EXPECT(c) do {if(!(c)){fprintf(stderr,"accept lock line %d: %s\n",__LINE__,#c);failed=1;}}while(0)
static void fixture_pause(void) {struct timespec pause={0,1000000L};nanosleep(&pause,NULL);}
static int fixture_application(void* unused) {(void)unused;return fixture_application_main();}
static int fixture_connect(void) {
  unsigned long long end=ku_test_real_now_ms()+2000u;
  while(ku_test_real_now_ms()<end) {
    int fd=socket(AF_INET,SOCK_STREAM,IPPROTO_TCP);
    if(fd<0)return -1;
    struct sockaddr_in endpoint={0};endpoint.sin_family=AF_INET;
    endpoint.sin_addr.s_addr=htonl(INADDR_LOOPBACK);endpoint.sin_port=htons(FIXTURE_PORT);
    if(!ku_http_socket_nonblocking(fd) || ku_http_socket_suppress_sigpipe(fd)) {close(fd);return -1;}
    int connected=connect(fd,(struct sockaddr*)&endpoint,sizeof(endpoint))==0;
    if(!connected && errno==EINPROGRESS) {
      struct pollfd poller={fd,POLLOUT,0};int error=0;socklen_t length=sizeof(error);
      connected=poll(&poller,1,10)>0 && !getsockopt(fd,SOL_SOCKET,SO_ERROR,&error,&length) && !error;
    }
    if(connected) {
      const char request[]="GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
      if(send(fd,request,sizeof(request)-1,0)==(ssize_t)(sizeof(request)-1))return fd;
    }
    close(fd);fixture_pause();
  }
  return -1;
}
int main(int argc,char** argv) {
  int failed=0,drain=argc>1 && !strcmp(argv[1],"drain");
  setenv("KU_EVENT_LOOPS","2",1);setenv("KU_BLOCKING_THREADS","2",1);
  setenv("KU_THREAD_BUDGET","64",1);setenv("KU_HTTP_MAX_REQUESTS","2",1);
  unsetenv("KU_SERVICE_PROBE_ADDR");unsetenv("KU_SERVICE_CONTROL_TOKEN");
  if(!ku_test_event_init(&fixture_entered) || !ku_test_event_init(&fixture_release))return 96;
  KuTestThread application;
  if(!ku_test_thread_start(&application,fixture_application,NULL))return 96;
  int first=fixture_connect(),second=fixture_connect();
  EXPECT(first>=0 && second>=0);
  int entered=ku_test_event_wait(&fixture_entered,1500u);EXPECT(entered);
  KuHttpLoopGroup* group=fixture_group;
  int admitted=0,other_progress=0;
  int unlocked=0;
  unsigned long long lock_deadline=ku_test_real_now_ms()+100u;
  while(entered && !unlocked && ku_test_real_now_ms()<lock_deadline) {
    unlocked=!pthread_mutex_trylock(&group->lock);
    if(!unlocked)fixture_pause();
  }
  if(unlocked) {
    int other=1-fixture_owner;
    pthread_mutex_lock(&fixture_lock);
    unsigned long long turns=fixture_turns[other];
    pthread_mutex_unlock(&fixture_lock);
    EXPECT(group->accept_turn==fixture_owner && !group->accepted);
    KuHttpWake* wake=group->loops[other].wake;
    if(wake)ku_http_wake_notify(wake); /* Lifetime remains under group.lock. */
    ku_http_mutex_unlock(&group->lock);
    ku_http_current_loop=&group->loops[other];
    admitted=ku_http_reused_request_admit();ku_http_current_loop=NULL;
    unsigned long long end=ku_test_real_now_ms()+500u;
    while(ku_test_real_now_ms()<end && !other_progress) {
      pthread_mutex_lock(&fixture_lock);
      other_progress=fixture_turns[other]>turns;
      pthread_mutex_unlock(&fixture_lock);
      if(!other_progress)fixture_pause();
    }
    pthread_mutex_lock(&fixture_lock);
    EXPECT(fixture_calls==1 && fixture_inflight==1 && fixture_peak==1);
    pthread_mutex_unlock(&fixture_lock);
    if(drain) {
      ku_http_mutex_lock(&group->lock);
      group->draining=1;group->drain_deadline=ku_http_deadline_ms(2000u);
      ku_http_mutex_unlock(&group->lock);
      ku_http_group_notify(group);
    }
  }
  /* Old code fails these bounded assertions, then is released to finish N=2.
   * No failed trylock path blocks trying to acquire the accept-held mutex. */
  if(!(admitted && other_progress))
    fprintf(stderr,"accept gate entered=%d unlocked=%d admitted=%d other_progress=%d owner=%d\n",
        entered,unlocked,admitted,other_progress,fixture_owner);
  EXPECT(admitted && other_progress);
  EXPECT(ku_test_event_set(&fixture_release));
  int joined=ku_test_thread_join(&application,3000u);EXPECT(joined);
  if(!joined)return 96; /* Process exit bounds even a broken native teardown. */
  EXPECT(application.outcome==0 && !fixture_gate_failed && fixture_exits==2);
  EXPECT(fixture_peak==1 && !fixture_inflight && !fixture_remaining);
  EXPECT(atomic_load_explicit(&ku_http_active_conns,memory_order_relaxed)==0);
  if(drain && admitted) {
    EXPECT(fixture_calls==1 && fixture_accepted==0 && fixture_submits==0);
    EXPECT(fixture_returned_fd>=0 && fcntl(fixture_returned_fd,F_GETFD)==-1 && errno==EBADF);
  } else if(!drain) {
    EXPECT(fixture_accepted==2 && fixture_submits==2);
    EXPECT(fixture_owner_accepted[0]==1 && fixture_owner_accepted[1]==1);
  }
  if(first>=0)close(first);if(second>=0)close(second);
  EXPECT(ku_test_event_destroy(&fixture_entered) && ku_test_event_destroy(&fixture_release));
  if(failed)return 1;
  puts("http-accept-fd-wait-lock-turn-drain-ok");return 0;
}
"#);
    let path = directory.path().join("accept-lock.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) =
        native_c_harness::compile_harness(directory.path(), &path, "accept-lock")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires accept lock regression"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    for mode in ["normal", "drain"] {
        let output = run_bounded(
            Command::new(&executable).arg(mode),
            RUN_TIMEOUT,
            RUN_OUTPUT_LIMITS,
        )
        .unwrap();
        assert!(output.status.success(), "{mode}: {output:?}");
        assert_eq!(output.stdout, b"http-accept-fd-wait-lock-turn-drain-ok\n");
        assert!(output.stderr.is_empty(), "{mode}: {output:?}");
    }
}

#[test]
fn native_http_fixed_loops_share_limits_authenticate_control_and_drain() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import { http, json } from "std"
fn main(): null! {
    config=json.decode("{\"max_active_requests\":8256,\"max_pending_requests\":128,\"max_connections\":8320}")?
    app = http.server({max_active_requests: config["max_active_requests"]?.as_int()?,
        max_pending_requests: config["max_pending_requests"]?.as_int()?,
        max_connections: config["max_connections"]?.as_int()?, max_buffer_bytes: 1048576})
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
        assert!(metrics.contains("\"limits\":{\"max_active_requests\":8256,\"max_connections\":8320,\"max_pending_requests\":128}"),"{metrics}");
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
    // An explicit loop count may exceed C: the empty-C owner still hosts its
    // Task/net/wake resources, while accept rotation skips it without spending
    // an extra C permit or rejecting every other sequential request.
    let source = source
        .replace("max_active_requests\\\":8256", "max_active_requests\\\":2")
        .replace("max_pending_requests\\\":128", "max_pending_requests\\\":2")
        .replace("max_connections\\\":8320", "max_connections\\\":1");
    let address = unused_local_address();
    let management = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "zero-connection-owner",
        &source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "2"),
            ("KU_BLOCKING_THREADS", "4"),
            ("KU_SERVICE_CONTROL_TOKEN", "regression-token"),
            ("KU_SERVICE_PROBE_ADDR", &management),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    for _ in 0..4 {
        let response = http_response(
            &address,
            "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            Duration::from_secs(3),
        );
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(response.ends_with("ready"));
    }
    let metrics=http_response(&management,"GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: regression-token\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&metrics, "HTTP/1.1 200 OK");
    assert!(
        metrics
            .contains("\"max_active_requests\":2,\"max_connections\":1,\"max_pending_requests\":2"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\"id\":1,"),
        "zero-C owner must remain alive: {metrics}"
    );
    let drain=http_response(&management,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: regression-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&drain, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_task_io_batch_bounds_frozen_clocks_and_resets_before_every_wait() {
    let runtime = include_str!("../src/backend/c_http_runtime.c").replace("\r\n", "\n");
    let start = runtime.find("/* Bounded HTTP Task/I/O batch.").unwrap();
    let end = runtime[start..]
        .find("/* End bounded HTTP Task/I/O batch. */")
        .unwrap()
        + start;
    let batch = &runtime[start..end];
    let checks = r#"
/* Exact production helpers with a field-only connection model. */
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#define KU_HTTP_INVALID_SOCKET (-1)
enum {
  KU_HTTP_IO_FREE = 0, KU_HTTP_IO_HEADER = 1, KU_HTTP_IO_BODY = 2,
  KU_HTTP_IO_JOB = 3, KU_HTTP_IO_WRITE = 4, KU_HTTP_IO_CLOSED_JOB = 5,
  KU_HTTP_IO_REJECT = 6, KU_HTTP_IO_REUSE_WAIT = 7,
  KU_HTTP_IO_ASYNC = 8, KU_HTTP_IO_CLOSED_ASYNC = 9
};
/* Classifier needs only these fields, no socket, heap or Task is created. */
typedef struct {
  int phase,reused,socket_value;
  size_t buffered,carry_length;
  void *job,*async_task;
  unsigned long long deadline,request_deadline;
} KuHttpConnection;
"#
    .to_owned()
        + batch
        + r#"
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"Task I/O batch line %d: %s\n",__LINE__,#c); return 97; } } while(0)
static void clean_probe(KuHttpTaskIoBatch* b,unsigned long long t) {
  ku_http_task_io_batch_reset(b); /* Real-loop continue(false), before OS call. */
  ku_http_task_io_batch_probe_result(b,0,0,1,t,10u,33,1);
}
static int idle_probe_checks(void) {
  KuHttpTaskIoBatch b={0};
  CHECK(!ku_http_task_io_batch_probe_matches(&b,10u,33,1));
  ku_http_task_io_batch_begin(&b,1000u);
  CHECK(!ku_http_task_io_batch_continue(&b,1000u,1,1)); /* Missing first proof. */
  CHECK(!b.probe_clean && !b.turns);
  clean_probe(&b,1000u);
  CHECK(b.probe_clean && b.started_ms==1000u && b.turns==1u);
  CHECK(ku_http_task_io_batch_probe_matches(&b,10u,33,1));
  CHECK(!ku_http_task_io_batch_probe_matches(&b,11u,33,1));
  CHECK(!ku_http_task_io_batch_probe_matches(&b,10u,32,1));
  CHECK(!ku_http_task_io_batch_probe_matches(&b,10u,33,0));
  CHECK(!ku_http_task_io_batch_probe_matches(&b,UINT64_MAX,33,1));
  CHECK(!ku_http_task_io_batch_probe_matches(&b,10u,0,1));

  /* A zero poll charges its own turn; the next begin retains its old age. */
  ku_http_task_io_batch_begin(&b,1100u);
  CHECK(b.started_ms==1000u && b.turns==2u);
  CHECK(!ku_http_task_io_batch_continue(&b,1100u,1,0));
  CHECK(!b.probe_clean && !b.turns && !b.started_ms);
  clean_probe(&b,1000u);
  ku_http_task_io_batch_begin(&b,1001u);
  CHECK(!ku_http_task_io_batch_continue(&b,1001u,1,0));
  clean_probe(&b,1000u);
  ku_http_task_io_batch_begin(&b,999u);
  CHECK(!ku_http_task_io_batch_continue(&b,999u,1,0));
  clean_probe(&b,1000u);
  unsigned int skips=0,pumps=1;
  for(unsigned int i=0;i<32;i++) {
    ku_http_task_io_batch_begin(&b,1000u); pumps++;
    if(!ku_http_task_io_batch_continue(&b,1000u,1,0)) break;
    skips++;
  }
  CHECK(skips==6u && pumps==8u && !b.probe_clean && !b.turns);
  ku_http_task_io_batch_reset(&b);
  skips=0;pumps=0;
  for(unsigned int i=0;i<32;i++) { /* Original pure-ASYNC entry remains unchanged. */
    ku_http_task_io_batch_begin(&b,1000u);pumps++;
    if(!ku_http_task_io_batch_continue(&b,1000u,1,0)) break;
    skips++;
  }
  CHECK(skips==7u && pumps==8u);

  /* Caller clears before every real result. Positive, error/EINTR, blocking
     timeout and structurally urgent zero cannot grant a new proof. */
  int waits[]={0,1,-1}; int selecteds[]={0,1,-1,-4};
  for(size_t i=0;i<3;i++) for(size_t j=0;j<4;j++) {
    clean_probe(&b,1000u);
    CHECK(!ku_http_task_io_batch_continue(&b,1000u,1,1));
    ku_http_task_io_batch_probe_result(&b,waits[i],selecteds[j],1,1000u,10u,33,1);
    CHECK(b.probe_clean==(waits[i]==0 && selecteds[j]==0));
  }
  for(int reason=0;reason<8;reason++) { /* pending-seen,wake,NET,drain,
      no-ready,rejection,deadline,noneligible share structural urgency. */
    clean_probe(&b,1000u);
    CHECK(!ku_http_task_io_batch_continue(&b,1000u,reason!=4,1));
    ku_http_task_io_batch_probe_result(&b,0,0,0,1000u,10u,33,1);
    CHECK(!b.probe_clean && !b.turns);
  }
  ku_http_task_io_batch_reset(&b);
  ku_http_task_io_batch_probe_result(&b,0,0,1,1000u,UINT64_MAX,33,1);
  CHECK(!b.probe_clean);
  ku_http_task_io_batch_probe_result(&b,0,0,1,~0ULL,10u,33,1);
  CHECK(!b.probe_clean);

  KuHttpConnection c={.phase=KU_HTTP_IO_HEADER,.reused=1,.socket_value=9,.deadline=1020u};
  CHECK(ku_http_task_io_idle_eligible(&c));
  c.request_deadline=1010u;CHECK(!ku_http_task_io_idle_eligible(&c));c.request_deadline=0;
  c.deadline=0;CHECK(!ku_http_task_io_idle_eligible(&c));
  c.deadline=~0ULL;CHECK(!ku_http_task_io_idle_eligible(&c));c.deadline=1020u;
  CHECK(ku_http_task_io_idle_eligible(&c));
  c.reused=0;CHECK(!ku_http_task_io_idle_eligible(&c));c.reused=1;
  c.buffered=1;CHECK(!ku_http_task_io_idle_eligible(&c));c.buffered=0;
  c.carry_length=1;CHECK(!ku_http_task_io_idle_eligible(&c));c.carry_length=0;
  c.job=&c;CHECK(!ku_http_task_io_idle_eligible(&c));c.job=NULL;
  c.async_task=&c;CHECK(!ku_http_task_io_idle_eligible(&c));c.async_task=NULL;
  c.socket_value=-1;CHECK(!ku_http_task_io_idle_eligible(&c));c.socket_value=9;
  int phases[]={KU_HTTP_IO_BODY,KU_HTTP_IO_WRITE,KU_HTTP_IO_JOB,
      KU_HTTP_IO_REUSE_WAIT,KU_HTTP_IO_CLOSED_ASYNC,KU_HTTP_IO_CLOSED_JOB};
  for(size_t i=0;i<6;i++) {c.phase=phases[i];CHECK(!ku_http_task_io_idle_eligible(&c));}
  c.phase=KU_HTTP_IO_HEADER;
  KuHttpTaskIoEligibility e={~0ULL,1,1,0};
  ku_http_task_io_eligibility_observe_idle(&e,1,0,1100u);
  ku_http_task_io_eligibility_observe_idle(&e,0,ku_http_task_io_idle_eligible(&c),c.deadline);
  CHECK(!e.all_async && e.all_batchable && e.has_idle && e.deadline==1020u);
  CHECK(!(1019u>=e.deadline) && 1020u>=e.deadline);
  CHECK(c.deadline==1020u); /* Qualification cannot renew the original D. */
  ku_http_task_io_eligibility_observe_idle(&e,0,0,0u);
  CHECK(!e.all_batchable);
  ku_http_task_io_eligibility_observe_idle(&e,1,0,1200u);
  CHECK(!e.all_batchable); /* A later valid member cannot undo an invalid one. */
  e=(KuHttpTaskIoEligibility){~0ULL,1,1,0};
  ku_http_task_io_eligibility_observe_idle(&e,1,0,1050u);
  ku_http_task_io_eligibility_observe_idle(&e,1,0,1030u);
  CHECK(e.all_async && e.all_batchable && !e.has_idle && e.deadline==1030u);
  return 0;
}

int main(void) {
  KuHttpTaskIoBatch batch={0};
  /* Test the production obligation helpers, not a copy of the owner loop:
   * forced I/O must suppress early compute, never sleep on a carried ready
   * hint, and survive EINTR or maintenance continues until a real OS return. */
  KuHttpTaskSliceTurn turn={1,0u}; uint64_t carried=UINT64_MAX;
  CHECK(!ku_http_task_slice_needs_pump(&turn,&carried) && carried==0u);
  CHECK(ku_http_task_slice_wait_ms(&turn,-1)==0);
  ku_http_task_slice_observed_io(&turn,-1);
  CHECK(turn.force_io && !ku_http_task_slice_needs_pump(&turn,&carried));
  turn.next_deadline=UINT64_MAX;
  CHECK(!ku_http_task_slice_needs_pump(&turn,&carried) && carried==UINT64_MAX);
  CHECK(ku_http_task_slice_wait_ms(&turn,500)==0);
  ku_http_task_slice_observed_io(&turn,0);
  CHECK(!turn.force_io && ku_http_task_slice_needs_pump(&turn,&carried));
  CHECK(ku_http_task_slice_wait_ms(&turn,-1)==-1);
  turn.force_io=1; ku_http_task_slice_observed_io(&turn,2);
  CHECK(!turn.force_io); /* A positive event also pays the full I/O turn. */
  /* A ready Task makes the shared nearest deadline zero. The same-turn
   * eligibility reduction must retain the real request deadline separately,
   * including connections whose paused peer supplies no poll interest. */
  KuHttpTaskIoEligibility eligibility={UINT64_MAX,1,1,0};
  ku_http_task_io_eligibility_observe(&eligibility,1,1050u);
  ku_http_task_io_eligibility_observe(&eligibility,1,1030u);
  ku_http_task_io_eligibility_observe(&eligibility,1,1040u);
  CHECK(eligibility.all_async && eligibility.deadline==1030u);
  CHECK(!(1029u>=eligibility.deadline));
  CHECK(1030u>=eligibility.deadline); /* The late clock, not scan time. */
  ku_http_task_io_eligibility_observe(&eligibility,0,UINT64_MAX);
  CHECK(!eligibility.all_async); /* Read/write, closed, missing or cancelled. */
  ku_http_task_io_eligibility_observe(&eligibility,1,1100u);
  CHECK(!eligibility.all_async); /* Later valid members cannot undo urgency. */
  eligibility=(KuHttpTaskIoEligibility){UINT64_MAX,0,0,0};
  CHECK(!eligibility.all_async); /* An empty active set cannot defer I/O. */
  eligibility=(KuHttpTaskIoEligibility){UINT64_MAX,1,1,0};
  ku_http_task_io_eligibility_observe(&eligibility,1,2000u);
  CHECK(eligibility.all_async && eligibility.deadline==2000u);
  /* Rebuilding on the next traversal discards the old expiry/cancellation. */
  /* Frozen time cannot turn ready computation into an unbounded I/O skip.
   * Each caller turn still has its original pump(32) logical-poll limit. */
  unsigned int skipped=0,logical_polls=0;
  for(unsigned int turn=0;turn<1024u;turn++) {
    ku_http_task_io_batch_begin(&batch,1000u);
    logical_polls+=32u;
    if(!ku_http_task_io_batch_continue(&batch,1000u,1,0)) break;
    skipped++;
  }
  CHECK(skipped==7u && logical_polls==256u && !batch.turns && !batch.started_ms);
  /* A fresh clock boundary, including the exact millisecond, requires I/O.
   * A backwards sample must reset rather than restart/extend this burst. */
  ku_http_task_io_batch_begin(&batch,1000u);
  CHECK(!ku_http_task_io_batch_continue(&batch,1001u,1,0));
  CHECK(!batch.turns && !batch.started_ms);
  ku_http_task_io_batch_begin(&batch,2000u);
  CHECK(!ku_http_task_io_batch_continue(&batch,1999u,1,0));
  CHECK(!batch.turns && !batch.started_ms);
  ku_http_task_io_batch_begin(&batch,UINT64_MAX);
  CHECK(!ku_http_task_io_batch_continue(&batch,UINT64_MAX,1,0));
  ku_http_task_io_batch_begin(&batch,UINT64_MAX);
  CHECK(!ku_http_task_io_batch_continue(&batch,0u,1,0));
  /* No ready work (real WAIT), urgent I/O, cancellation/close/fault and a
   * blocking OS wait all use this same reset-before-poll branch. None may
   * carry the prior turn credit or timestamp into the next owner iteration. */
  for(int ready=0;ready<=1;ready++) for(int urgent=0;urgent<=1;urgent++) {
    if(ready && !urgent) continue;
    ku_http_task_io_batch_begin(&batch,3000u);
    CHECK(ku_http_task_io_batch_continue(&batch,3000u,1,0));
    ku_http_task_io_batch_begin(&batch,3000u);
    CHECK(!ku_http_task_io_batch_continue(&batch,3000u,ready,urgent));
    CHECK(!batch.turns && !batch.started_ms);
    ku_http_task_io_batch_begin(&batch,4000u);
    CHECK(batch.turns==1u && batch.started_ms==4000u);
    CHECK(ku_http_task_io_batch_continue(&batch,4000u,1,0));
    ku_http_task_io_batch_reset(&batch);
  }
  CHECK(!ku_http_task_io_batch_continue(&batch,0u,1,0));
  ku_http_task_io_batch_begin(&batch,0u);
  CHECK(ku_http_task_io_batch_continue(&batch,0u,1,0));
  CHECK(!ku_http_task_io_batch_continue(&batch,1u,1,0));
  CHECK(idle_probe_checks()==0);
  puts("http-task-io-batch-bounds-reset-ok"); return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-task-io-batch");
    let path = directory.path().join("batch.c");
    fs::write(&path, checks).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "batch")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires the native Task I/O batch regression"
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
        "Task I/O batch regression failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "http-task-io-batch-bounds-reset-ok\n"
    );
    assert!(output.stderr.is_empty());
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

#[cfg(unix)]
#[test]
fn native_http_wake_coalesces_without_losing_work_at_clear_or_concurrent_publication() {
    let runtime = include_str!("../src/backend/c_http_runtime.c").replace("\r\n", "\n");
    let start = runtime
        .find("typedef struct {\n  KuHttpSocket read_socket;")
        .unwrap();
    let end = runtime[start..]
        .find("/* Payload/request-map quota")
        .unwrap()
        + start;
    let wake = runtime[start..end].replace(
        "  (void)ku_http_wake_pending_exchange(wake,0);",
        "  test_at_clear(wake,0);\n  (void)ku_http_wake_pending_exchange(wake,0);\n  test_at_clear(wake,1);",
    ).replace("write(wake->write_socket, &byte, 1)", "test_write(wake->write_socket, &byte, 1)");
    let header = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <pthread.h>
typedef int KuHttpSocket;
typedef _Atomic size_t KuAtomicRefcount;
#define KU_HTTP_INVALID_SOCKET (-1)
static void counter_init(KuAtomicRefcount* p) { atomic_init(p,(size_t)0); }
static size_t counter_read(KuAtomicRefcount* p) { return atomic_load_explicit(p,memory_order_acquire); }
static void counter_add(KuAtomicRefcount* p) { (void)atomic_fetch_add_explicit(p,1,memory_order_release); }
static int ku_http_socket_last_error(void) { return errno; }
static int ku_http_socket_would_block(int e) { return e==EAGAIN || e==EWOULDBLOCK; }
static int ku_http_socket_error_interrupted(int e) { return e==EINTR; }
static void ku_http_socket_close(KuHttpSocket s) { if(s>=0) close(s); }
static int ku_http_socket_nonblocking(KuHttpSocket s) { int f=fcntl(s,F_GETFL,0); return f>=0 && !fcntl(s,F_SETFL,f|O_NONBLOCK); }
static int ready(KuHttpSocket s,int timeout) { struct pollfd f={s,POLLIN,0}; return poll(&f,1,timeout)>0 && (f.revents&POLLIN); }
static KuAtomicRefcount publications,send_calls;
static ssize_t test_write(KuHttpSocket s,const char* p,size_t n) { counter_add(&send_calls);return write(s,p,n); }
static void test_at_clear(void*,int);
"#;
    let checks = r#"
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"wake check line %d: %s\n",__LINE__,#c); exit(97); } } while(0)
static int clear_edge=-1;
static void publish(KuHttpWake* w) { counter_add(&publications);ku_http_wake_notify(w); }
static void test_at_clear(void* raw,int edge) {
 if(clear_edge==edge) { clear_edge=-1;publish((KuHttpWake*)raw); }
}
static void* producer(void* raw) {
 for(int i=0;i<1000;i++) publish((KuHttpWake*)raw);
 return NULL;
}
static pthread_mutex_t baton_lock=PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t baton_cond=PTHREAD_COND_INITIALIZER;
static size_t baton_permit,baton_ack;
static void* baton_producer(void* raw) {
 for(size_t i=1;i<=1000;i++) {
  CHECK(!pthread_mutex_lock(&baton_lock));
  while(baton_permit<i) CHECK(!pthread_cond_wait(&baton_cond,&baton_lock));
  CHECK(!pthread_mutex_unlock(&baton_lock));
  publish((KuHttpWake*)raw);
  CHECK(!pthread_mutex_lock(&baton_lock));
  while(baton_ack<i) CHECK(!pthread_cond_wait(&baton_cond,&baton_lock));
  CHECK(!pthread_mutex_unlock(&baton_lock));
 }
 return NULL;
}
int main(void) {
 counter_init(&publications);counter_init(&send_calls);
 KuHttpWake w;CHECK(ku_http_wake_init(&w));
 CHECK((fcntl(w.read_socket,F_GETFD)&FD_CLOEXEC) && (fcntl(w.write_socket,F_GETFD)&FD_CLOEXEC));
 for(int i=0;i<512;i++) publish(&w);
 CHECK(counter_read(&send_calls)==1 && ready(w.read_socket,0));
 ku_http_wake_drain(&w);CHECK(counter_read(&publications)==512 && !ready(w.read_socket,0));
 publish(&w);CHECK(ready(w.read_socket,0));size_t before=counter_read(&send_calls);
 clear_edge=0;ku_http_wake_drain(&w);
 CHECK(counter_read(&publications)==514 && counter_read(&send_calls)==before);
 publish(&w);CHECK(ready(w.read_socket,0));before=counter_read(&send_calls);
 clear_edge=1;ku_http_wake_drain(&w);
 CHECK(counter_read(&send_calls)==before+1);
 CHECK(counter_read(&publications)==516 && ready(w.read_socket,0));
 ku_http_wake_drain(&w);
 /* For a one-byte nonblocking pipe write, EAGAIN has an independently
    observable readable endpoint. No equivalent assertion is made for UDP. */
 char fill[512];memset(fill,1,sizeof(fill));size_t filled=0;
 for(;;) { ssize_t n=write(w.write_socket,fill,sizeof(fill));if(n>0) {filled+=(size_t)n;continue;} CHECK(n<0 && ku_http_socket_would_block(errno));break; }
 CHECK(filled>0 && ready(w.read_socket,0));
 while(write(w.write_socket,fill,1)==1) filled++;
 CHECK(ku_http_socket_would_block(errno));
 before=counter_read(&send_calls);publish(&w);
 CHECK(counter_read(&send_calls)==before+1 && errno==EAGAIN && ready(w.read_socket,0));
 do {ku_http_wake_drain(&w);} while(ready(w.read_socket,0));
 CHECK(counter_read(&publications)==517);
 pthread_t threads[4];for(int i=0;i<4;i++) CHECK(!pthread_create(&threads[i],NULL,producer,&w));
 size_t observed=517;
 while(observed<4517) {
  /* A timeout is a failure, never a reason to scan the publication counter. */
  CHECK(ready(w.read_socket,2000));ku_http_wake_drain(&w);
  observed=counter_read(&publications);
 }
 CHECK(observed==4517);
 for(int i=0;i<4;i++) CHECK(!pthread_join(threads[i],NULL));
 ku_http_wake_drain(&w);
 pthread_t baton;CHECK(!pthread_create(&baton,NULL,baton_producer,&w));
 for(size_t i=1;i<=1000;i++) {
  CHECK(!pthread_mutex_lock(&baton_lock));baton_permit=i;
  CHECK(!pthread_cond_broadcast(&baton_cond));CHECK(!pthread_mutex_unlock(&baton_lock));
  CHECK(ready(w.read_socket,2000));ku_http_wake_drain(&w);
  CHECK(counter_read(&publications)==4517+i);
  CHECK(!pthread_mutex_lock(&baton_lock));baton_ack=i;
  CHECK(!pthread_cond_broadcast(&baton_cond));CHECK(!pthread_mutex_unlock(&baton_lock));
 }
 CHECK(!pthread_join(baton,NULL));
 ku_http_wake_close(&w);
 CHECK(w.read_socket==KU_HTTP_INVALID_SOCKET && w.write_socket==KU_HTTP_INVALID_SOCKET);
 CHECK(!pthread_cond_destroy(&baton_cond));CHECK(!pthread_mutex_destroy(&baton_lock));
 puts("http-wake-coalescing-ok");return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-wake-coalescing");
    let path = directory.path().join("wake.c");
    fs::write(&path, format!("{header}{wake}{checks}")).unwrap();
    let executable = native_c_harness::compile_harness(directory.path(), &path, "wake")
        .expect("wake publication regression requires the native C compiler");
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "wake regression failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("http-wake-coalescing-ok"));
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
typedef struct { uint64_t task_slot,task_generation,function_id; uint32_t reason,driver_status,stage; } KuHttpFailure;
static KuHttpFailure ku_http_failure_current;
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
static size_t ku_http_task_response_admit(void* d,KuStruct___ku_http_response* r,KuHttpFailure* f) { (void)d; (void)r; (void)f; adopts++; return 17; }
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
fn native_http_async_idle_probe_preserves_reuse_partial_pipeline_and_half_close() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import "std.http"
async fn burn(): HttpResponse! {
    index=0
    total=0
    while(index<250000) {
        total=total+1
        index=index+1
    }
    return ok(http.text(str(total)))
}
async fn echo(req: HttpRequest): HttpResponse! {
    return ok(http.text(req.params.id + ":" + req.body))
}
fn main(): null! {
    app=http.server({max_active_requests:16,max_pending_requests:16,max_connections:32,
        idle_timeout_ms:5000,read_header_timeout_ms:1000,read_body_timeout_ms:1000,
        write_timeout_ms:2000,handler_timeout_ms:5000})
    app.get("/ok",fn() { return http.text("ok") })
    app.get("/burn",burn)
    app.post("/echo/{id}",echo)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let management_address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "async-idle-probe",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "1"),
            ("KU_BLOCKING_THREADS", "2"),
            ("KU_SERVICE_CONTROL_TOKEN", "idle-probe-regression"),
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
    // Use the existing absolute-deadline socket reader one byte at a time for
    // headers, then exactly the framed body: never consume the next pipeline
    // response while verifying the first one.
    let read_chunk = |stream: &mut TcpStream, buffer: &mut [u8], deadline: Instant| loop {
        match read_http_chunk_until(stream, buffer, deadline) {
            Ok(read) => break read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) && deadline.saturating_duration_since(Instant::now())
                    >= Duration::from_millis(1) => {}
            Err(error) => panic!("framed HTTP read before absolute deadline: {error}"),
        }
    };
    let read_frame = |stream: &mut TcpStream| {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            assert!(response.len() < 65536, "bounded response header");
            let mut byte = [0];
            assert_eq!(
                read_chunk(stream, &mut byte, deadline),
                1,
                "complete response header"
            );
            response.push(byte[0]);
        }
        let total = http_response_expected_total(&response, response.len() - 4)
            .unwrap()
            .expect("framed HTTP response");
        assert!(total <= MAX_HTTP_TEST_RESPONSE_BYTES);
        let mut offset = response.len();
        response.resize(total, 0);
        while offset < total {
            let read = read_chunk(stream, &mut response[offset..], deadline);
            assert!(read > 0, "complete response body");
            offset += read;
        }
        String::from_utf8(response).unwrap()
    };
    let metrics_request = "GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: idle-probe-regression\r\nConnection: close\r\n\r\n";
    assert_status(
        &http_response(&management_address, metrics_request, Duration::from_secs(3)),
        "HTTP/1.1 200 OK",
    );
    let mut idle = Vec::new();
    for _ in 0..4 {
        let mut stream = connect_with_retry(&address, Duration::from_secs(3));
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let response = read_frame(&mut stream);
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(response.ends_with("ok"));
        assert!(response
            .to_ascii_lowercase()
            .contains("connection: keep-alive\r\n"));
        idle.push(stream);
    }
    // Real ready computation coexists with already-reused idle sockets. The
    // helper regression proves the zero-poll/count/time bounds; this socket
    // test checks framing and lifecycle without a microsecond timing claim.
    let mut cpu = connect_with_retry(&address, Duration::from_secs(3));
    cpu.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    cpu.write_all(b"GET /burn HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let admitted_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let metrics = http_response(&management_address, metrics_request, Duration::from_secs(3));
        let tasks = metrics
            .split_once("\"tasks\":[")
            .unwrap()
            .1
            .split_once(']')
            .unwrap()
            .0;
        let polls = tasks
            .split_once("\"polls\":")
            .unwrap()
            .1
            .split(|character: char| !character.is_ascii_digit())
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        if polls > 0 {
            break;
        }
        assert!(
            Instant::now() < admitted_deadline,
            "CPU Task never ran: {metrics}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    idle[0]
        .write_all(b"POST /echo/partial HTTP/1.1\r\nHost:")
        .unwrap();
    idle[1]
        .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = read_frame(&mut idle[1]);
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("ok"));
    idle[0]
        .write_all(b" localhost\r\nContent-Length: 3\r\n\r\nx")
        .unwrap();
    idle[2]
        .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = read_frame(&mut idle[2]);
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("ok"));
    idle[0]
        .write_all(b"yzGET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    idle[0].shutdown(std::net::Shutdown::Write).unwrap();
    for body in ["partial:xyz", "ok"] {
        let response = read_frame(&mut idle[0]);
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(response.ends_with(body), "{response}");
    }
    let mut byte = [0];
    assert_eq!(
        read_chunk(
            &mut idle[0],
            &mut byte,
            Instant::now() + Duration::from_secs(3)
        ),
        0,
        "half-close preserves both responses before the connection closes"
    );
    let response = read_frame(&mut cpu);
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("250000"), "{response}");
    // A reused connection with an incomplete body retains its existing finite
    // deadline. Batch eligibility must not turn it back into an idle socket.
    idle[3]
        .write_all(b"POST /echo/timeout HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nx")
        .unwrap();
    assert_status(&read_frame(&mut idle[3]), "HTTP/1.1 408 Request Timeout");
    drop(idle);
    drop(cpu);
    let recovered = http_response(
        &address,
        "GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&recovered, "HTTP/1.1 200 OK");
    assert!(recovered.ends_with("ok"));
    let reclaimed_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let metrics = http_response(&management_address, metrics_request, Duration::from_secs(3));
        let section = |name: &str, end: char| {
            metrics
                .split_once(name)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0
        };
        if section("\"tasks\":[", ']').contains("\"resident\":0")
            && section("\"tasks\":[", ']').contains("\"timers\":0")
            && section("\"task_driver\":{", '}').contains("\"resident\":0")
            && section("\"loops\":[", ']').contains("\"connections\":0")
            && section("\"loops\":[", ']').contains("\"pending\":0")
        {
            break;
        }
        assert!(
            Instant::now() < reclaimed_deadline,
            "resources not reclaimed: {metrics}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let response = http_response(
        &management_address,
        "POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: idle-probe-regression\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
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
    app=http.server({{max_active_requests:8320,max_pending_requests:64,max_connections:16640,
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
            ("KU_TASK_MAX_BYTES", "67108864"),
            // Capacity includes 8192 held requests plus a live probe margin.
            // This smoke checks allocation/cleanup and stack bounds; the full
            // 8192 simultaneous network waiting acceptance runs separately.
            ("KU_THREAD_STACK_BYTES", "262144"),
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

#[test]
fn native_http_rejection_samples_cannot_exhaust_internal_failure_evidence() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import http from "std.http"
async fn refusal(): HttpResponse! { return ok(http.text(503,"busy")) }
async fn broken(): HttpResponse! { fail "deliberate diagnostic regression" }
fn main(): null! {
    app=http.server({max_active_requests:16,max_pending_requests:16,max_connections:64})
    app.get("/refuse",refusal)
    app.get("/broken",broken)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "sample-pressure",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "2"),
            ("KU_BLOCKING_THREADS", "2"),
            ("KU_SERVICE_CONTROL_TOKEN", "sample-pressure"),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    for _ in 0..12 {
        let response = http_response(
            &address,
            "GET /refuse HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            Duration::from_secs(3),
        );
        assert_status(&response, "HTTP/1.1 503 Service Unavailable");
    }
    let response=http_response(&address,"GET /broken HTTP/1.1\r\nHost: localhost\r\nX-Request-Id: actual-500\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 500 Internal Server Error");
    let metrics=http_response(&address,"GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: sample-pressure\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&metrics, "HTTP/1.1 200 OK");
    assert!(metrics.contains("\"handler_result\":1"), "{metrics}");
    assert!(metrics.contains("\"http_500_total\":1"), "{metrics}");
    assert!(metrics.contains("\"unknown_500\":0"), "{metrics}");
    assert!(
        metrics.contains("\"sample_policy\":\"500_and_peer_reset_500_priority\""),
        "{metrics}"
    );
    assert_eq!(
        metrics.matches("\"reason\":\"handler_result\"").count(),
        1,
        "{metrics}"
    );
    assert!(
        metrics.contains("61637475616c2d353030"),
        "500 request identity lost: {metrics}"
    );
    let response=http_response(&address,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: sample-pressure\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_failures_have_bounded_private_reason_witnesses() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import { http } from "std"
async fn broken(): HttpResponse! {
    large=9223372036854775807
    overflow=large+1
    if(overflow==0) { return ok(http.text("unreachable")) }
    return ok(http.text("unreachable"))
}
async fn healthy(): HttpResponse! { return ok(http.text("ready")) }
fn main(): null! {
    app=http.server({max_active_requests:16,max_pending_requests:16,max_connections:64})
    app.get("/broken",broken)
    app.get("/healthy",healthy)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "failure-witness",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "2"),
            ("KU_BLOCKING_THREADS", "2"),
            ("KU_SERVICE_CONTROL_TOKEN", "failure-witness"),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    for i in 0..12 {
        let response=http_response(&address,&format!("GET /broken HTTP/1.1\r\nHost: localhost\r\nX-Request-Id: failure-{i}\r\nConnection: close\r\n\r\n"),Duration::from_secs(3));
        assert_status(&response, "HTTP/1.1 500 Internal Server Error");
        assert!(response.ends_with("Internal Server Error"), "{response}");
        assert!(
            !response.contains("handler_result"),
            "internal reason leaked: {response}"
        );
    }
    let denied = http_response(
        &address,
        "GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&denied, "HTTP/1.1 403 Forbidden");
    let metrics=http_response(&address,"GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: failure-witness\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&metrics, "HTTP/1.1 200 OK");
    assert!(metrics.contains("\"handler_result\":12"), "{metrics}");
    assert!(metrics.contains("\"unknown\":0"), "{metrics}");
    assert!(metrics.contains("\"dropped_samples\":4"), "{metrics}");
    assert_eq!(
        metrics.matches("\"reason\":\"handler_result\"").count(),
        8,
        "{metrics}"
    );
    assert!(
        metrics.contains("6661696c7572652d30"),
        "request identity lost: {metrics}"
    );
    assert!(
        !metrics.contains("\"task_generation\":0"),
        "Task generation lost: {metrics}"
    );
    let healthy = http_response(
        &address,
        "GET /healthy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&healthy, "HTTP/1.1 200 OK");
    let response=http_response(&address,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: failure-witness\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[cfg(any(unix, windows))]
#[test]
fn native_http_reset_retires_async_work_but_write_half_close_keeps_response() {
    use std::io::Read;
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import {http,time} from "std"
async fn slow(): HttpResponse! { time.sleep(5000)? return ok(http.text("late")) }
async fn pipeline_slow(): HttpResponse! { time.sleep(1000)? return ok(http.text("pipeline-late")) }
fn job_slow(): HttpResponse {
    started=time.steady_millis()
    while(time.steady_millis()-started<200) {}
    return http.text("job-late")
}
async fn brief(): HttpResponse! { time.sleep(20)? return ok(http.text("half-close-ok")) }
fn main(): null! {
    app=http.server({max_active_requests:16,max_pending_requests:16,max_connections:64,handler_timeout_ms:10000})
    app.get("/slow",slow)
    app.get("/pipeline-slow",pipeline_slow)
    app.post("/job-slow",job_slow)
    app.get("/short",brief)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let management_address = unused_local_address();
    assert_ne!(address, management_address);
    let witness_directory = native_c_harness::TempDir::new("http-peer-pause-witness");
    let witness_path = witness_directory.path().join("paused-byte");
    let witness_path_text = witness_path.to_str().expect("UTF-8 temporary witness path");
    let Some(mut server) = spawn_native_server_settings(
        "reset-vs-half-close",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "1"),
            ("KU_BLOCKING_THREADS", "2"),
            ("KU_SERVICE_CONTROL_TOKEN", "reset-witness"),
            ("KU_SERVICE_PROBE_ADDR", &management_address),
            ("KU_NATIVE_HTTP_TEST_PEER_PAUSE_FILE", witness_path_text),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let metrics_request="GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: reset-witness\r\nConnection: close\r\n\r\n";
    // Wait for real startup using the existing bounded socket reader.
    assert_status(
        &http_response(&management_address, metrics_request, Duration::from_secs(3)),
        "HTTP/1.1 200 OK",
    );
    let mut peer = TcpStream::connect(&address).unwrap();
    peer.write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let metrics = http_response(&management_address, metrics_request, Duration::from_secs(3));
        let tasks = metrics
            .split("\"tasks\":[")
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap();
        if tasks.contains("\"timers\":1") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "request never reached timer: {metrics}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let reset_peer = reset_http_peer;
    reset_peer(peer); // A confirmed RST, not an ambiguous FIN or caller-side timer.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let metrics = http_response(&management_address, metrics_request, Duration::from_secs(3));
        let tasks = metrics
            .split("\"tasks\":[")
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap();
        if tasks.contains("\"resident\":0") && tasks.contains("\"timers\":0") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reset request kept its Task alive: {metrics}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // A pending pipelined byte is legal future input. First witness that the
    // owner actually paused read interest on that byte, then send a real RST.
    // The byte stays unread: consuming it would hide the transport error case.
    let section = |metrics: &str, name: &str, end: char| {
        metrics
            .split_once(name)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0
            .to_owned()
    };
    let counter = |value: &str, name: &str| -> u64 {
        let prefix = format!("\"{name}\":");
        value
            .split_once(&prefix)
            .unwrap()
            .1
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let baseline = http_response(&management_address, metrics_request, Duration::from_secs(3));
    assert_eq!(
        counter(&section(&baseline, "\"tasks\":[", ']'), "resident"),
        0,
        "idle Task baseline: {baseline}"
    );
    assert_eq!(
        counter(&section(&baseline, "\"task_driver\":{", '}'), "resident"),
        0,
        "idle driver baseline: {baseline}"
    );
    assert_eq!(
        counter(&section(&baseline, "\"loops\":[", ']'), "connections"),
        0,
        "idle connection baseline: {baseline}"
    );
    let baseline_reserved = counter(
        &section(&baseline, "\"task_driver\":{", '}'),
        "reserved_bytes",
    );
    let baseline_resets = counter(&section(&baseline, "\"counts\":{", '}'), "peer_reset");
    let mut peer = TcpStream::connect(&address).unwrap();
    peer.write_all(
        b"GET /pipeline-slow HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let metrics = http_response(
            &management_address,
            metrics_request,
            Duration::from_millis(100),
        );
        let tasks = section(&metrics, "\"tasks\":[", ']');
        assert!(
            Instant::now() < deadline,
            "pipeline request never reached timer: {metrics}"
        );
        if counter(&tasks, "resident") == 1 && counter(&tasks, "timers") == 1 {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    peer.write_all(b"G").unwrap();
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        if fs::read(&witness_path).ok().as_deref() == Some(b"G") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "owner did not witness the pending pipeline byte"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let before_reset = http_response(
        &management_address,
        metrics_request,
        Duration::from_millis(100),
    );
    assert_eq!(
        counter(&section(&before_reset, "\"tasks\":[", ']'), "timers"),
        1,
        "timer must still be pending when RST is sent: {before_reset}"
    );
    let reset_at = reset_peer(peer);
    let deadline = reset_at + Duration::from_millis(500);
    let mut last_metrics = before_reset;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining >= Duration::from_millis(5),
            "paused pipeline RST was not physically reclaimed within 500ms (less than5ms left, no tiny-deadline metrics request); expected reserved={baseline_reserved}, peer_reset={}; last metrics: {last_metrics}",
            baseline_resets + 1
        );
        let metrics = http_response(
            &management_address,
            metrics_request,
            remaining.min(Duration::from_millis(100)),
        );
        last_metrics = metrics.clone();
        let tasks = section(&metrics, "\"tasks\":[", ']');
        let driver = section(&metrics, "\"task_driver\":{", '}');
        let loops = section(&metrics, "\"loops\":[", ']');
        // Check the observation time as well: the 1s natural timer completion
        // must never masquerade as prompt cancellation or a transport receipt.
        assert!(
            Instant::now() < deadline,
            "paused pipeline RST exceeded its reclamation bound: {metrics}"
        );
        if counter(&section(&metrics, "\"counts\":{", '}'), "peer_reset") == baseline_resets + 1
            && counter(&tasks, "resident") == 0
            && counter(&tasks, "timers") == 0
            && counter(&driver, "resident") == 0
            && counter(&driver, "reserved_bytes") == baseline_reserved
            && counter(&loops, "connections") == 0
            && counter(&loops, "pending") == 0
        {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    // The same paused-read branch also owns synchronous JOB sockets. A
    // bounded 200ms handler keeps a real worker reference alive after RST:
    // transport detection must be prompt, physical retirement must wait.
    fs::remove_file(&witness_path).unwrap();
    let job_baseline = http_response(
        &management_address,
        metrics_request,
        Duration::from_millis(100),
    );
    let job_resets = counter(&section(&job_baseline, "\"counts\":{", '}'), "peer_reset");
    let job_buffer_baseline = counter(&job_baseline, "buffer_bytes");
    let body = "x".repeat(4096);
    let mut peer = TcpStream::connect(&address).unwrap();
    peer.write_all(format!("POST /job-slow HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut job_last_metrics = loop {
        let metrics = http_response(
            &management_address,
            metrics_request,
            Duration::from_millis(50),
        );
        let loops = section(&metrics, "\"loops\":[", ']');
        assert!(
            Instant::now() < deadline,
            "synchronous worker did not start: {metrics}"
        );
        if counter(&loops, "running") == 1 && counter(&loops, "resident") == 1 {
            assert!(
                counter(&metrics, "buffer_bytes") > job_buffer_baseline,
                "live JOB must retain its actual input charge: {metrics}"
            );
            break metrics;
        }
        thread::sleep(Duration::from_millis(1));
    };
    peer.write_all(b"G").unwrap();
    let deadline = Instant::now() + Duration::from_millis(50);
    loop {
        if fs::read(&witness_path).ok().as_deref() == Some(b"G") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "JOB did not pause on its unconsumed pipeline byte"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let reset_at = reset_peer(peer);
    let deadline = reset_at + Duration::from_millis(100);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining >= Duration::from_millis(5),
            "paused JOB RST was not observed within 100ms (no tiny-deadline request); last metrics: {job_last_metrics}"
        );
        let metrics = http_response(
            &management_address,
            metrics_request,
            remaining.min(Duration::from_millis(25)),
        );
        assert!(
            Instant::now() < deadline,
            "paused JOB reset receipt arrived after 100ms: {metrics}"
        );
        job_last_metrics = metrics.clone();
        let loops = section(&metrics, "\"loops\":[", ']');
        if counter(&section(&metrics, "\"counts\":{", '}'), "peer_reset") == job_resets + 1
            && counter(&loops, "connections") == 0
        {
            assert_eq!(
                counter(&loops, "running"),
                1,
                "fixture must witness the still-running worker: {metrics}"
            );
            assert_eq!(
                counter(&loops, "resident"),
                1,
                "live worker reference returned early: {metrics}"
            );
            assert_eq!(
                counter(&loops, "active_requests"),
                1,
                "closed fd must retain A until the real worker retires: {metrics}"
            );
            assert!(
                counter(&metrics, "buffer_bytes") > job_buffer_baseline,
                "live worker input charge returned early: {metrics}"
            );
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let deadline = reset_at + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining >= Duration::from_millis(5),
            "JOB failed to retire after its bounded work finished (no tiny-deadline request); last metrics: {job_last_metrics}"
        );
        let metrics = http_response(
            &management_address,
            metrics_request,
            remaining.min(Duration::from_millis(50)),
        );
        job_last_metrics = metrics.clone();
        let loops = section(&metrics, "\"loops\":[", ']');
        assert!(
            Instant::now() < deadline,
            "JOB physical retirement exceeded 500ms: {metrics}"
        );
        if counter(&loops, "running") == 0
            && counter(&loops, "resident") == 0
            && counter(&loops, "queued") == 0
            && counter(&loops, "connections") == 0
            && counter(&loops, "active_requests") == 0
            && counter(&metrics, "buffer_bytes") == job_buffer_baseline
            && counter(
                &section(&metrics, "\"task_driver\":{", '}'),
                "reserved_bytes",
            ) == baseline_reserved
        {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let mut peer = TcpStream::connect(&address).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    peer.write_all(b"GET /short HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    peer.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = String::new();
    peer.read_to_string(&mut response).unwrap();
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(response.ends_with("half-close-ok"), "{response}");
    let response=http_response(&management_address,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: reset-witness\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
    // One real closed JOB consumes the only physical record and A lease even
    // though its fd has gone away. A new empty accepted transport therefore
    // enters RAW P, then moves into C only after the actual worker is released.
    // This is the whole OS path missing from the field-only FIFO regression.
    let address = unused_local_address();
    let management_address = unused_local_address();
    let gate_directory = native_c_harness::TempDir::new("http-raw-retained-job");
    let gate = gate_directory.path().join("worker");
    let gate_text = gate.to_str().expect("UTF-8 temporary gate path");
    let source = r#"
import http from "std.http"
fn held(): HttpResponse {return http.text("real-handler-complete")}
fn main(): null! {
    app=http.server({max_connections:1,max_active_requests:1,max_pending_requests:2,
        idle_timeout_ms:5000,handler_timeout_ms:10000})
    app.get("/held",held)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let Some(mut server) = spawn_native_server_settings(
        "raw-after-retained-job",
        source,
        &address,
        false,
        None,
        &[
            ("KU_EVENT_LOOPS", "1"),
            ("KU_BLOCKING_THREADS", "2"),
            ("KU_SERVICE_CONTROL_TOKEN", "raw-retirement"),
            ("KU_SERVICE_PROBE_ADDR", &management_address),
            ("KU_HTTP_REQUEST_TIMEOUT_MS", "2000"),
            ("KU_NATIVE_HTTP_TEST_JOB_GATE", gate_text),
        ],
    ) else {
        panic!("native compiler was already available");
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let metrics_request="GET /__ku/metrics HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: raw-retirement\r\nConnection: close\r\n\r\n";
    // Match the first server's startup barrier before applying the short
    // state/retirement observation budgets. Process spawn is not socket readiness.
    eprintln!("retained JOB/RAW regression: waiting for second server management readiness");
    assert_status(
        &http_response(&management_address, metrics_request, Duration::from_secs(3)),
        "HTTP/1.1 200 OK",
    );
    eprintln!("retained JOB/RAW regression: management ready; checking initial snapshot");
    let snapshot = || {
        http_response(
            &management_address,
            metrics_request,
            Duration::from_millis(500),
        )
    };
    let witness = |suffix: &str| {
        let path = gate.with_extension(suffix);
        let deadline = Instant::now() + Duration::from_millis(700);
        loop {
            if let Ok(text) = fs::read_to_string(&path) {
                if text.ends_with('\n') {
                    return text;
                }
            }
            assert!(
                Instant::now() < deadline,
                "missing bounded worker/raw witness {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(1));
        }
    };
    let wait_state = |connections: u64, pending: u64, active: u64, resident: u64| {
        eprintln!(
            "retained JOB/RAW regression: waiting for connections={connections}, pending={pending}, active={active}, resident={resident}"
        );
        let deadline = Instant::now() + Duration::from_millis(700);
        loop {
            let metrics = snapshot();
            let loops = section(&metrics, "\"loops\":[", ']');
            if counter(&loops, "connections") == connections
                && counter(&loops, "pending") == pending
                && counter(&loops, "active_requests") == active
                && counter(&loops, "resident") == resident
            {
                return metrics;
            }
            assert!(
                Instant::now() < deadline,
                "retained JOB/RAW-P state missing: {metrics}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    };
    assert_status(&snapshot(), "HTTP/1.1 200 OK");
    let mut held = TcpStream::connect(&address).unwrap();
    held.write_all(b"GET /held HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert_eq!(witness("started"), "worker-retains-real-job\n");
    wait_state(1, 0, 1, 1);
    reset_http_peer(held);
    let held_metrics = wait_state(0, 0, 1, 1);
    assert_eq!(
        counter(&section(&held_metrics, "\"loops\":[", ']'), "running"),
        1,
        "RST must not release the gated physical worker: {held_metrics}"
    );
    let raw = TcpStream::connect(&address).unwrap(); // Intentionally send NO bytes.
    wait_state(1, 1, 1, 1);
    let accepted: Vec<u64> = witness("raw")
        .split_whitespace()
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(accepted.len(), 2);
    let original_deadline = accepted[0];
    assert!(original_deadline > accepted[1] && original_deadline <= accepted[1] + 2000);
    // Opening the gate, rather than a chosen sleep duration, makes the worker
    // reference and A/C record physically reusable in the actual owner path.
    fs::write(gate.with_extension("release"), b"release\n").unwrap();
    let transfer: Vec<u64> = witness("transfer")
        .split_whitespace()
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(transfer.len(), 6);
    assert_eq!(
        transfer[0], original_deadline,
        "RAW transfer renewed request D"
    );
    assert_eq!(
        transfer[1], original_deadline,
        "empty RAW transfer must clamp its idle D before heap insertion"
    );
    assert!(
        transfer[2] < original_deadline,
        "raw must still be alive at transfer"
    );
    assert_eq!(
        &transfer[3..],
        &[0, 0, 1],
        "actual RAW peek must be EAGAIN, without acquiring A or starting the header"
    );
    let transferred = wait_state(1, 0, 0, 0);
    assert_eq!(
        counter(
            &section(&transferred, "\"loops\":[", ']'),
            "peak_active_requests"
        ),
        1
    );
    let response = read_http_stream(raw, Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 408 Request Timeout");
    let expired: Vec<u64> = witness("timeout")
        .split_whitespace()
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(expired.len(), 3);
    assert_eq!(expired[0], original_deadline);
    assert_eq!(expired[1], original_deadline);
    assert!(
        expired[2] >= original_deadline && expired[2] - original_deadline < 1000,
        "408 must use original request D, not the new 5s idle interval: {expired:?}"
    );
    wait_state(0, 0, 0, 0);
    let drain=http_response(&management_address,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: raw-retirement\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",Duration::from_secs(3));
    assert_status(&drain, "HTTP/1.1 200 OK");
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_json_dynamic_values_have_json_mime_and_exact_body() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"import { http, json } from "std"
fn encode(req: HttpRequest): HttpResponse! {
    value = json.decode(req.body)?
    return ok(http.json(201, value))
}
fn quoted(): HttpResponse { return http.json("quoted\n") }
fn bad_status(): HttpResponse { return http.json(99, "invalid") }
fn bad_writer(): HttpResponse {
    try { return http.json(ok("unsupported")) }
    catch (err) { return http.text("unexpected-catch") }
    return http.text("unreachable")
}
fn main(): null! {
    app = http.server()
    app.post("/json", encode)
    app.get("/quoted", quoted)
    app.get("/bad-status", bad_status)
    app.get("/bad-writer", bad_writer)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let Some(mut server) = spawn_finite_native_server("json-mime", source, &address, 4) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    let body = r#"{"label":"中文🙂\u0000","items":[1,true,null]}"#;
    let response = http_response(&address, &format!(
        "POST /json HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body), Duration::from_secs(3));
    assert_status(&response, "HTTP/1.1 201 Created");
    let (headers, encoded) = response
        .split_once("\r\n\r\n")
        .expect("HTTP JSON header/body boundary");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("content-type: application/json; charset=utf-8"),
        "{headers}"
    );
    assert_eq!(encoded, r#"{"items":[1,true,null],"label":"中文🙂\u0000"}"#);
    for path in ["/bad-status", "/bad-writer"] {
        let response = http_response(
            &address,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            Duration::from_secs(3),
        );
        assert_status(&response, "HTTP/1.1 500 Internal Server Error");
        assert!(!response.contains("unexpected-catch"), "{response}");
    }
    let response = http_response(
        &address,
        "GET /quoted HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    );
    assert_status(&response, "HTTP/1.1 200 OK");
    assert!(
        response.ends_with("\"quoted\\n\""),
        "string must be JSON encoded exactly once: {response}"
    );
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_direct_cpu_formatting_uses_fixed64_frame_and_exact_eight_digits() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let original = include_str!("benchmarks/cross_stack/ku/server.ku");
    let marker = "    return (await work_response(req, total))";
    assert_eq!(original.matches(marker).count(), 1);
    let expanded = original.replace("__PEER_PORT__", "19191").replace(
        marker,
        r#"    text = str(total)
    while (text.len() < 8) { text = "0" + text }
    return ok(http.text(req.query.rid + ":" + text))"#,
    );
    // Keep the actual whole work function and all other function declarations.
    // Replace only unrelated DB/setup main with the existing finite HTTP fixture.
    let handlers = expanded.split_once("fn main(): null! {").unwrap().0;
    let source = format!(
        r#"{handlers}
// An unused handler must not create a factory reference to an absent thunk.
async fn unregistered(req: HttpRequest): HttpResponse! {{
    return ok(http.text("unused"))
}}
fn main(): null! {{
    app=http.server()
    app.get("/ok",ready)
    app.post("/json",encode)
    app.get("/work",work)
    app.get("/timer",timer)
    app.get("/wait",waiting)
    app.get("/tcp",network)
    app.listen("__ADDRESS__")?
    return ok(null)
}}
"#
    );
    let address = unused_local_address();
    let Some(mut server) = spawn_finite_native_server("direct-cpu64", &source, &address, 2) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    for (work, seed, expected) in [(512usize, 3usize, 22944003i64), (2, 5, 7129005)] {
        let response = http_response(&address, &format!(
            "GET /work?rid=proof&work={}&seed={} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", "x".repeat(work), "s".repeat(seed)), Duration::from_secs(5));
        assert_status(&response, "HTTP/1.1 200 OK");
        assert!(
            response.ends_with(&format!("proof:{expected:08}")),
            "{response}"
        );
    }
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}

#[test]
fn native_http_persistent_owner_rejects_recycled_events_and_indexes_idle_deadlines() {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return; // Windows intentionally retains its existing poll backend.
    }
    let runtime = include_str!("../src/backend/c_http_runtime.c");
    let begin = runtime.find("/* Persistent owner reactor.").unwrap();
    let end = runtime[begin..]
        .find("/* End persistent owner reactor. */")
        .unwrap()
        + begin;
    // Compile actual production registry/heap/kernel helpers with a model of
    // only their borrowed connection fields. Real socketpair readiness and fd
    // recycling exercise the OS backend; no alternate event algorithm exists.
    let header = r#"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <time.h>
#include <sys/socket.h>
#if defined(__linux__)
#include <sys/epoll.h>
#else
#include <sys/event.h>
#endif
#define KU_HTTP_PERSISTENT_EVENT_LOOP 1
#define KU_HTTP_INVALID_SOCKET (-1)
#define KU_HTTP_POLL_READ POLLIN
#define KU_HTTP_POLL_WRITE POLLOUT
typedef int KuHttpSocket;
typedef struct pollfd KuHttpPollFd;
enum { KU_HTTP_IO_HEADER=1,KU_HTTP_IO_BODY=2,KU_HTTP_IO_JOB=3,KU_HTTP_IO_WRITE=4,KU_HTTP_IO_ASYNC=8,KU_HTTP_IO_REQUEST_WAIT=10 };
struct KuHttpConnection;
typedef struct KuHttpActiveSet {
  struct KuHttpConnection* connections;
  int capacity;
  struct KuHttpReactor* reactor;
} KuHttpActiveSet;
typedef struct KuHttpConnection {
  KuHttpActiveSet* active_set;
  struct KuHttpReactor* reactor;
  int work_position,idle_position,active_position;
  int phase,reused,peer_read_paused,socket_value,request_lease,request_queued;
  size_t buffered,carry_length;
  void* job;void* async_task;
  uint64_t request_deadline,deadline,connection_generation,request_wait_probe_deadline;
} KuHttpConnection;
"#;
    let checks = r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"persistent owner line %d: %s\n",__LINE__,#c);return 1;}}while(0)
static int fixture_external_net_close(int reuse,int both_filters) {
  KuHttpConnection connections[1]={{0}};
  KuHttpActiveSet active={connections,1,NULL};KuHttpReactor reactor;
  CHECK(ku_http_reactor_init(&reactor,&active,1u));
  int retired[2],replacement[2];
  CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,retired));
  CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,replacement));
  int fd=retired[0];size_t old_net=4u;
  ku_http_reactor_interest(&reactor,old_net,fd,(short)(KU_HTTP_POLL_READ|(both_filters?KU_HTTP_POLL_WRITE:0)),UINT64_MAX-300u,-1);
  CHECK(!reactor.fault && reactor.registrations[old_net].registered);
  /* The net owner closes during pump/interests before HTTP reconciles the
   * previous compact snapshot. No HTTP close hook runs on that resource. */
  CHECK(!close(fd));
  if(!reuse) {
    ku_http_reactor_delete(&reactor,old_net);
    CHECK(!reactor.fault && !reactor.registrations[old_net].registered);
  } else {
    CHECK(dup2(replacement[0],fd)==fd);CHECK(!close(replacement[0]));
    ku_http_reactor_interest(&reactor,0u,fd,KU_HTTP_POLL_READ,UINT64_MAX-200u,-1);
    CHECK(!reactor.fault && reactor.registrations[0].registered);
    uint64_t current_token=reactor.registrations[0].token;
    /* A new HTTP slot (or new net resource) can register the reused numeric
     * fd before the retired net entry is reconciled. DEL is by numeric fd:
     * the old token must never delete this new kernel registration. */
    ku_http_reactor_delete(&reactor,old_net);
    CHECK(!reactor.fault);
    CHECK(write(replacement[1],"r",1)==1);
    KuHttpPollFd items[8]={{0}};
    CHECK(ku_http_reactor_wait(&reactor,items,1000)>0);
    int found=0;
    for(size_t i=0;i<reactor.ready_count;i++) if(reactor.ready[i].token==current_token) {
      CHECK(reactor.ready[i].generation==UINT64_MAX-200u);
      CHECK(ku_http_reactor_event_valid(&reactor,&reactor.ready[i]));found=1;
    }
    CHECK(found);
    char byte;CHECK(read(fd,&byte,1)==1 && byte=='r');
    /* No retired WRITE filter/token may remain ready after READ was drained. */
    CHECK(ku_http_reactor_wait(&reactor,items,0)==0);
    ku_http_reactor_delete(&reactor,0u);CHECK(!close(fd));
  }
  CHECK(!close(retired[1]));
  if(!reuse) CHECK(!close(replacement[0]));
  CHECK(!close(replacement[1]));ku_http_reactor_destroy(&reactor);
  puts(reuse?"http-external-net-reuse-preserves-current-registration-ok":"http-external-net-close-retirement-ok");
  return 0;
}
static int fixture_filter_masks(void) {
  KuHttpConnection connections[1]={{0}};
  KuHttpActiveSet active={connections,1,NULL};KuHttpReactor reactor;
  CHECK(ku_http_reactor_init(&reactor,&active,1u));
  int pair[2];CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,pair));
  KuHttpPollFd items[8]={{0}};size_t net=4u;
  ku_http_reactor_interest(&reactor,net,pair[0],KU_HTTP_POLL_READ|KU_HTTP_POLL_WRITE,123u,0);
  uint64_t token=reactor.registrations[net].token;
  CHECK(!reactor.fault && write(pair[1],"m",1)==1);
  CHECK(ku_http_reactor_wait(&reactor,items,1000)>0);
  CHECK((items[0].revents&(KU_HTTP_POLL_READ|KU_HTTP_POLL_WRITE))==(KU_HTTP_POLL_READ|KU_HTTP_POLL_WRITE));
  char byte;CHECK(read(pair[0],&byte,1)==1 && byte=='m');
  items[0].revents=0;
  ku_http_reactor_interest(&reactor,net,pair[0],KU_HTTP_POLL_READ,123u,0);
  CHECK(!reactor.fault && reactor.registrations[net].token==token);
  CHECK(ku_http_reactor_wait(&reactor,items,0)==0 && !items[0].revents);
  ku_http_reactor_interest(&reactor,net,pair[0],KU_HTTP_POLL_WRITE,123u,0);
  CHECK(!reactor.fault && ku_http_reactor_wait(&reactor,items,1000)>0);
  CHECK(items[0].revents&KU_HTTP_POLL_WRITE);
  items[0].revents=0;
  ku_http_reactor_interest(&reactor,net,pair[0],0,123u,0);
  CHECK(!reactor.fault && ku_http_reactor_wait(&reactor,items,0)==0 && !items[0].revents);
  ku_http_reactor_interest(&reactor,net,pair[0],KU_HTTP_POLL_READ,123u,0);
  CHECK(write(pair[1],"n",1)==1 && ku_http_reactor_wait(&reactor,items,1000)>0);
  CHECK(items[0].revents&KU_HTTP_POLL_READ);
  CHECK(read(pair[0],&byte,1)==1 && byte=='n');
  ku_http_reactor_delete(&reactor,net);CHECK(!reactor.fault);
  CHECK(!close(pair[0]) && !close(pair[1]));ku_http_reactor_destroy(&reactor);
  puts("http-persistent-dual-filter-mask-transitions-ok");return 0;
}
static int fixture_closed_event_source(void) {
  KuHttpConnection connections[1]={{0}};
  KuHttpActiveSet active={connections,1,NULL};KuHttpReactor reactor;
  CHECK(ku_http_reactor_init(&reactor,&active,1u));
  int pair[2];CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,pair));
  ku_http_reactor_interest(&reactor,4u,pair[0],KU_HTTP_POLL_READ,99u,-1);
  CHECK(!reactor.fault);
  CHECK(!close(reactor.descriptor) && !close(pair[0]));
  ku_http_reactor_delete(&reactor,4u);
  CHECK(reactor.fault); /* EBADF must not hide failure of the event source. */
  reactor.descriptor=-1;CHECK(!close(pair[1]));ku_http_reactor_destroy(&reactor);
  puts("http-persistent-closed-event-source-fault-ok");return 0;
}
int main(int argc,char** argv) {
  if(argc>1 && !strcmp(argv[1],"external-net-close")) return fixture_external_net_close(0,0);
  if(argc>1 && !strcmp(argv[1],"external-net-reuse")) return fixture_external_net_close(1,0);
  if(argc>1 && !strcmp(argv[1],"external-net-reuse-both")) return fixture_external_net_close(1,1);
  if(argc>1 && !strcmp(argv[1],"filter-masks")) return fixture_filter_masks();
  if(argc>1 && !strcmp(argv[1],"closed-event-source")) return fixture_closed_event_source();
  KuHttpConnection connections[4]={{0}};
  KuHttpActiveSet active={connections,4,NULL};KuHttpReactor reactor;
  CHECK(ku_http_reactor_storage_bytes(4u,0u)>sizeof(reactor));
  CHECK(ku_http_reactor_init(&reactor,&active,0u));
  for(int i=0;i<4;i++) {
    connections[i].active_set=&active;connections[i].active_position=i;
    connections[i].socket_value=-1;ku_http_reactor_admit(&active,&connections[i]);
  }
  CHECK(reactor.work_count==4 && !reactor.idle_count);
  int first[2],second[2],replacement[2];
  CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,first));
  CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,second));
  CHECK(!socketpair(AF_UNIX,SOCK_STREAM,0,replacement));
  for(int i=0;i<2;i++) {
    KuHttpConnection* c=&connections[i];c->socket_value=i?second[0]:first[0];
    c->connection_generation=UINT64_MAX-100u+(uint64_t)i;
    c->phase=KU_HTTP_IO_HEADER;c->reused=1;c->deadline=(uint64_t)(1000+i*1000);
    ku_http_reactor_refresh(c);
  }
  CHECK(!reactor.fault && reactor.work_count==2 && reactor.idle_count==2);
  CHECK(ku_http_reactor_idle_deadline(&reactor)==1000u);
  uint64_t stable=reactor.registrations[0].token;
  connections[0].deadline=3000u;ku_http_reactor_refresh(&connections[0]);
  CHECK(ku_http_reactor_idle_deadline(&reactor)==2000u);
  CHECK(reactor.registrations[0].token==stable);
  /* RAW-P transfer can leave an untouched first request in the idle heap.
   * Its original accepted-request D, not a later idle timer, wins even before
   * any maintenance worklist pass. No A lease is needed for this empty C. */
  ku_http_reactor_idle_remove(&reactor,&connections[0]);
  connections[0].reused=0;connections[0].request_deadline=750u;
  ku_http_reactor_refresh(&connections[0]);
  CHECK(reactor.idle_initial_count==1 && ku_http_reactor_idle_deadline(&reactor)==750u);
  CHECK(!connections[0].request_lease && connections[0].deadline==3000u);
  ku_http_reactor_idle_remove(&reactor,&connections[0]);
  connections[0].reused=1;connections[0].request_deadline=0;
  ku_http_reactor_refresh(&connections[0]);
  CHECK(!reactor.idle_initial_count && ku_http_reactor_idle_deadline(&reactor)==2000u);
  connections[0].buffered=1u;ku_http_reactor_refresh(&connections[0]);
  CHECK(reactor.work_count==3 && reactor.idle_count==1 && connections[0].idle_position==-1);
  KuHttpPollFd items[8]={{0}};
  CHECK(ku_http_reactor_wait(&reactor,items,0)==0);
  CHECK(write(first[1],"a",1)==1);
  CHECK(ku_http_reactor_wait(&reactor,items,1000)>0 && reactor.ready_count);
  KuHttpReadyEvent old={0};int found=0;
  for(size_t i=0;i<reactor.ready_count;i++) if(reactor.ready[i].entry==0u) {old=reactor.ready[i];found=1;}
  CHECK(found && old.generation==UINT64_MAX-100u && ku_http_reactor_event_valid(&reactor,&old));
  char byte;CHECK(read(first[0],&byte,1)==1 && byte=='a');
  int recycled=first[0];ku_http_reactor_forget(&connections[0]);
  CHECK(!reactor.registrations[0].registered && ku_http_reactor_lookup(&reactor,old.token)==SIZE_MAX);
  CHECK(!close(recycled));CHECK(dup2(replacement[0],recycled)==recycled);CHECK(!close(replacement[0]));
  connections[0].socket_value=recycled;connections[0].buffered=0;
  connections[0].connection_generation=UINT64_MAX-10u;ku_http_reactor_refresh(&connections[0]);
  CHECK(!reactor.fault && reactor.idle_count==2);
  CHECK(reactor.registrations[0].token!=old.token && !ku_http_reactor_event_valid(&reactor,&old));
  CHECK(ku_http_reactor_wait(&reactor,items,0)==0);
  CHECK(write(replacement[1],"b",1)==1);
  CHECK(ku_http_reactor_wait(&reactor,items,1000)>0);
  found=0;
  for(size_t i=0;i<reactor.ready_count;i++) if(reactor.ready[i].entry==0u) {
    CHECK(ku_http_reactor_event_valid(&reactor,&reactor.ready[i]));
    CHECK(reactor.ready[i].generation==UINT64_MAX-10u);found=1;
  }
  CHECK(found && read(recycled,&byte,1)==1 && byte=='b');
  /* Exercise finite open-address back-shift deletion across repeated churn.
   * It must preserve every other live full-width token and leave no tombstones. */
  for(uint64_t round=0;round<1024u;round++) {
    for(uint64_t i=0;i<4u;i++) CHECK(ku_http_reactor_hash_add(&reactor,UINT64_C(1000000)+round*4u+i,6u));
    for(uint64_t i=0;i<4u;i++) {
      uint64_t token=UINT64_C(1000000)+round*4u+i;
      CHECK(ku_http_reactor_lookup(&reactor,token)==6u);ku_http_reactor_hash_remove(&reactor,token);
      CHECK(ku_http_reactor_lookup(&reactor,token)==SIZE_MAX);
      CHECK(ku_http_reactor_lookup(&reactor,reactor.registrations[0].token)==0u);
      CHECK(ku_http_reactor_lookup(&reactor,reactor.registrations[1].token)==1u);
    }
  }
  CHECK(!reactor.fault);
  reactor.next_token=UINT64_MAX-1u;
  ku_http_reactor_interest(&reactor,5u,first[1],KU_HTTP_POLL_READ,0,-1);
  CHECK(reactor.fault && !reactor.registrations[5].registered);
  for(int i=0;i<4;i++) {ku_http_reactor_forget(&connections[i]);ku_http_reactor_retire(&connections[i]);}
  CHECK(!reactor.idle_count && !reactor.work_count);
  CHECK(!close(recycled) && !close(first[1]) && !close(second[0]) && !close(second[1]) && !close(replacement[1]));
  ku_http_reactor_destroy(&reactor);
  puts("http-persistent-owner-generation-idle-ok");return 0;
}
"#;
    let directory = native_c_harness::TempDir::new("http-persistent-owner");
    let path = directory.path().join("persistent.c");
    fs::write(&path, format!("{header}{}{checks}", &runtime[begin..end])).unwrap();
    let Some(executable) = native_c_harness::compile_harness(directory.path(), &path, "persistent")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        &mut Command::new(&executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "status={:?}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"http-persistent-owner-generation-idle-ok\n");
    // Run both independent external-close counterexamples even if the first
    // fails, so retained output distinguishes stale-fd fault from a lost new
    // registration. Root serializes execution; product code is unchanged.
    let mut failures = Vec::new();
    for scenario in [
        "external-net-close",
        "external-net-reuse",
        "external-net-reuse-both",
        "filter-masks",
        "closed-event-source",
    ] {
        let output = run_bounded(
            Command::new(&executable).arg(scenario),
            RUN_TIMEOUT,
            RUN_OUTPUT_LIMITS,
        )
        .unwrap();
        if !output.status.success() {
            failures.push(format!(
                "{scenario}: status={:?}; stdout={}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn native_http_task_runtime_partial_init_reclaims_binding_and_native_resources() {
    if !cfg!(unix) {
        return; // Windows condition initialization has no fallible pthread edge.
    }
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = native_c_harness::TempDir::new("http-task-partial-init");
    let source = r#"
import "std.http"
async fn ready(): HttpResponse! { return ok(http.text("ready")) }
fn main(): null! {
    app = http.server()
    app.get("/", ready)
    app.listen("127.0.0.1:0")?
    return ok(null)
}
"#;
    fs::write(directory.path().join("runtime.ku"), source).unwrap();
    // Emit through the normal CLI, but never execute the Ku application main.
    let output = run_bounded(
        Command::new(ku_binary())
            .current_dir(directory.path())
            .args(["build", "--native", "runtime.ku"]),
        BUILD_TIMEOUT,
        BUILD_OUTPUT_LIMITS,
    )
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "C emission failed: {text}");
    let emitted = text
        .lines()
        .find_map(|line| line.strip_prefix("native c ok: "))
        .expect("CLI reports emitted native C");
    let emitted = PathBuf::from(emitted);
    let emitted = if emitted.is_absolute() {
        emitted
    } else {
        directory.path().join(emitted)
    };
    let mut generated = fs::read_to_string(emitted).unwrap();
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unused_application_main(void) {",
        1,
    );
    let allocator = r#"
static size_t fixture_calloc_attempts,fixture_calloc_fail_at;
static void* fixture_runtime_calloc(size_t count,size_t bytes) {
  fixture_calloc_attempts++;
  if(fixture_calloc_fail_at && fixture_calloc_attempts==fixture_calloc_fail_at) return NULL;
  return ku_perf_calloc(count,bytes);
}
#undef calloc
#define calloc fixture_runtime_calloc
"#;
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    generated = generated.replacen(
        "typedef struct KuString {",
        &format!(
            "{}\n{allocator}\ntypedef struct KuString {{",
            native_allocation_harness::ALLOCATION_HOOK
        ),
        1,
    );
    // Only driver acquisition/destruction is intercepted. HTTP/Task semantics
    // and the actual platform primitives are otherwise the production ones.
    for (before, after, count) in [
        (
            "pthread_mutex_init(&driver->mutex, NULL)",
            "fixture_driver_mutex_init(&driver->mutex, NULL)",
            1,
        ),
        (
            "pthread_mutex_destroy(&driver->mutex)",
            "fixture_driver_mutex_destroy(&driver->mutex)",
            1,
        ),
        (
            "pthread_cond_init(&driver->condition, NULL)",
            "fixture_driver_cond_init(&driver->condition, NULL)",
            1,
        ),
        (
            "pthread_cond_init(&driver->work_condition, NULL)",
            "fixture_driver_cond_init(&driver->work_condition, NULL)",
            1,
        ),
        (
            "pthread_cond_init(&driver->condition, &driver->condition_attributes)",
            "fixture_driver_cond_init(&driver->condition, &driver->condition_attributes)",
            1,
        ),
        (
            "pthread_cond_init(&driver->work_condition, &driver->condition_attributes)",
            "fixture_driver_cond_init(&driver->work_condition, &driver->condition_attributes)",
            1,
        ),
        (
            "pthread_cond_destroy(&driver->condition)",
            "fixture_driver_cond_destroy(&driver->condition)",
            1,
        ),
        (
            "pthread_cond_destroy(&driver->work_condition)",
            "fixture_driver_cond_destroy(&driver->work_condition)",
            1,
        ),
        (
            "pthread_condattr_init(&driver->condition_attributes)",
            "fixture_driver_attr_init(&driver->condition_attributes)",
            1,
        ),
        (
            "pthread_condattr_destroy(&driver->condition_attributes)",
            "fixture_driver_attr_destroy(&driver->condition_attributes)",
            2,
        ),
    ] {
        assert_eq!(
            generated.matches(before).count(),
            count,
            "unique driver edge: {before}"
        );
        generated = generated.replace(before, after);
    }
    let hooks = r#"
static size_t fixture_mutex_inits,fixture_mutex_drops,fixture_cond_inits,fixture_cond_drops;
static size_t fixture_attr_inits,fixture_attr_drops,fixture_cond_attempts,fixture_cond_fail_at;
static int fixture_driver_mutex_init(pthread_mutex_t* value,const pthread_mutexattr_t* attributes) {
  int status=pthread_mutex_init(value,attributes); if(!status)fixture_mutex_inits++; return status;
}
static int fixture_driver_mutex_destroy(pthread_mutex_t* value) {
  int status=pthread_mutex_destroy(value); if(!status)fixture_mutex_drops++; return status;
}
static int fixture_driver_cond_init(pthread_cond_t* value,const pthread_condattr_t* attributes) {
  fixture_cond_attempts++;
  if(fixture_cond_fail_at && fixture_cond_attempts==fixture_cond_fail_at)return ENOMEM;
  int status=pthread_cond_init(value,attributes); if(!status)fixture_cond_inits++; return status;
}
static int fixture_driver_cond_destroy(pthread_cond_t* value) {
  int status=pthread_cond_destroy(value); if(!status)fixture_cond_drops++; return status;
}
static int fixture_driver_attr_init(pthread_condattr_t* value) {
  int status=pthread_condattr_init(value); if(!status)fixture_attr_inits++; return status;
}
static int fixture_driver_attr_destroy(pthread_condattr_t* value) {
  int status=pthread_condattr_destroy(value); if(!status)fixture_attr_drops++; return status;
}
"#;
    let edge = "/* On post-resource failure this object remains LIVE or REAPING.";
    assert_eq!(generated.matches(edge).count(), 1);
    generated = generated.replacen(edge, &format!("{hooks}\n{edge}"), 1);
    generated.push_str(r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"HTTP runtime init line %d: %s\n",__LINE__,#c);return 1;}}while(0)
static void fixture_wake(void* context,size_t owner) {(void)context;(void)owner;}
static void fixture_reset(size_t allocation,size_t condition) {
  fixture_calloc_attempts=0;fixture_calloc_fail_at=allocation;
  fixture_cond_attempts=0;fixture_cond_fail_at=condition;
  fixture_mutex_inits=fixture_mutex_drops=fixture_cond_inits=fixture_cond_drops=0;
  fixture_attr_inits=fixture_attr_drops=0;
}
static uint64_t fixture_deadline(void) {return ku_task_driver_now_ms()+2000u;}
static int fixture_normal(void) {
  fixture_reset(0,0);
  uint64_t deadline=fixture_deadline();
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)ku_http_task_runtime_new(1u,1u,8u*1024u*1024u,NULL,fixture_wake,deadline);
  CHECK(runtime && runtime->capacity==1u && runtime->driver.external_loops);
  CHECK(fixture_calloc_attempts==5u && ku_perf_live_allocations==5u);
  CHECK(runtime->driver.binding_keys && runtime->driver.binding_cells==2u*runtime->driver.capacity);
  CHECK(!runtime->driver.resident && !runtime->active_handles && runtime->fixed_reserved
      && runtime->driver.reserved_bytes==runtime->extra_fixed);
  CHECK(ku_http_task_runtime_close(runtime,deadline)==KU_TASK_DRIVER_OK);
  uint64_t next=UINT64_MAX;
  for(size_t turn=0;turn<8u && runtime->driver.workers_exited!=runtime->driver.workers_created;turn++)
    CHECK(ku_http_task_pump(runtime,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(runtime->driver.workers_exited==runtime->driver.workers_created);
  CHECK(ku_http_task_runtime_free(runtime,deadline)==KU_TASK_DRIVER_OK);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(fixture_mutex_inits==1u && fixture_mutex_drops==1u && fixture_cond_inits==2u && fixture_cond_drops==2u);
  CHECK(fixture_attr_inits==fixture_attr_drops);
  return 0;
}
int main(void) {
  CHECK(!ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes);
  /* Every runtime/table calloc can fail before native acquisition. A following
   * normal creation/free proves this failure did not leave retained backing. */
  for(size_t allocation=1u;allocation<=5u;allocation++) {
    fixture_reset(allocation,0);
    CHECK(!ku_http_task_runtime_new(1u,1u,8u*1024u*1024u,NULL,fixture_wake,fixture_deadline()));
    CHECK(fixture_calloc_attempts==((allocation>=2u && allocation<=4u) ? 4u : allocation));
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
    CHECK(!fixture_mutex_inits && !fixture_cond_attempts && !fixture_attr_inits);
    CHECK(!fixture_normal());
  }
  int failures=0;
  for(size_t round=0;round<3u;round++) {
    size_t failed_condition=1u+round%2u;fixture_reset(0,failed_condition);
    CHECK(!ku_http_task_runtime_new(1u,1u,8u*1024u*1024u,NULL,fixture_wake,fixture_deadline()));
    CHECK(fixture_calloc_attempts==5u && fixture_cond_attempts==failed_condition);
    CHECK(fixture_mutex_inits==1u && fixture_cond_inits==failed_condition-1u);
    int reclaimed=!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow
      && fixture_mutex_drops==fixture_mutex_inits && fixture_cond_drops==fixture_cond_inits
      && fixture_attr_drops==fixture_attr_inits;
    if(!reclaimed) {
      fprintf(stderr,"partial-init round=%zu cond=%zu live_allocations=%zu live_bytes=%zu mutex=%zu/%zu condition=%zu/%zu attributes=%zu/%zu\n",
        round,failed_condition,ku_perf_live_allocations,ku_perf_live_bytes,fixture_mutex_drops,fixture_mutex_inits,
        fixture_cond_drops,fixture_cond_inits,fixture_attr_drops,fixture_attr_inits);
      failures++;
    }
  }
  CHECK(!failures);
  CHECK(!fixture_normal());
  puts("http-task-partial-init-reclaimed-ok");return 0;
}
"#);
    let path = directory.path().join("partial-init.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) =
        native_c_harness::compile_harness(directory.path(), &path, "partial-init")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a C compiler"
        );
        return;
    };
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "status={:?}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"http-task-partial-init-reclaimed-ok\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn native_http_task_runtime_blocking_init_timeout_preserves_cleanup_owner() {
    if !cfg!(unix) {
        return; // This actual-thread creation gate targets POSIX pthreads.
    }
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = native_c_harness::TempDir::new("http-task-blocking-init-owner");
    fs::write(
        directory.path().join("runtime.ku"),
        r#"
import { http, fs } from "std"
async fn ready(): HttpResponse! {
    exists = fs.exists("not-executed-by-this-fixture")
    if(exists) { return ok(http.text("present")) }
    return ok(http.text("absent"))
}
fn main(): null! {
    app = http.server()
    app.get("/", ready)
    app.listen("127.0.0.1:0")?
    return ok(null)
}
"#,
    )
    .unwrap();
    let emission = run_bounded(
        Command::new(ku_binary())
            .current_dir(directory.path())
            .args(["build", "--native", "runtime.ku"]),
        BUILD_TIMEOUT,
        BUILD_OUTPUT_LIMITS,
    )
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&emission.stdout),
        String::from_utf8_lossy(&emission.stderr)
    );
    assert!(
        emission.status.success(),
        "blocking fixture C emission failed: {text}"
    );
    let emitted = PathBuf::from(
        text.lines()
            .find_map(|line| line.strip_prefix("native c ok: "))
            .expect("emitted C path"),
    );
    let emitted = if emitted.is_absolute() {
        emitted
    } else {
        directory.path().join(emitted)
    };
    let mut generated = fs::read_to_string(emitted).unwrap();
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unused_application_main(void) {",
        1,
    );
    let records = r#"
/* A bounded identity ledger distinguishes an old allocation from address reuse.
 * The fs helper definitions precede this hook; their libc realpath allocations
 * and corresponding frees are consistently outside this narrow factory ledger. */
typedef struct {void* pointer;uint64_t serial;} FixtureAllocation;
static FixtureAllocation fixture_allocations[64];
static uint64_t fixture_allocation_serial;
static pthread_mutex_t fixture_record_mutex=PTHREAD_MUTEX_INITIALIZER;
static size_t fixture_record_find(void* pointer) {
  for(size_t i=0;i<64u;i++)if(fixture_allocations[i].pointer==pointer)return i;
  return SIZE_MAX;
}
static void fixture_record_add(void* pointer) {
  if(!pointer)return;
  for(size_t i=0;i<64u;i++)if(!fixture_allocations[i].pointer) {
    if(fixture_allocation_serial==UINT64_MAX)abort();
    fixture_allocations[i]=(FixtureAllocation){pointer,++fixture_allocation_serial};return;
  }
  abort();
}
#undef malloc
#undef calloc
#undef realloc
#undef free
static void* fixture_malloc(size_t bytes) {
  if(pthread_mutex_lock(&fixture_record_mutex))abort();
  void* pointer=ku_perf_malloc(bytes);fixture_record_add(pointer);
  if(pthread_mutex_unlock(&fixture_record_mutex))abort();return pointer;
}
static void* fixture_calloc(size_t count,size_t bytes) {
  if(pthread_mutex_lock(&fixture_record_mutex))abort();
  void* pointer=ku_perf_calloc(count,bytes);fixture_record_add(pointer);
  if(pthread_mutex_unlock(&fixture_record_mutex))abort();return pointer;
}
static void* fixture_realloc(void* old,size_t bytes) {
  if(pthread_mutex_lock(&fixture_record_mutex))abort();
  size_t index=old ? fixture_record_find(old):SIZE_MAX;
  if(old && index==SIZE_MAX)abort();
  void* pointer=ku_perf_realloc(old,bytes);
  if(pointer || !bytes) {
    if(old)fixture_allocations[index]=(FixtureAllocation){0};fixture_record_add(pointer);
  }
  if(pthread_mutex_unlock(&fixture_record_mutex))abort();return pointer;
}
static void fixture_free(void* pointer) {
  if(!pointer)return;
  if(pthread_mutex_lock(&fixture_record_mutex))abort();
  size_t index=fixture_record_find(pointer);if(index==SIZE_MAX)abort();
  fixture_allocations[index]=(FixtureAllocation){0};ku_perf_free(pointer);
  if(pthread_mutex_unlock(&fixture_record_mutex))abort();
}
static uint64_t fixture_serial(void* pointer) {
  if(pthread_mutex_lock(&fixture_record_mutex))abort();
  size_t index=fixture_record_find(pointer);uint64_t serial=index==SIZE_MAX ? 0:fixture_allocations[index].serial;
  if(pthread_mutex_unlock(&fixture_record_mutex))abort();return serial;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
static void *fixture_saved_runtime,*fixture_saved_pool;
static uint64_t fixture_runtime_serial,fixture_pool_serial,fixture_original_deadline;
static uint32_t fixture_init_status,fixture_finish_status;
static int fixture_gate_enabled;
"#;
    // These runtime definitions follow the already-emitted fs helpers. No
    // original application main, filesystem job or HTTP listener is executed.
    let ledger_edge = "#define KU_TASK_DRIVER_COMPUTE_BANK_STATES";
    assert_eq!(generated.matches(ledger_edge).count(), 1);
    generated = generated.replacen(
        ledger_edge,
        &format!(
            "{}\n{}\n{records}\n{ledger_edge}",
            native_c_harness::NATIVE_THREAD_LIFECYCLE_HARNESS,
            native_allocation_harness::ALLOCATION_HOOK
        ),
        1,
    );
    for (before, after) in [
        (
            "pthread_create(&worker->thread,&attributes,ku_task_blocking_thread,worker)",
            "fixture_blocking_create(&worker->thread,&attributes,ku_task_blocking_thread,worker)",
        ),
        (
            "pthread_join(worker->thread,NULL)",
            "fixture_blocking_join(worker->thread,NULL)",
        ),
    ] {
        assert_eq!(
            generated.matches(before).count(),
            1,
            "unique blocking edge {before}"
        );
        generated = generated.replacen(before, after, 1);
    }
    let gates = r#"
static KuTestEvent fixture_entered,fixture_release,fixture_returned;
static _Atomic size_t fixture_gate_failed;
static size_t fixture_real_creates,fixture_real_joins;
static void* (*fixture_original_start)(void*);
static void* fixture_original_argument;
static void* fixture_blocking_trampoline(void* argument) {
  (void)argument;
  if(!ku_test_event_set(&fixture_entered))atomic_store(&fixture_gate_failed,1u);
  if(!ku_test_event_wait(&fixture_release,2000u))atomic_store(&fixture_gate_failed,1u);
  void* result=fixture_original_start(fixture_original_argument);
  if(!ku_test_event_set(&fixture_returned))atomic_store(&fixture_gate_failed,1u);
  return result;
}
static int fixture_blocking_create(pthread_t* thread,const pthread_attr_t* attributes,
                                   void* (*start)(void*),void* argument) {
  if(!fixture_gate_enabled) {
    int status=pthread_create(thread,attributes,start,argument);if(!status)fixture_real_creates++;return status;
  }
  fixture_original_start=start;fixture_original_argument=argument;
  int status=pthread_create(thread,attributes,fixture_blocking_trampoline,NULL);
  if(status)return status;
  fixture_real_creates++;
  if(!ku_test_event_wait(&fixture_entered,1000u))atomic_store(&fixture_gate_failed,1u);
  /* Delay only the successful create return, while real init owns pool.mutex.
   * No fake thread, clock, deadline, EXITED store or destructor error exists. */
  size_t turns=0;
  while(ku_test_real_now_ms()<fixture_original_deadline && turns++<2048u) {
    struct timespec pause={0,1000000L};(void)nanosleep(&pause,NULL);
  }
  if(ku_test_real_now_ms()<fixture_original_deadline)atomic_store(&fixture_gate_failed,1u);
  return status;
}
static int fixture_blocking_join(pthread_t thread,void** result) {
  int status=pthread_join(thread,result);if(!status)fixture_real_joins++;return status;
}
"#;
    let init_edge =
        "static uint32_t ku_task_blocking_init(KuTaskBlockingPoolV1* pool, size_t capacity,";
    assert_eq!(generated.matches(init_edge).count(), 1);
    generated = generated.replacen(init_edge, &format!("{gates}\n{init_edge}"), 1);
    let pool_init = "status=ku_task_blocking_init(runtime->blocking,runtime->driver.capacity,1u,startup_deadline);";
    assert_eq!(generated.matches(pool_init).count(), 1);
    generated = generated.replacen(pool_init,&format!(r#"
  if(fixture_gate_enabled) {{
    fixture_saved_runtime=runtime;fixture_saved_pool=runtime->blocking;
    fixture_runtime_serial=fixture_serial(runtime);fixture_pool_serial=fixture_serial(runtime->blocking);
  }}
  {pool_init}
  if(fixture_gate_enabled)fixture_init_status=status;
"#),1);
    let finish_edge = "(void)ku_task_blocking_finish(runtime->blocking,startup_deadline);";
    assert_eq!(generated.matches(finish_edge).count(), 1);
    generated = generated.replacen(finish_edge,r#"uint32_t fixture_actual_finish=ku_task_blocking_finish(runtime->blocking,startup_deadline);
    if(fixture_gate_enabled)fixture_finish_status=fixture_actual_finish;"#,1);
    generated.push_str(r#"
#define EXPECT(c) do {if(!(c)){fprintf(stderr,"blocking init owner line %d: %s\n",__LINE__,#c);failed=1;}}while(0)
static void fixture_wake(void* context,size_t owner){(void)context;(void)owner;}
static int fixture_release_runtime(KuHttpTaskRuntime* runtime,uint64_t deadline) {
  if(ku_http_task_runtime_close(runtime,deadline)!=KU_TASK_DRIVER_OK)return 0;
  for(size_t turn=0;turn<2048u;turn++) {
    uint64_t next=UINT64_MAX;
    if(runtime->driver.workers_exited!=runtime->driver.workers_created
        && ku_http_task_pump(runtime,0u,32u,&next)>KU_TASK_DRIVER_PENDING)return 0;
    uint32_t status=ku_http_task_runtime_free(runtime,deadline);
    if(status==KU_TASK_DRIVER_OK)return 1;
    if(status!=KU_TASK_DRIVER_PENDING || ku_test_real_now_ms()>=deadline)return 0;
    struct timespec pause={0,1000000L};(void)nanosleep(&pause,NULL);
  }
  return 0;
}
int main(void) {
  int failed=0,returned=0;
  size_t initial_allocations=0,initial_bytes=0,initial_created=0,initial_exited=0;
  uint32_t initial_storage=0;
  size_t returned_exited=0,returned_joined=0;
  EXPECT(!ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes);
  if(!ku_test_event_init(&fixture_entered)||!ku_test_event_init(&fixture_release)||!ku_test_event_init(&fixture_returned))return 2;
  fixture_gate_enabled=1;fixture_original_deadline=ku_test_real_now_ms()+100u;
  void* initial=ku_http_task_runtime_new(1u,1u,8u*1024u*1024u,NULL,fixture_wake,fixture_original_deadline);
  fixture_gate_enabled=0;
  EXPECT(!initial && fixture_real_creates==1u && !atomic_load(&fixture_gate_failed));
  EXPECT(fixture_init_status==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT && fixture_finish_status==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  EXPECT(ku_test_real_now_ms()>=fixture_original_deadline);
  int saved_live=fixture_runtime_serial && fixture_pool_serial
      && fixture_serial(fixture_saved_runtime)==fixture_runtime_serial && fixture_serial(fixture_saved_pool)==fixture_pool_serial;
  EXPECT(saved_live);
  if(saved_live) {
    KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)fixture_saved_runtime;
    KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)fixture_saved_pool;
    initial_allocations=ku_perf_live_allocations;initial_bytes=ku_perf_live_bytes;
    initial_created=pool->workers_created;initial_exited=pool->workers_exited;initial_storage=pool->initialized;
    EXPECT(pool==runtime->blocking && pool->initialized==KU_TASK_DRIVER_STORAGE_LIVE && pool->closing
        && pool->fault==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT && pool->workers_created==1u
        && !pool->workers_exited && !pool->workers_joined && !pool->resident && !pool->queued && !pool->running);
    EXPECT(runtime->driver.initialized==KU_TASK_DRIVER_STORAGE_LIVE && runtime->fixed_reserved
        && !runtime->driver.blocking_pool && runtime->driver.shutdown_deadline==UINT64_MAX);
    EXPECT(ku_perf_live_allocations==7u && ku_perf_live_bytes==runtime->driver.fixed_bytes+runtime->extra_fixed
        +ku_task_blocking_fixed_bytes(runtime->driver.capacity));
  }
  EXPECT(ku_test_event_set(&fixture_release));
  if(fixture_real_creates) {
    returned=ku_test_event_wait(&fixture_returned,2000u);EXPECT(returned);
  }
  EXPECT(!atomic_load(&fixture_gate_failed));
  if(returned && fixture_serial(fixture_saved_pool)==fixture_pool_serial) {
    KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)fixture_saved_pool;
    returned_exited=pool->workers_exited;returned_joined=pool->workers_joined;
    EXPECT(returned_exited==1u && !returned_joined);
  }
  /* A new, separate normal initializer has its own D. It must also reap any
   * retained previous failed-init owner; the first initializer's D is never
   * extended. No test rescue runs until this actual product check has failed. */
  uint64_t normal_deadline=ku_test_real_now_ms()+2000u;
  KuHttpTaskRuntime* normal=(KuHttpTaskRuntime*)ku_http_task_runtime_new(1u,1u,8u*1024u*1024u,NULL,fixture_wake,normal_deadline);
  EXPECT(normal);
  if(normal)EXPECT(fixture_release_runtime(normal,normal_deadline));
  int product_reclaimed=!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow;
  if(!product_reclaimed) {
    fprintf(stderr,"factory NULL at original D=%llu: init=%u finish=%u storage=%u created=%zu exited=%zu allocations=%zu bytes=%zu\n",
      (unsigned long long)fixture_original_deadline,fixture_init_status,fixture_finish_status,initial_storage,
      initial_created,initial_exited,initial_allocations,initial_bytes);
    fprintf(stderr,"failed-init owner retained after actual worker return and normal new/free: old_exited=%zu old_joined=%zu allocations=%zu bytes=%zu joins=%zu/%zu\n",
      returned_exited,returned_joined,ku_perf_live_allocations,ku_perf_live_bytes,fixture_real_joins,fixture_real_creates);
  }
  EXPECT(product_reclaimed);
  /* Only the failed oracle permits test-owned rescue. Matching allocation
   * serials are required before dereferencing pointers a product fix may free. */
  if(returned && fixture_runtime_serial && fixture_pool_serial
      && fixture_serial(fixture_saved_runtime)==fixture_runtime_serial
      && fixture_serial(fixture_saved_pool)==fixture_pool_serial) {
    KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)fixture_saved_runtime;
    KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)fixture_saved_pool;
    uint32_t rescued=ku_task_blocking_finish(pool,fixture_original_deadline);
    EXPECT(rescued==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT && pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO);
    if(pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO) {
      free(pool);runtime->blocking=NULL;
      EXPECT(fixture_release_runtime(runtime,fixture_original_deadline));
      fprintf(stderr,"test-only rescue (does not satisfy product oracle): allocations=%zu bytes=%zu joins=%zu/%zu recorded_failure=%d\n",
        ku_perf_live_allocations,ku_perf_live_bytes,fixture_real_joins,fixture_real_creates,failed);
    }
  }
  EXPECT(fixture_real_creates==2u && fixture_real_joins==2u);
  EXPECT(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  EXPECT(ku_test_event_destroy(&fixture_entered));EXPECT(ku_test_event_destroy(&fixture_release));EXPECT(ku_test_event_destroy(&fixture_returned));
  if(failed)return 1;
  puts("http-blocking-init-owner-reclaimed-ok");return 0;
}
"#);
    let path = directory.path().join("blocking-init-owner.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) =
        native_c_harness::compile_harness(directory.path(), &path, "blocking-init-owner")
    else {
        assert!(
            env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a C compiler"
        );
        return;
    };
    let output = run_bounded(
        &mut Command::new(executable),
        RUN_TIMEOUT,
        RUN_OUTPUT_LIMITS,
    )
    .unwrap();
    // Native HTTP already fail-stops with 80 when live teardown cannot
    // complete by its original deadline. It must not return an ordinary NULL
    // and lose the still-live pool owner. The old path prints its actual leak
    // and test-only rescue evidence, then exits 1 instead of passing.
    assert_eq!(
        output.status.code(), Some(80),
        "live blocking init timeout must use native HTTP teardown exit 80; status={:?}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_http_pure_native_routes_keep_error_fallback_and_exit() {
    let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = r#"
import "std.http"
async fn ready(): HttpResponse! { return ok(http.text("ready")) }
fn main(): null! {
    app=http.server({max_connections:16,max_active_requests:8,max_pending_requests:8})
    app.get("/ok",ready)
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#;
    let address = unused_local_address();
    let Some(mut server) = spawn_native_server_settings(
        "pure-native-error-fallback",
        source,
        &address,
        false,
        Some(3),
        &[
            ("KU_EVENT_LOOPS", "2"),
            ("KU_BLOCKING_THREADS", "17"),
            ("KU_THREAD_BUDGET", "64"),
        ],
    ) else {
        return;
    };
    let watchdog = server.arm_kill_watchdog(RUN_TIMEOUT);
    for (request,status,body) in [
        ("GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", "HTTP/1.1 200 OK", Some("ready")),
        ("GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", "HTTP/1.1 404 Not Found", None),
        ("POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "HTTP/1.1 405 Method Not Allowed", None),
    ] {
        let mut stream=connect_with_retry(&address,Duration::from_secs(3));
        stream.write_all(request.as_bytes()).expect("write exactly once");
        let response=read_http_stream(stream,Duration::from_secs(3));
        assert_status(&response,status);
        if let Some(body)=body { assert!(response.ends_with(body),"{response}"); }
    }
    assert!(watchdog
        .wait_for_natural_exit(Duration::from_secs(5))
        .success());
    assert!(!watchdog.timed_out());
}
