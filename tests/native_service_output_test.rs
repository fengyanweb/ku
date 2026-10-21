//! A stalled service log consumer cannot block request/Task output producers.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use native_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

struct ChildOwner(Child);

impl Drop for ChildOwner {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            self.0.kill().ok();
        }
        self.0.wait().ok();
    }
}

fn build(directory: &TempDir) -> Option<std::path::PathBuf> {
    let source = directory.path().join("service-output.c");
    fs::write(
        &source,
        format!(
            r#"
#if !defined(_WIN32)
#define _POSIX_C_SOURCE 200809L
#endif
#if defined(__APPLE__)
#define _DARWIN_C_SOURCE
#endif
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <errno.h>
#if defined(_WIN32)
#include <windows.h>
#endif
#define KU_SERVICE_OUTPUT_SUPPORTED 1
static size_t ku_runtime_thread_stack_bytes(void) {{ return 2097152; }}
{}
int main(int argc, char** argv) {{
  if (!ku_service_output_start()) return 10;
  if (argc > 1 && !strcmp(argv[1], "normal")) {{
    if (ku_output_printf("%d:%s:", 42, "Ku") < 0) return 11;
    const unsigned char text[] = {{0xe4,0xb8,0xad,0,0xe6,0x96,0x87,'\n'}};
    if (ku_output_write(text, sizeof(text)) < 0) return 12;
    return ku_service_output_exit(0);
  }}
  unsigned char data[4096]; memset(data, 'x', sizeof(data));
  if (argc > 1 && !strcmp(argv[1], "closed")) {{
    if (fgetc(stdin) != 'g') return 17;
    if (ku_output_write(data,sizeof(data)) < 0) return 15;
    unsigned long long deadline=ku_output_now_ms()+1000;
    ku_output_lock();
    while (!ku_service_output.metrics.failures)
      if (ku_output_wait(deadline)<=0) {{ ku_output_unlock(); return 16; }}
    ku_output_unlock();
    fputs("closed collector recorded failure\n",stderr); fflush(stderr);
    return ku_service_output_exit(0);
  }}
  for (size_t index=0; index<10000; index++)
    if (ku_output_write(data, sizeof(data)) < 0) return 13;
  KuServiceOutputSnapshot snapshot=ku_service_output_snapshot();
  if (!snapshot.dropped_records || !snapshot.dropped_bytes
      || snapshot.queued_records>256 || snapshot.queued_bytes>1048576
      || snapshot.peak_bytes>1048576) return 14;
  fprintf(stderr,"bounded output: %zu %zu %zu %zu\n",snapshot.queued_records,
      snapshot.queued_bytes,snapshot.dropped_records,snapshot.dropped_bytes);
  fflush(stderr);
  return ku_service_output_exit(0);
}}
"#,
            include_str!("../src/backend/c_service_output.c")
        ),
    )
    .expect("write service output regression");
    compile_harness(directory.path(), &source, "service-output")
}

#[test]
fn service_output_stalled_pipe_is_bounded_and_exit_does_not_join_forever() {
    let directory = TempDir::new("service-output-stall");
    let Some(executable) = build(&directory) else {
        return;
    };
    let started = Instant::now();
    let mut child = ChildOwner(
        Command::new(executable)
            .env("KU_SERVICE_OUTPUT", "bounded")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start stalled-output service"),
    );
    // Deliberately keep stdout unread. A normal bounded-process helper drains
    // pipes and therefore cannot reproduce the actual blocked log consumer.
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll output regression") {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "blocked stdout stopped shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "stalled-output process failed: {status}");
    let mut evidence = String::new();
    child
        .0
        .stderr
        .take()
        .expect("output regression stderr")
        .take(4096)
        .read_to_string(&mut evidence)
        .expect("read bounded output evidence");
    assert!(evidence.starts_with("bounded output: "), "{evidence}");
    eprintln!("{}", evidence.trim());
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn service_output_flush_preserves_unicode_nul_and_normal_program_format() {
    let directory = TempDir::new("service-output-flush");
    let Some(executable) = build(&directory) else {
        return;
    };
    for mode in ["bounded", "sync"] {
        let mut command = Command::new(&executable);
        command.arg("normal").env("KU_SERVICE_OUTPUT", mode);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .expect("bounded normal-output regression");
        assert!(output.status.success());
        // Both bounded and synchronous output retain the C stdout text mode.
        // Compare the full payload, including NUL, without stripping any CR.
        let expected: &[u8] = if cfg!(windows) {
            b"42:Ku:\xe4\xb8\xad\0\xe6\x96\x87\r\n"
        } else {
            b"42:Ku:\xe4\xb8\xad\0\xe6\x96\x87\n"
        };
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
    }
}

fn request(address: SocketAddr, method: &str, path: &str) -> String {
    let mut connection = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("connect native output service");
    connection
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("bound HTTP output response");
    connection
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("bound HTTP output request");
    write!(connection, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: fixture-output\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
        .expect("write output service request");
    let mut response = String::new();
    connection
        .take(1024 * 1024)
        .read_to_string(&mut response)
        .expect("read bounded output service response");
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    response
        .split_once("\r\n\r\n")
        .expect("HTTP body")
        .1
        .to_owned()
}

#[test]
fn native_http_continues_and_drains_when_stdout_consumer_is_stalled() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve HTTP output fixture port");
    let address = listener.local_addr().expect("fixture address");
    drop(listener);
    let directory = TempDir::new("http-output-stall");
    let payload = "x".repeat(4096);
    let generated = emit_c(
        directory.path(),
        &format!(
            r#"import http from "std.http"
fn main(): null! {{
    app = http.service()
    app.handler_timeout_ms = 2000
    app.get("/log", fn() {{
        for index in 10000 {{ println("{payload}") }}
        return http.text("alive")
    }})
    app.get("/health", fn() {{ return http.text("ready") }})
    app.listen("{address}")?
    return ok(null)
}}
"#
        ),
    );
    let source = directory.path().join("http-output.c");
    fs::write(&source, generated).expect("write native HTTP output fixture");
    let Some(executable) = compile_harness(directory.path(), &source, "http-output") else {
        return;
    };
    let mut child = ChildOwner(
        Command::new(executable)
            .env("KU_EVENT_LOOPS", "2")
            .env("KU_BLOCKING_THREADS", "2")
            .env("KU_SERVICE_CONTROL_TOKEN", "fixture-output")
            .env("KU_SERVICE_OUTPUT", "bounded")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start native HTTP output fixture"),
    );
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            break;
        }
        assert!(child.0.try_wait().expect("poll service startup").is_none());
        assert!(
            Instant::now() < ready_deadline,
            "HTTP output fixture did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // stdout is never drained: the service's logger must remain the only
    // blocked thread, while handler and event loops continue normally.
    assert_eq!(request(address, "GET", "/log"), "alive");
    assert_eq!(request(address, "GET", "/health"), "ready");
    let metrics = request(address, "GET", "/__ku/metrics");
    let dropped = metrics
        .split_once("\"dropped_records\":")
        .expect("bounded output drop metric")
        .1
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse::<usize>()
        .expect("numeric output drop metric");
    assert!(dropped > 0, "{metrics}");
    eprintln!("service-output-metrics: {metrics}");
    request(address, "POST", "/__ku/drain");
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().expect("poll service output exit") {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "stalled logger prevented service exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn service_output_closed_collector_records_failure_without_terminating_service() {
    let directory = TempDir::new("service-output-closed");
    let Some(executable) = build(&directory) else {
        return;
    };
    let mut child = ChildOwner(
        Command::new(executable)
            .arg("closed")
            .env("KU_SERVICE_OUTPUT", "bounded")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start closed-output service"),
    );
    drop(child.0.stdout.take());
    child
        .0
        .stdin
        .take()
        .expect("closed-output start barrier")
        .write_all(b"g")
        .expect("release after collector closed");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().expect("poll closed-output service") {
            assert!(
                status.success(),
                "closed collector killed service: {status}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed-output service failed to exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut evidence = String::new();
    child
        .0
        .stderr
        .take()
        .expect("closed-output evidence")
        .take(4096)
        .read_to_string(&mut evidence)
        .expect("read closed-output evidence");
    let expected = if cfg!(windows) {
        "closed collector recorded failure\r\n"
    } else {
        "closed collector recorded failure\n"
    };
    assert_eq!(evidence, expected);
}
