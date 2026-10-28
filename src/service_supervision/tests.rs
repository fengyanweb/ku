use super::*;

fn accept_fixture(listener: TcpListener) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                // Accepted sockets may inherit nonblocking mode on macOS.
                stream.set_nonblocking(false).unwrap();
                return Some(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
    None
}

#[test]
fn control_token_rejects_duplicates_and_partial_matches() {
    let expected = "a".repeat(32);
    assert!(valid_control_token(
        &format!("GET /status HTTP/1.1\r\nX-Ku-Control-Token: {expected}\r\n\r\n"),
        &expected,
    ));
    assert!(!valid_control_token(
        &format!("GET /status HTTP/1.1\r\nX-Ku-Control-Token: {expected}\r\nx-ku-control-token: {expected}\r\n\r\n"),
        &expected,
    ));
    assert!(!valid_control_token(
        "GET /status HTTP/1.1\r\n\r\n",
        &expected
    ));
}

fn probe_fixture(body: &str) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_string();
    let worker = thread::spawn(move || {
        let mut stream = accept_fixture(listener).expect("probe fixture was not contacted");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        read_headers(&mut stream, MAX_CONTROL_REQUEST).unwrap();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        );
        // The probe must complete using Content-Length, without relying on EOF.
        thread::sleep(Duration::from_millis(100));
    });
    (address, worker)
}

#[test]
fn probe_requires_this_instance_and_accepts_complete_keep_alive_body() {
    let token = "b".repeat(64);
    let (address, worker) = probe_fixture(&token);
    assert!(probe_at(
        address,
        Duration::from_secs(1),
        &token,
        "GET",
        "/__ku/health/ready"
    )
    .unwrap());
    worker.join().unwrap();
    let (address, worker) = probe_fixture("an unrelated process");
    assert!(!probe_at(
        address,
        Duration::from_secs(1),
        &token,
        "GET",
        "/__ku/health/ready"
    )
    .unwrap());
    worker.join().unwrap();
}

#[test]
fn slow_probe_has_one_absolute_response_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let mut stream = accept_fixture(listener).expect("slow probe fixture was not contacted");
        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
        for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let start = Instant::now();
    assert!(!probe_at(
        address,
        Duration::from_millis(80),
        &"c".repeat(64),
        "GET",
        "/__ku/health/live"
    )
    .unwrap_or(false));
    assert!(start.elapsed() < Duration::from_millis(400));
    worker.join().unwrap();
}

#[test]
fn tokens_are_bounded_and_per_generation() {
    let first = fresh_token().unwrap();
    let second = fresh_token().unwrap();
    assert_eq!(first.len(), 64);
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

fn native_metrics_fixture(
    body: String,
    length: usize,
    slow: bool,
) -> (SocketAddr, thread::JoinHandle<(bool, bool)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let Some(mut stream) = accept_fixture(listener) else {
            return (false, false);
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let request = read_headers(&mut stream, MAX_CONTROL_REQUEST).unwrap();
        assert!(request.starts_with("GET /__ku/metrics HTTP/1.1\r\n"));
        assert!(request.contains(&format!("X-Ku-Control-Token: {}\r\n", "d".repeat(64))));
        if write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: keep-alive\r\n\r\n"
        )
        .is_err()
        {
            return (true, false);
        }
        if slow {
            let mut body_started = false;
            for byte in body.bytes() {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                body_started = true;
                thread::sleep(Duration::from_millis(20));
            }
            (true, body_started)
        } else {
            (true, stream.write_all(body.as_bytes()).is_ok())
        }
    });
    (address, worker)
}

#[test]
fn native_metrics_has_a_separate_large_bound_without_changing_health() {
    assert_eq!(MAX_PROBE_RESPONSE, 8192);
    let body = format!("{{\"counter\":17,\"padding\":\"{}\"}}", "x".repeat(10000));
    let (address, worker) = native_metrics_fixture(body.clone(), body.len(), false);
    let actual = native_metrics_at(
        address,
        &"d".repeat(64),
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(actual, body);
    worker.join().unwrap();
}

#[test]
fn native_metrics_rejects_oversize_and_token_echo() {
    for (body, length) in [
        ("{}".to_string(), MAX_NATIVE_METRICS_BODY + 1),
        (format!("{{\"secret\":\"{}\"}}", "d".repeat(64)), 77),
    ] {
        let length = if body.len() > 2 { body.len() } else { length };
        let (address, worker) = native_metrics_fixture(body, length, false);
        assert_eq!(
            native_metrics_at(
                address,
                &"d".repeat(64),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        worker.join().unwrap();
    }
}

#[test]
fn native_metrics_body_consumes_the_original_remaining_deadline() {
    let body = "{\"value\":17}".to_string();
    // A deadline may expire before connect when a parallel test is descheduled.
    // The fixture must also retire when no client ever reaches accept.
    let (address, worker) = native_metrics_fixture(body.clone(), body.len(), true);
    assert_eq!(
        native_metrics_at(address, &"d".repeat(64), Instant::now())
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let (accepted, _) = worker.join().unwrap();
    assert!(!accepted, "expired request must not connect");
    let (address, worker) = native_metrics_fixture(body.clone(), body.len(), true);
    let began = Instant::now();
    let deadline = began + Duration::from_millis(100);
    thread::sleep(Duration::from_millis(40));
    assert!(native_metrics_at(address, &"d".repeat(64), deadline).is_err());
    assert!(began.elapsed() < Duration::from_millis(400));
    let (_, body_started) = worker.join().unwrap();
    assert!(body_started, "slow response body was not exercised");
}

#[test]
fn unavailable_metrics_is_not_a_cached_zero_or_native_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut state = exited_status();
    state.generation = 9;
    let (code, body) = metrics_response(
        listener.local_addr().unwrap(),
        None,
        &state,
        Instant::now() + Duration::from_millis(100),
    );
    assert_eq!(code, 503);
    assert!(body.contains("\"available\":false") && body.contains("\"generation\":9"));
    assert!(body.contains("\"native\":null") && body.contains("no_native_instance"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn oversized_probe_length_cannot_overflow_or_exceed_the_response_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let mut stream =
            accept_fixture(listener).expect("oversize probe fixture was not contacted");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        read_headers(&mut stream, MAX_CONTROL_REQUEST).unwrap();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
    });
    assert!(!probe_at(
        address,
        Duration::from_secs(1),
        &"d".repeat(64),
        "GET",
        "/__ku/health/ready"
    )
    .unwrap());
    worker.join().unwrap();
}

fn exited_status() -> Status {
    Status {
        phase: Phase::Stopped,
        generation: 1,
        pid: 0,
        restarts: 0,
        failures: 0,
        last_reason: "process_exit",
        started: Instant::now(),
        ready_after_ms: None,
        retired_after_ms: None,
        limit_metric: None,
        memory_peak_bytes: None,
        memory_limit_observed: None,
        memory_limit_evidence: None,
        last_exit_generation: Some(1),
        last_exit_code: Some(1),
        last_launcher_signal: None,
        launcher_status: Some("nonzero_exit"),
        termination_requested: false,
        unfinished_work_possible: true,
        database_outcome: "unknown",
    }
}

fn oom_metrics(local_limit: bool) -> platform::Metrics {
    platform::Metrics {
        limit_metric: "linux_cgroup_v2_charged_memory_bytes",
        memory_peak_bytes: Some(40 * 1024 * 1024),
        memory_current_bytes: Some(0),
        oom_kill_count: Some(1),
        memory_limit_event: Some(local_limit),
        memory_limit_termination: Some(local_limit),
        memory_limit_evidence: None,
        process_count: Some(0),
        resource_group: "test-only-provider-snapshot".into(),
    }
}

#[test]
fn host_oom_count_does_not_claim_the_configured_service_limit_caused_exit() {
    let mut state = exited_status();
    let mut metrics = oom_metrics(false);
    record_provider_metrics(&mut state, &metrics);
    assert_eq!(state.last_reason, "process_exit");
    assert_eq!(state.memory_limit_observed, Some(false));
    assert_eq!(state.memory_peak_bytes, metrics.memory_peak_bytes);
    assert_eq!(state.limit_metric, Some(metrics.limit_metric));
    assert!(state.unfinished_work_possible);
    assert_eq!(state.database_outcome, "unknown");

    // An unavailable provider event remains unknown even with a raw OOM count.
    metrics.memory_limit_event = None;
    record_provider_metrics(&mut state, &metrics);
    assert_eq!(state.memory_limit_observed, None);
    assert_eq!(state.last_reason, "process_exit");
}

#[test]
fn confirmed_leaf_memory_limit_termination_records_the_provider_reason() {
    let mut state = exited_status();
    let mut metrics = oom_metrics(true);
    metrics.memory_limit_evidence =
        Some("{\"worker\":{\"max\":1,\"oom\":1,\"oom_kill\":1}}".into());
    record_provider_metrics(&mut state, &metrics);
    assert_eq!(state.last_reason, "os_memory_limit");
    assert_eq!(state.memory_limit_observed, Some(true));
    assert_eq!(state.memory_peak_bytes, metrics.memory_peak_bytes);
    assert_eq!(state.limit_metric, Some(metrics.limit_metric));
    assert!(state.unfinished_work_possible);
    assert_eq!(state.database_outcome, "unknown");
    assert_eq!(state.memory_limit_evidence, metrics.memory_limit_evidence);
    assert!(state
        .json()
        .contains("\"memory_limit_evidence\":{\"worker\":"));
}
