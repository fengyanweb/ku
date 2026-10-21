use super::*;

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
        let (mut stream, _) = listener.accept().unwrap();
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
        let (mut stream, _) = listener.accept().unwrap();
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

#[test]
fn oversized_probe_length_cannot_overflow_or_exceed_the_response_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
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
    let metrics = oom_metrics(true);
    record_provider_metrics(&mut state, &metrics);
    assert_eq!(state.last_reason, "os_memory_limit");
    assert_eq!(state.memory_limit_observed, Some(true));
    assert_eq!(state.memory_peak_bytes, metrics.memory_peak_bytes);
    assert_eq!(state.limit_metric, Some(metrics.limit_metric));
    assert!(state.unfinished_work_possible);
    assert_eq!(state.database_outcome, "unknown");
}
