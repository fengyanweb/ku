//! Offline RESP-over-socket integration tests. A local Rust `TcpListener` acts as
//! Redis so framing, binary payloads, timeouts and poisoned-connection behavior are
//! deterministic and never depend on a developer machine's Redis installation.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};

static REDIS_TEST_LOCK: Mutex<()> = Mutex::new(());
const RESP_TEST_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const RESP_TEST_MAX_LINE_BYTES: usize = 1024;
const RESP_TEST_MAX_ARGS: usize = 64;
const RESP_TEST_MAX_BULK_BYTES: usize = 1024 * 1024;
const RESP_TEST_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
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
    let executable = if cfg!(windows) { "ku.exe" } else { "ku" };
    let target_dir = env::var("CARGO_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    [
        target_dir.join("debug").join(executable),
        target_dir.join("release").join(executable),
        repo_root().join("target").join("debug").join(executable),
        repo_root().join("target").join("release").join(executable),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("ku binary not found; set KU_BIN or build the ku binary first")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "ku-native-redis-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create Redis test directory");
    dir
}

fn native_executable_name() -> &'static str {
    if cfg!(windows) {
        "program.exe"
    } else {
        "program"
    }
}

struct NativeProgram {
    dir: PathBuf,
    exe: PathBuf,
    c_source: PathBuf,
}

impl Drop for NativeProgram {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).ok();
    }
}

fn build_native(name: &str, source: &str) -> Option<NativeProgram> {
    let dir = unique_temp_dir(name);
    fs::write(dir.join("main.ku"), source).expect("write Redis Ku source");
    let mut command = Command::new(ku_binary());
    command.current_dir(&dir).args([
        "build",
        "--native",
        "main.ku",
        "-o",
        native_executable_name(),
    ]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native Redis build was not bounded: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() && combined.contains("C compiler not found") {
        eprintln!("skip: no C compiler available for native Redis E2E test");
        fs::remove_dir_all(dir).ok();
        return None;
    }
    if !output.status.success() {
        fs::remove_dir_all(&dir).ok();
        panic!("ku build --native failed for {name}:\n{combined}");
    }
    let c_source = combined
        .lines()
        .find_map(|line| line.strip_prefix("native c ok: "))
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| panic!("native Redis build did not report a C artifact:\n{combined}"));
    let exe = dir.join(native_executable_name());
    if !exe.exists() {
        fs::remove_dir_all(&dir).ok();
        panic!("native Redis executable was not produced");
    }
    Some(NativeProgram { dir, exe, c_source })
}

fn run_with_timeout(exe: &Path, timeout: Duration) -> BoundedOutput {
    run_with_timeout_env(exe, timeout, &[])
}

fn run_with_timeout_env(
    exe: &Path,
    timeout: Duration,
    environment: &[(&str, &str)],
) -> BoundedOutput {
    let mut command = Command::new(exe);
    command.current_dir(exe.parent().expect("program directory"));
    command.envs(environment.iter().copied());
    run_bounded(&mut command, timeout.min(RUN_TIMEOUT), RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
        panic!(
            "native Redis client {} did not complete safely: {error}",
            exe.display()
        )
    })
}

fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("make fake Redis listener nonblocking");
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("make accepted fake Redis socket blocking");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set fake Redis read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .expect("set fake Redis write timeout");
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    started.elapsed() < timeout,
                    "fake Redis server received no connection within {timeout:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept fake Redis client: {error}"),
        }
    }
}

fn join_server_with_timeout<T>(server: thread::JoinHandle<T>, timeout: Duration, label: &str) -> T {
    let started = Instant::now();
    while !server.is_finished() {
        assert!(
            started.elapsed() < timeout,
            "{label} did not stop within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    server
        .join()
        .unwrap_or_else(|_| panic!("{label} thread panicked"))
}

fn read_resp_byte_until(stream: &mut TcpStream, deadline: Instant) -> u8 {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("fake Redis command exceeded its absolute deadline"));
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(200))))
            .expect("set bounded fake Redis read timeout");
        let mut byte = [0_u8; 1];
        match stream.read_exact(&mut byte) {
            Ok(()) => return byte[0],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("read bounded fake Redis byte: {error}"),
        }
    }
}

fn read_resp_exact_until(stream: &mut TcpStream, buffer: &mut [u8], deadline: Instant) {
    let mut offset = 0usize;
    while offset < buffer.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("fake Redis command exceeded its absolute deadline"));
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(200))))
            .expect("set bounded fake Redis read timeout");
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => panic!("fake Redis command ended before its declared bulk length"),
            Ok(read) => offset += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("read bounded fake Redis bulk: {error}"),
        }
    }
}

fn read_resp_line_until(
    stream: &mut TcpStream,
    deadline: Instant,
    frame_bytes: &mut usize,
) -> Vec<u8> {
    let mut line = Vec::new();
    loop {
        let byte = read_resp_byte_until(stream, deadline);
        *frame_bytes = frame_bytes
            .checked_add(1)
            .expect("fake Redis frame byte count overflowed");
        assert!(
            *frame_bytes <= RESP_TEST_MAX_FRAME_BYTES,
            "fake Redis command exceeded {RESP_TEST_MAX_FRAME_BYTES} bytes"
        );
        if byte == b'\r' {
            let newline = read_resp_byte_until(stream, deadline);
            *frame_bytes = frame_bytes
                .checked_add(1)
                .expect("fake Redis frame byte count overflowed");
            assert!(
                *frame_bytes <= RESP_TEST_MAX_FRAME_BYTES,
                "fake Redis command exceeded {RESP_TEST_MAX_FRAME_BYTES} bytes"
            );
            assert_eq!(newline, b'\n', "client emitted malformed RESP line");
            return line;
        }
        assert!(
            line.len() < RESP_TEST_MAX_LINE_BYTES,
            "fake Redis line exceeded {RESP_TEST_MAX_LINE_BYTES} bytes"
        );
        line.push(byte);
    }
}

fn read_command(stream: &mut TcpStream) -> Vec<Vec<u8>> {
    read_command_until(stream, Instant::now() + RESP_TEST_COMMAND_TIMEOUT)
}

fn read_command_until(stream: &mut TcpStream, deadline: Instant) -> Vec<Vec<u8>> {
    let mut frame_bytes = 0usize;
    let array = read_resp_line_until(stream, deadline, &mut frame_bytes);
    assert_eq!(array.first(), Some(&b'*'));
    let argc: usize = std::str::from_utf8(&array[1..])
        .expect("UTF-8 argc")
        .parse()
        .expect("numeric argc");
    assert!(
        argc <= RESP_TEST_MAX_ARGS,
        "fake Redis command exceeded {RESP_TEST_MAX_ARGS} arguments"
    );
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        let bulk = read_resp_line_until(stream, deadline, &mut frame_bytes);
        assert_eq!(bulk.first(), Some(&b'$'));
        let len: usize = std::str::from_utf8(&bulk[1..])
            .expect("UTF-8 bulk length")
            .parse()
            .expect("numeric bulk length");
        assert!(
            len <= RESP_TEST_MAX_BULK_BYTES,
            "fake Redis bulk exceeded {RESP_TEST_MAX_BULK_BYTES} bytes"
        );
        assert!(
            frame_bytes
                .checked_add(len)
                .and_then(|total| total.checked_add(2))
                .is_some_and(|total| total <= RESP_TEST_MAX_FRAME_BYTES),
            "fake Redis command exceeded {RESP_TEST_MAX_FRAME_BYTES} bytes"
        );
        let mut value = vec![0_u8; len];
        read_resp_exact_until(stream, &mut value, deadline);
        let mut ending = [0_u8; 2];
        read_resp_exact_until(stream, &mut ending, deadline);
        assert_eq!(ending, *b"\r\n");
        frame_bytes += len + 2;
        args.push(value);
    }
    args
}

#[test]
fn native_redis_fake_resp_parser_bounds_untrusted_lengths() {
    for (label, frame) in [
        ("argument count", b"*65\r\n".as_slice()),
        ("bulk length", b"*1\r\n$1048577\r\n".as_slice()),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind parser limit listener");
        let address = listener.local_addr().expect("parser limit address");
        let mut client = TcpStream::connect(address).expect("connect parser limit client");
        let (mut server, _) = listener.accept().expect("accept parser limit client");
        client.write_all(frame).expect("write oversized RESP frame");
        client
            .shutdown(Shutdown::Write)
            .expect("finish oversized RESP frame");
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            read_command_until(&mut server, Instant::now() + Duration::from_millis(200))
        }));
        assert!(failure.is_err(), "untrusted {label} was accepted");
    }
}

fn write_fragmented(stream: &mut TcpStream, response: &[u8]) {
    let mut at = 0;
    let mut chunk = 1;
    while at < response.len() {
        let end = (at + chunk).min(response.len());
        stream
            .write_all(&response[at..end])
            .expect("write fragmented Redis response");
        stream.flush().expect("flush Redis response");
        at = end;
        chunk = if chunk == 3 { 1 } else { chunk + 1 };
    }
}

fn http_get_with_timeout(
    address: SocketAddr,
    path: &str,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    const RESPONSE_LIMIT: usize = 64 * 1024;
    let started = Instant::now();
    let mut last_connect_error = None;
    let mut stream = loop {
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "HTTP connect to {address} timed out: {}",
                    last_connect_error
                        .map(|error: io::Error| error.to_string())
                        .unwrap_or_else(|| "no connection attempt completed".to_string())
                ),
            ));
        };
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("HTTP connect to {address} timed out"),
            ));
        }
        match TcpStream::connect_timeout(&address, remaining.min(Duration::from_millis(200))) {
            Ok(stream) => break stream,
            Err(error) => {
                last_connect_error = Some(error);
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }
    };

    let remaining = timeout.checked_sub(started.elapsed()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP request timed out before write",
        )
    })?;
    stream.set_write_timeout(Some(remaining))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    stream.shutdown(Shutdown::Write)?;

    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "HTTP response timed out"))?;
        stream.set_read_timeout(Some(remaining.min(Duration::from_millis(500))))?;
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(response),
            Ok(read) => {
                if response.len().saturating_add(read) > RESPONSE_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "HTTP response exceeded test limit",
                    ));
                }
                response.extend_from_slice(&buffer[..read]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn native_redis_binary_acl_and_server_error_keep_connection_usable() {
    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Redis");
    let port = listener.local_addr().expect("fake Redis address").port();
    let source = format!(
        r#"
import redis from "std.redis"
fn main(): null! {{
    client = redis.client({{
        host: "127.0.0.1", port: {port}, username: "alice", password: "secret",
        max_connections: 1, connect_timeout_ms: 1000,
        acquire_timeout_ms: 1000, command_timeout_ms: 1000
    }})?
    client.ping()?
    try {{
        ignored = client.get("server-error")?
        println(ignored)
    }} catch (err) {{
        println("server-error")
    }}
    try {{
        missing = client.get("missing")?
        println(missing)
    }} catch (err) {{
        println(err.code)
        println(err.message)
    }}
    client.set("binary", "a\r\nb")?
    value = client.get("binary")?
    println(value.len())
    empty = client.get("empty")?
    println(empty.len())
    try {{
        invalid_error = client.get("invalid-error")?
        println(invalid_error)
    }} catch (err) {{
        println(err.code)
        println(err.message)
    }}
    println(client.exists("after-error")?)
    try {{
        invalid = client.get("invalid-utf8")?
        println(invalid)
    }} catch (err) {{
        println(err.code)
        println(err.message)
    }}
    println(client.exists("binary")?)
    println(client.del("binary")?)
    client.close()
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("success", &source) else {
        return;
    };
    let generated = fs::read_to_string(&program.c_source).expect("read generated Redis C");
    assert!(generated.contains("#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1"));
    assert!(generated.contains("typedef SOCKET KuRedisSocket;"));
    assert!(generated.contains("typedef int KuRedisSocket;"));
    assert!(generated.contains("CreateSemaphoreW(NULL, 1, 1, NULL)"));
    assert!(generated.contains("WaitForSingleObject(r->command_gate.semaphore, wait_ms)"));
    assert!(generated.contains("ReleaseSemaphore(r->command_gate.semaphore, 1, NULL)"));
    assert!(generated.contains("CloseHandle(gate->semaphore)"));
    assert!(generated.contains("pthread_cond_timedwait_relative_np"));
    assert!(generated.contains("pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC)"));
    assert!(generated.contains("poll(&descriptor, 1, wait_ms)"));
    assert!(generated.contains("send(socket_value, data, chunk, MSG_NOSIGNAL)"));
    assert!(generated.contains("setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE"));
    assert!(!generated.contains("TryEnterCriticalSection(&r->lock)"));
    assert!(!generated.contains("Sleep(1);"));
    assert!(generated.contains("__ku_handler_deadline != 0 && __ku_handler_deadline < deadline"));
    assert!(generated.contains("ku_redis_send_cmd(r, argc, args, deadline)"));
    assert!(generated.contains("#define KU_REDIS_IO_TIMEOUT (-4)"));
    assert!(generated.contains("ku_redis_socket_error_timed_out"));
    let validation = generated
        .find("int utf8_valid = ku_redis_utf8_valid")
        .expect("bulk UTF-8 validation must be emitted");
    let validation_tail = &generated[validation..];
    assert!(
        validation_tail
            .find("ku_redis_deadline_alive(r)")
            .expect("deadline must be rechecked after bulk validation")
            < validation_tail
                .find("if (!utf8_valid)")
                .expect("UTF-8 result must be handled after the deadline check")
    );
    assert!(generated.contains("return ku_redis_command_timeout_err()"));
    assert!(!generated.contains("std.redis native backend is Winsock-only"));
    assert!(generated.contains("struct KuRedisClient"));
    assert!(generated.contains("SleepConditionVariableSRW"));
    assert!(generated.contains("client->max_waiters"));
    assert!(generated.contains("ku_redis_ping"));
    assert!(generated.contains("ku_redis_utf8_valid"));
    let server_error = generated
        .split_once("static KuError ku_redis_err_n(")
        .expect("Redis server error sanitizer")
        .1
        .split_once("static KuError ku_redis_err(")
        .expect("Redis internal error helper")
        .0;
    assert!(server_error.contains("redis server returned an error"));
    assert!(!server_error.contains("ku_string_owned_copy"));
    let auth = generated
        .split_once("static KuResult_null ku_redis_simple_expected_locked(")
        .expect("Redis AUTH reply parser")
        .1
        .split_once("static KuResult_null ku_redis_simple_expected(")
        .expect("Redis simple reply wrapper")
        .0;
    assert!(auth.contains("if (read_rc != 0) return"));
    assert!(auth.contains("line[0] == '-'"));
    assert!(auth.contains("redact_server_error ? ku_redis_auth_failed_err()"));
    let handoff = generated
        .split_once("static void ku_redis_client_handoff_available_locked(")
        .expect("Redis waiter handoff helper")
        .1
        .split_once("static KuRedisLeaseResult ku_redis_client_acquire(")
        .expect("Redis client acquire after handoff")
        .0;
    assert!(handoff.contains("client->waiters != 0"));
    assert!(handoff.contains("ku_redis_pool_wake_one(&client->sync)"));
    let acquire = generated
        .split_once("static KuRedisLeaseResult ku_redis_client_acquire(")
        .expect("Redis client acquire")
        .1
        .split_once("static void ku_redis_client_release(")
        .expect("Redis client release after acquire")
        .0;
    assert!(acquire.contains("else ku_redis_client_handoff_available_locked(client)"));

    let server = thread::spawn(move || {
        let mut stream = accept_with_timeout(&listener, Duration::from_secs(8));
        let responses: [&[u8]; 12] = [
            b"+OK\r\n",
            b"+PONG\r\n",
            b"-ERR deliberate test error\r\n",
            b"$-1\r\n",
            b"+OK\r\n",
            b"$4\r\na\r\nb\r\n",
            b"$0\r\n\r\n",
            b"-\xc3(\r\n",
            b":1\r\n",
            b"$2\r\n\xc3(\r\n",
            b":1\r\n",
            b":1\r\n",
        ];
        let mut commands = Vec::new();
        for response in responses {
            commands.push(read_command(&mut stream));
            write_fragmented(&mut stream, response);
        }
        commands
    });

    let output = run_with_timeout(&program.exe, Duration::from_secs(8));
    assert!(
        output.status.success(),
        "native Redis client failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "server-error\nkey_not_found\nredis key does not exist\n4\n0\ninvalid_utf8\nredis response text is not valid UTF-8\ntrue\ninvalid_utf8\nredis response text is not valid UTF-8\ntrue\n1\n"
    );
    let commands = join_server_with_timeout(server, Duration::from_secs(5), "fake Redis server");
    assert_eq!(
        commands[0],
        [b"AUTH".to_vec(), b"alice".to_vec(), b"secret".to_vec()]
    );
    assert_eq!(commands[1], [b"PING".to_vec()]);
    assert_eq!(commands[2], [b"GET".to_vec(), b"server-error".to_vec()]);
    assert_eq!(commands[3], [b"GET".to_vec(), b"missing".to_vec()]);
    assert_eq!(
        commands[4],
        [b"SET".to_vec(), b"binary".to_vec(), b"a\r\nb".to_vec()]
    );
    assert_eq!(commands[5], [b"GET".to_vec(), b"binary".to_vec()]);
    assert_eq!(commands[6], [b"GET".to_vec(), b"empty".to_vec()]);
    assert_eq!(commands[7], [b"GET".to_vec(), b"invalid-error".to_vec()]);
    assert_eq!(commands[8], [b"EXISTS".to_vec(), b"after-error".to_vec()]);
    assert_eq!(commands[9], [b"GET".to_vec(), b"invalid-utf8".to_vec()]);
    assert_eq!(commands[10], [b"EXISTS".to_vec(), b"binary".to_vec()]);
    assert_eq!(commands[11], [b"DEL".to_vec(), b"binary".to_vec()]);
}

#[test]
fn native_redis_auth_failure_does_not_reflect_credentials() {
    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Redis");
    let port = listener.local_addr().expect("fake Redis address").port();
    let source = format!(
        r#"
import redis from "std.redis"
fn main(): null! {{
    try {{
        client = redis.client({{
            host: "127.0.0.1", port: {port}, username: "alice", password: "secret",
            connect_timeout_ms: 1000, acquire_timeout_ms: 1000,
            command_timeout_ms: 1000
        }})?
        client.close()
        println("unexpected")
    }} catch (err) {{
        println(err.code)
        println(err.message)
    }}
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("auth-sanitized", &source) else {
        return;
    };
    let server = thread::spawn(move || {
        let mut stream = accept_with_timeout(&listener, Duration::from_secs(5));
        let command = read_command(&mut stream);
        write_fragmented(&mut stream, b"-ERR user alice supplied password secret\r\n");
        let mut unexpected = [0_u8; 1];
        assert_eq!(
            stream.read(&mut unexpected).unwrap_or(0),
            0,
            "failed AUTH connection remained open"
        );
        command
    });

    let output = run_with_timeout(&program.exe, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "Redis AUTH failure was not catchable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "auth_failed\nredis authentication failed\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    let command = join_server_with_timeout(server, Duration::from_secs(5), "fake Redis server");
    assert_eq!(
        command,
        [b"AUTH".to_vec(), b"alice".to_vec(), b"secret".to_vec()]
    );
}

#[test]
fn native_redis_auth_transport_close_and_timeout_are_not_auth_failed() {
    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let closed_listener = TcpListener::bind("127.0.0.1:0").expect("bind closing fake Redis");
    let closed_port = closed_listener
        .local_addr()
        .expect("closing fake Redis address")
        .port();
    let timeout_listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled fake Redis");
    let timeout_port = timeout_listener
        .local_addr()
        .expect("stalled fake Redis address")
        .port();
    let source = format!(
        r#"
import redis from "std.redis"
fn Probe(port: int, label: str): null! {{
    try {{
        client = redis.client({{
            host: "127.0.0.1", port: port, username: "alice", password: "secret",
            connect_timeout_ms: 500, acquire_timeout_ms: 500, command_timeout_ms: 150
        }})?
        client.close()
        println(label + ":unexpected")
    }} catch (err) {{
        println(label + ":" + err.code)
    }}
    return ok(null)
}}
fn main(): null! {{
    Probe({closed_port}, "closed")?
    Probe({timeout_port}, "timeout")?
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("auth-transport-classification", &source) else {
        return;
    };
    let closed_server = thread::spawn(move || {
        let mut stream = accept_with_timeout(&closed_listener, Duration::from_secs(5));
        let command = read_command(&mut stream);
        stream
            .shutdown(Shutdown::Both)
            .expect("close AUTH transport fixture");
        command
    });
    let timeout_server = thread::spawn(move || {
        let mut stream = accept_with_timeout(&timeout_listener, Duration::from_secs(5));
        let command = read_command(&mut stream);
        thread::sleep(Duration::from_millis(600));
        let mut unexpected = [0_u8; 1];
        assert_eq!(
            stream.read(&mut unexpected).unwrap_or(0),
            0,
            "timed-out AUTH connection remained open"
        );
        command
    });

    let output = run_with_timeout(&program.exe, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "Redis AUTH transport failures were not catchable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, "closed:redis_error\ntimeout:timeout\n");
    assert!(!stdout.contains("auth_failed"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    for command in [
        join_server_with_timeout(closed_server, Duration::from_secs(3), "closing AUTH server"),
        join_server_with_timeout(
            timeout_server,
            Duration::from_secs(3),
            "stalled AUTH server",
        ),
    ] {
        assert_eq!(
            command,
            [b"AUTH".to_vec(), b"alice".to_vec(), b"secret".to_vec()]
        );
    }
}

#[test]
fn native_redis_connects_to_ipv6_loopback_when_available() {
    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let listener = match TcpListener::bind("[::1]:0") {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("skip: IPv6 loopback is unavailable: {error}");
            return;
        }
    };
    let port = listener
        .local_addr()
        .expect("fake IPv6 Redis address")
        .port();
    let source = format!(
        r#"
import redis from "std.redis"
fn main(): null! {{
    client = redis.client({{ host: "::1", port: {port}, max_connections: 1,
        connect_timeout_ms: 1000, acquire_timeout_ms: 1000, command_timeout_ms: 1000 }})?
    println(client.exists("missing")?)
    client.close()
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("ipv6", &source) else {
        return;
    };
    let server = thread::spawn(move || {
        let mut stream = accept_with_timeout(&listener, Duration::from_secs(5));
        let command = read_command(&mut stream);
        write_fragmented(&mut stream, b":0\r\n");
        command
    });
    let output = run_with_timeout(&program.exe, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "IPv6 Redis client failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "false\n"
    );
    assert_eq!(
        join_server_with_timeout(server, Duration::from_secs(5), "IPv6 fake Redis server"),
        [b"EXISTS".to_vec(), b"missing".to_vec()]
    );
}

#[cfg(unix)]
#[test]
fn native_redis_peer_close_is_an_error_not_a_sigpipe_process_exit() {
    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Redis");
    let port = listener.local_addr().expect("fake Redis address").port();
    let source = format!(
        r#"
import redis from "std.redis"
import time from "std.time"
fn main(): null! {{
    client = redis.client({{ host: "127.0.0.1", port: {port}, max_connections: 1,
        connect_timeout_ms: 1000, acquire_timeout_ms: 1000, command_timeout_ms: 1000 }})?
    client.ping()?
    started = time.steady_millis()
    while (time.steady_millis() - started < 150) {{
    }}
    try {{
        client.set("after-close", "value")?
        println("unexpected")
    }} catch (err) {{
        println("survived:" + err.domain)
    }}
    client.close()
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("peer-close", &source) else {
        return;
    };
    let server = thread::spawn(move || {
        let mut stream = accept_with_timeout(&listener, Duration::from_secs(5));
        assert_eq!(read_command(&mut stream), vec![b"PING".to_vec()]);
        stream.write_all(b"+PONG\r\n").expect("write PONG");
        stream.flush().expect("flush PONG");
        let _ = stream.shutdown(Shutdown::Both);
    });

    let output = run_with_timeout(&program.exe, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "peer-close probe must return a Ku error instead of terminating by SIGPIPE: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "survived:redis\n"
    );
    join_server_with_timeout(server, Duration::from_secs(3), "peer-close fake Redis");
}

enum BadReply {
    Bytes(Vec<u8>),
    Truncated(Vec<u8>),
    Trickle(Vec<u8>),
    Stall,
}

#[test]
fn native_redis_rejects_malformed_or_oversized_replies_and_poisons_socket() {
    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Redis");
    let port = listener.local_addr().expect("fake Redis address").port();
    let source = format!(
        r#"
import redis from "std.redis"
fn probe_get(port: int, label: str): null! {{
    client = redis.client({{ host: "127.0.0.1", port: port, max_connections: 1,
        connect_timeout_ms: 200, acquire_timeout_ms: 200, command_timeout_ms: 200 }})?
    try {{
        value = client.get(label)?
        println(value)
    }} catch (err) {{
        println(label + ":rejected")
    }}
    client.exists("after")?
    println(label + ":recovered")
    client.close()
    return ok(null)
}}
fn probe_exists(port: int, label: str): null! {{
    client = redis.client({{ host: "127.0.0.1", port: port, max_connections: 1,
        connect_timeout_ms: 200, acquire_timeout_ms: 200, command_timeout_ms: 200 }})?
    try {{
        println(client.exists(label)?)
    }} catch (err) {{
        println(label + ":rejected")
    }}
    client.exists("after")?
    println(label + ":recovered")
    client.close()
    return ok(null)
}}
fn probe_set(port: int, label: str): null! {{
    client = redis.client({{ host: "127.0.0.1", port: port, max_connections: 1,
        connect_timeout_ms: 200, acquire_timeout_ms: 200, command_timeout_ms: 200 }})?
    try {{
        client.set(label, "value")?
        println("unexpected")
    }} catch (err) {{
        println(label + ":rejected")
    }}
    client.exists("after")?
    println(label + ":recovered")
    client.close()
    return ok(null)
}}
fn main(): null! {{
    probe_get({port}, "bad-crlf")?
    probe_get({port}, "bad-length")?
    probe_get({port}, "bad-nil")?
    probe_get({port}, "overflow")?
    probe_get({port}, "oversized")?
    probe_get({port}, "long-line")?
    probe_get({port}, "truncated")?
    probe_get({port}, "stalled")?
    probe_get({port}, "trickle")?
    probe_exists({port}, "bad-integer")?
    probe_exists({port}, "plus-integer")?
    probe_set({port}, "bad-simple")?
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("malformed", &source) else {
        return;
    };

    let mut long_line = vec![b'-'];
    long_line.extend(std::iter::repeat_n(b'A', 4096));
    long_line.extend_from_slice(b"\r\n");
    let scenarios = vec![
        BadReply::Bytes(b"$3\r\nabcXY".to_vec()),
        BadReply::Bytes(b"$abc\r\n".to_vec()),
        BadReply::Bytes(b"$-2\r\n".to_vec()),
        BadReply::Bytes(b"$9223372036854775808\r\n".to_vec()),
        BadReply::Bytes(b"$67108865\r\n".to_vec()),
        BadReply::Bytes(long_line),
        BadReply::Truncated(b"$3\r".to_vec()),
        BadReply::Stall,
        BadReply::Trickle(b"$3\r\nabc\r\n".to_vec()),
        BadReply::Bytes(b":not-an-integer\r\n".to_vec()),
        BadReply::Bytes(b":+1\r\n".to_vec()),
        BadReply::Bytes(b"+NOPE\r\n".to_vec()),
    ];
    let server = thread::spawn(move || {
        let mut commands = Vec::new();
        let mut slow_replies = Vec::new();
        for scenario in scenarios {
            let mut stream = Some(accept_with_timeout(&listener, Duration::from_secs(10)));
            commands.push(read_command(
                stream.as_mut().expect("Redis scenario stream"),
            ));
            let slow_reply = match scenario {
                BadReply::Bytes(bytes) => {
                    let stream = stream.as_mut().expect("Redis byte reply stream");
                    let _ = stream.write_all(&bytes);
                    let _ = stream.flush();
                    None
                }
                BadReply::Truncated(bytes) => {
                    let stream = stream.as_mut().expect("Redis truncated reply stream");
                    let _ = stream.write_all(&bytes);
                    let _ = stream.shutdown(Shutdown::Write);
                    None
                }
                BadReply::Trickle(bytes) => {
                    let mut stream = stream.take().expect("Redis trickle reply stream");
                    Some(thread::spawn(move || {
                        for byte in bytes {
                            if stream.write_all(&[byte]).is_err() {
                                return;
                            }
                            thread::sleep(Duration::from_millis(80));
                        }
                    }))
                }
                BadReply::Stall => {
                    let mut stream = stream.take().expect("Redis stalled reply stream");
                    Some(thread::spawn(move || {
                        thread::sleep(Duration::from_millis(500));
                        let mut unexpected = [0_u8; 1];
                        assert_eq!(
                            stream.read(&mut unexpected).unwrap_or(0),
                            0,
                            "timed-out Redis connection remained usable"
                        );
                    }))
                }
            };
            if let Some(slow_reply) = slow_reply {
                slow_replies.push(slow_reply);
            } else {
                let mut unexpected = [0_u8; 1];
                assert_eq!(
                    stream
                        .as_mut()
                        .expect("Redis poisoned reply stream")
                        .read(&mut unexpected)
                        .unwrap_or(0),
                    0,
                    "poisoned Redis connection sent another command"
                );
            }
            let mut replacement = accept_with_timeout(&listener, Duration::from_secs(10));
            let recovery = read_command(&mut replacement);
            assert_eq!(recovery, [b"EXISTS".to_vec(), b"after".to_vec()]);
            commands.push(recovery);
            write_fragmented(&mut replacement, b":0\r\n");
        }
        for slow_reply in slow_replies {
            slow_reply.join().expect("slow Redis reply worker");
        }
        commands
    });

    let output = run_with_timeout(&program.exe, Duration::from_secs(10));
    assert!(
        output.status.success(),
        "malformed reply probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    for label in [
        "bad-crlf",
        "bad-length",
        "bad-nil",
        "overflow",
        "oversized",
        "long-line",
        "truncated",
        "stalled",
        "trickle",
        "bad-integer",
        "plus-integer",
        "bad-simple",
    ] {
        assert!(stdout.contains(&format!("{label}:rejected\n")), "{stdout}");
        assert!(stdout.contains(&format!("{label}:recovered\n")), "{stdout}");
    }
    let commands = join_server_with_timeout(server, Duration::from_secs(5), "fake Redis server");
    assert_eq!(commands.len(), 24);
    assert_eq!(commands[18][0], b"EXISTS");
    assert_eq!(commands[20][0], b"EXISTS");
    assert_eq!(commands[22][0], b"SET");
}

#[test]
fn native_redis_http_shared_connection_serializes_32_concurrent_requests() {
    const REQUESTS: usize = 32;
    const HTTP_CLIENT_TIMEOUT: Duration = Duration::from_secs(12);
    const NATIVE_TIMEOUT: Duration = Duration::from_secs(20);

    let _guard = REDIS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let redis_listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Redis");
    let redis_port = redis_listener
        .local_addr()
        .expect("fake Redis address")
        .port();
    let http_probe = TcpListener::bind("127.0.0.1:0").expect("reserve native HTTP port");
    let http_address = http_probe.local_addr().expect("native HTTP address");
    drop(http_probe);

    let source = format!(
        r#"
import http from "std.http"
import redis from "std.redis"
fn main(): null! {{
    cache = redis.client({{ host: "127.0.0.1", port: {redis_port}, max_connections: 1,
        max_waiters: 64, connect_timeout_ms: 10000,
        acquire_timeout_ms: 10000, command_timeout_ms: 10000 }})?
    app = http.server({{
        max_connections: 64,
        max_active_requests: 8,
        max_pending_requests: 64,
        handler_timeout_ms: 10000,
        read_header_timeout_ms: 5000,
        read_body_timeout_ms: 5000,
        write_timeout_ms: 5000,
        idle_timeout_ms: 5000
    }})
    app.get("/item/{{id}}", fn(req) {{
        status = 500
        body = "redis-error"
        try {{
            body = cache.get(req.params.id)?
            status = 200
        }} catch (err) {{
            body = err.code
        }}
        return http.text(status, body)
    }})
    app.listen("{http_address}")?
    cache.close()
    return ok(null)
}}
"#
    );
    let Some(program) = build_native("http-shared-connection", &source) else {
        return;
    };

    let generated = fs::read_to_string(&program.c_source).expect("read generated HTTP + Redis C");
    assert!(generated.contains("#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1"));
    assert!(generated.contains("#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1"));
    assert!(generated.contains("ku_redis_lock_until(r, deadline)"));
    assert!(generated.contains("pthread_create(&worker, NULL, ku_http_worker, &ctx)"));

    let redis_server = thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut stream = accept_with_timeout(&redis_listener, Duration::from_secs(8));
            let mut ids = Vec::with_capacity(REQUESTS);
            for _ in 0..REQUESTS {
                let command = read_command(&mut stream);
                assert_eq!(command.len(), 2, "expected GET plus one key");
                assert_eq!(command[0], b"GET", "unexpected Redis command");
                let id = std::str::from_utf8(&command[1])
                    .expect("HTTP route id must be UTF-8")
                    .parse::<usize>()
                    .expect("HTTP route id must be numeric");
                assert!(id < REQUESTS, "unexpected HTTP route id {id}");
                ids.push(id);

                // Keep the first worker inside the complete command/response
                // critical section long enough for the other seven workers to
                // contend on the same Redis gate.
                thread::sleep(Duration::from_millis(15));
                let body = format!("value-{id}");
                let response = format!("${}\r\n{}\r\n", body.len(), body);
                write_fragmented(&mut stream, response.as_bytes());
            }

            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set Redis EOF timeout");
            let mut trailing = [0_u8; 1];
            assert_eq!(
                stream.read(&mut trailing).expect("wait for Redis EOF"),
                0,
                "native program sent bytes after the expected 32 commands"
            );
            ids
        }))
    });

    let executable = program.exe.clone();
    let native_runner = thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with_timeout_env(
                &executable,
                NATIVE_TIMEOUT,
                &[("KU_HTTP_MAX_REQUESTS", "32")],
            )
        }))
    });

    let start = Arc::new(Barrier::new(REQUESTS + 1));
    let mut clients = Vec::with_capacity(REQUESTS);
    for id in 0..REQUESTS {
        let start = Arc::clone(&start);
        clients.push(thread::spawn(move || {
            start.wait();
            let response =
                http_get_with_timeout(http_address, &format!("/item/{id}"), HTTP_CLIENT_TIMEOUT);
            (id, response)
        }));
    }
    start.wait();
    let client_results = clients
        .into_iter()
        .map(thread::JoinHandle::join)
        .collect::<Vec<_>>();

    let native_result = join_server_with_timeout(
        native_runner,
        Duration::from_secs(25),
        "native HTTP + Redis program",
    );
    let redis_result = join_server_with_timeout(
        redis_server,
        Duration::from_secs(8),
        "shared-connection fake Redis server",
    );

    let output = match native_result {
        Ok(output) => output,
        Err(_) => panic!("native HTTP + Redis program panicked"),
    };
    assert!(
        output.status.success(),
        "native HTTP + Redis program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut observed_ids = match redis_result {
        Ok(ids) => ids,
        Err(_) => panic!("shared-connection fake Redis server panicked"),
    };
    observed_ids.sort_unstable();
    assert_eq!(observed_ids, (0..REQUESTS).collect::<Vec<_>>());

    for joined in client_results {
        let (id, response) = joined.expect("HTTP client thread panicked");
        let response = response.unwrap_or_else(|error| panic!("HTTP client {id} failed: {error}"));
        let text = std::str::from_utf8(&response)
            .unwrap_or_else(|error| panic!("HTTP client {id} received non-UTF-8: {error}"));
        assert!(
            text.starts_with("HTTP/1.1 200 OK\r\n"),
            "HTTP client {id} did not receive 200:\n{text}"
        );
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or_else(|| panic!("HTTP client {id} received no response body"));
        assert_eq!(body, format!("value-{id}"));
    }
}
