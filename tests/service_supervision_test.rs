//! Real CLI/process/probe contracts. The fixture is isolated to this test
//! executable; production Ku has no crash or hang injection setting.

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Preparation(Option<PathBuf>);

impl Drop for Preparation {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

struct Fixture {
    supervisor: Child,
    control: SocketAddr,
    service: SocketAddr,
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = request(self.control, "POST", "/stop", Some(TOKEN));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.supervisor.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn unused_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn start(mode: &str, extra: &[&str]) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "ku-resident-regression-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let mut preparation = Preparation(Some(root.clone()));
    let token_file = root.join("token");
    fs::write(&token_file, TOKEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let service = unused_address();
    let mut control = unused_address();
    while control == service {
        control = unused_address();
    }
    let supervisor = Command::new(env!("CARGO_BIN_EXE_ku"))
        .args([
            "service",
            "--probe-addr",
            &service.to_string(),
            "--control-addr",
            &control.to_string(),
            "--control-token-file",
        ])
        .arg(token_file)
        .args([
            "--startup-timeout-ms",
            "2000",
            "--probe-timeout-ms",
            "100",
            "--probe-interval-ms",
            "30",
            "--failure-threshold",
            "2",
            "--drain-timeout-ms",
            "150",
            "--restart-window-ms",
            "5000",
            "--backoff-ms",
            "30",
            "--max-backoff-ms",
            "100",
        ])
        .args(extra)
        .arg("--")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "resident_process_fixture", "--nocapture"])
        .env("KU_SERVICE_TEST_ADDR", service.to_string())
        .env("KU_SERVICE_TEST_MODE", mode)
        .env("KU_SERVICE_TEST_STATE", root.join("launch-state"))
        .stdin(if extra.contains(&"--watch-stdin") {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    Fixture {
        supervisor,
        control,
        service,
        root: preparation.0.take().unwrap(),
    }
}

fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> std::io::Result<String> {
    request_bounded(address, method, path, token, 8192)
}

fn request_bounded(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    response_limit: u64,
) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(100))?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    let header = token.map_or_else(String::new, |value| {
        format!("X-Ku-Control-Token: {value}\r\n")
    });
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{header}Content-Length: 0\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.take(response_limit).read_to_string(&mut response)?;
    Ok(response)
}

#[test]
fn resident_metrics_is_authorized_fresh_and_generation_bound() {
    let mut fixture = start("metrics", &[]);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"")
    });
    let first = request_bounded(fixture.control, "GET", "/metrics", Some(TOKEN), 65536).unwrap();
    assert!(
        first.starts_with("HTTP/1.1 200 ") && first.len() > 8192,
        "metrics bridge did not preserve bounded large native data"
    );
    assert!(first.contains("\"available\":true") && first.contains("\"generation\":1"));
    assert!(first.contains("\"launch\":1") && first.contains("\"calls\":1"));
    assert!(!first.contains(TOKEN));
    assert!(request(fixture.control, "GET", "/metrics", Some("invalid"))
        .unwrap()
        .starts_with("HTTP/1.1 403"));
    let next = request_bounded(fixture.control, "GET", "/metrics", Some(TOKEN), 65536).unwrap();
    assert!(
        next.contains("\"calls\":2"),
        "a rejected operator request reached native metrics or data was cached"
    );
    let _ = request(fixture.service, "GET", "/crash", None);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":2")
    });
    let restarted =
        request_bounded(fixture.control, "GET", "/metrics", Some(TOKEN), 65536).unwrap();
    assert!(
        restarted.contains("\"generation\":2")
            && restarted.contains("\"launch\":2")
            && restarted.contains("\"calls\":1")
    );
    assert!(request(fixture.control, "POST", "/stop", Some(TOKEN))
        .unwrap()
        .starts_with("HTTP/1.1 202"));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(exit) = fixture.supervisor.try_wait().unwrap() {
            assert!(exit.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "metrics bridge prevented normal stop"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_status(fixture: &mut Fixture, predicate: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(response) = request(fixture.control, "GET", "/status", Some(TOKEN)) {
            last = response;
            if predicate(&last) {
                return last;
            }
        }
        assert!(
            fixture.supervisor.try_wait().unwrap().is_none(),
            "supervisor exited; last={last}"
        );
        thread::sleep(Duration::from_millis(15));
    }
    panic!("status deadline; last={last}");
}

#[test]
fn resident_restart_and_requested_drain_have_one_owner() {
    let mut fixture = start("normal", &[]);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":1")
    });
    assert!(request(fixture.control, "GET", "/status", Some("invalid"))
        .unwrap()
        .starts_with("HTTP/1.1 403"));
    assert!(request(fixture.service, "GET", "/value", None)
        .unwrap()
        .ends_with("value=17"));
    let _ = request(fixture.service, "GET", "/crash", None);
    let status = wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":2")
    });
    assert!(status.contains("\"restarts\":1"), "{status}");
    assert!(status.contains("\"last_exit_code\":74"), "{status}");
    assert!(
        status.contains("\"launcher_status\":\"nonzero_exit\""),
        "{status}"
    );
    assert!(
        status.contains("\"database_outcome\":\"unknown\""),
        "{status}"
    );
    assert!(request(fixture.service, "GET", "/value", None)
        .unwrap()
        .ends_with("value=17"));
    assert!(request(fixture.control, "POST", "/stop", Some(TOKEN))
        .unwrap()
        .starts_with("HTTP/1.1 202"));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = fixture.supervisor.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "requested stop did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(TcpStream::connect_timeout(&fixture.service, Duration::from_millis(100)).is_err());
}

#[test]
fn repeated_startup_failure_is_bounded_and_leaves_no_listener() {
    let mut fixture = start("exit", &["--restart-limit", "2"]);
    let started = Instant::now();
    loop {
        if let Some(status) = fixture.supervisor.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        thread::sleep(Duration::from_millis(10));
    }
    assert!(started.elapsed() >= Duration::from_millis(70));
    assert!(TcpStream::connect_timeout(&fixture.service, Duration::from_millis(100)).is_err());
}

#[test]
fn reset_control_peers_leave_the_same_coordinator_and_generation_running() {
    let mut fixture = start("normal", &[]);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":1")
    });
    // Queue a reset before the bounded control loop accepts the connection.
    // Darwin can accept that fd, then reject its timeout setsockopt with EINVAL.
    for _ in 0..4 {
        let stream = TcpStream::connect_timeout(&fixture.control, Duration::from_millis(100))
            .expect("connect isolated control peer");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_LINGER,
                        (&linger as *const libc::linger).cast(),
                        std::mem::size_of_val(&linger) as libc::socklen_t,
                    )
                },
                0,
                "configure reset peer: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::Networking::WinSock::{
                setsockopt, WSAGetLastError, LINGER, SOL_SOCKET, SO_LINGER,
            };
            // Winsock uses two u16 fields, unlike Unix's two-int linger.
            let linger = LINGER {
                l_onoff: 1,
                l_linger: 0,
            };
            assert_eq!(
                unsafe {
                    setsockopt(
                        stream.as_raw_socket() as _,
                        SOL_SOCKET,
                        SO_LINGER,
                        (&linger as *const LINGER).cast(),
                        std::mem::size_of_val(&linger) as i32,
                    )
                },
                0,
                "configure reset peer WSA error: {}",
                unsafe { WSAGetLastError() }
            );
        }
        drop(stream);
    }
    let status = wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":1")
    });
    assert!(status.contains("\"restarts\":0"), "{status}");
    assert!(fixture.supervisor.try_wait().unwrap().is_none());
    assert!(request(fixture.service, "GET", "/value", None)
        .unwrap()
        .ends_with("value=17"));
    assert!(request(fixture.control, "POST", "/stop", Some(TOKEN))
        .unwrap()
        .starts_with("HTTP/1.1 202"));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = fixture.supervisor.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "requested stop did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(TcpStream::connect_timeout(&fixture.service, Duration::from_millis(100)).is_err());
}

#[cfg(unix)]
#[test]
fn supervisor_loss_retires_the_normal_native_service() {
    let mut fixture = start("normal", &[]);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"")
    });
    fixture.supervisor.kill().unwrap();
    fixture.supervisor.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while TcpStream::connect_timeout(&fixture.service, Duration::from_millis(50)).is_ok() {
        assert!(
            Instant::now() < deadline,
            "orphan resident service listener"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn liveness_failure_retires_one_instance_then_replacement_can_serve() {
    let mut fixture = start("first_liveness_failure", &[]);
    let status = wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":2")
    });
    assert!(status.contains("\"last_reason\":\"liveness_failed\""));
    assert!(request(fixture.service, "GET", "/value", None)
        .unwrap()
        .ends_with("value=17"));
}

#[test]
fn a_non_exiting_drain_is_terminated_at_the_fixed_process_boundary() {
    let mut fixture = start("ignore_drain", &[]);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"")
    });
    let started = Instant::now();
    request(fixture.control, "POST", "/stop", Some(TOKEN)).unwrap();
    loop {
        if let Some(status) = fixture.supervisor.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }
    assert!(started.elapsed() >= Duration::from_millis(140));
    assert!(TcpStream::connect_timeout(&fixture.service, Duration::from_millis(100)).is_err());
}

#[test]
fn host_wrapper_pipe_loss_drains_the_supervisor_and_target() {
    let mut fixture = start("normal", &["--watch-stdin"]);
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"")
    });
    fixture.supervisor.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = fixture.supervisor.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor outlived its host wrapper lease"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(TcpStream::connect_timeout(&fixture.service, Duration::from_millis(100)).is_err());
}

#[test]
fn unfinished_runtime_work_exit_is_reported_without_claiming_rollback() {
    let mut fixture = start("exit75", &["--restart-limit", "1"]);
    let status = wait_status(&mut fixture, |status| {
        status.contains("\"last_exit_code\":75")
    });
    assert!(
        status.contains("\"launcher_status\":\"runtime_unfinished_work\""),
        "{status}"
    );
    assert!(
        status.contains("\"last_reason\":\"runtime_unfinished_work\""),
        "{status}"
    );
    assert!(
        status.contains("\"unfinished_work_possible\":true"),
        "{status}"
    );
    assert!(
        status.contains("\"database_outcome\":\"unknown\""),
        "{status}"
    );
    assert!(status.contains("\"database_retry\":false"), "{status}");
}

#[test]
fn forced_retirement_records_the_action_and_observed_launcher_status() {
    let mut fixture = start("first_liveness_failure_ignore_drain", &[]);
    let status = wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"") && status.contains("\"generation\":2")
    });
    assert!(
        status.contains("\"launcher_status\":\"termination_requested\""),
        "{status}"
    );
    assert!(
        status.contains("\"termination_requested\":true"),
        "{status}"
    );
    assert!(
        status.contains("\"unfinished_work_possible\":true"),
        "{status}"
    );
    assert!(
        status.contains("\"database_outcome\":\"unknown\""),
        "{status}"
    );
}

#[test]
fn full_native_business_slots_do_not_starve_reserved_probes_or_restart() {
    let root = std::env::temp_dir().join(format!(
        "ku-resident-native-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let mut preparation = Preparation(Some(root.clone()));
    fs::write(
        root.join("service.ku"),
        include_str!("../deploy/service/http_service.ku"),
    )
    .unwrap();
    let executable = if cfg!(windows) {
        "service.exe"
    } else {
        "service"
    };
    let mut build = Command::new(env!("CARGO_BIN_EXE_ku"))
        .args(["build", "--native", "service.ku", "-o", executable])
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = build.try_wait().unwrap() {
            assert!(status.success(), "native service build: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = build.kill();
            let _ = build.wait();
            let _ = fs::remove_dir_all(&root);
            panic!("native build exceeded fixed 120s budget");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let service = unused_address();
    let probe = unused_address();
    let control = unused_address();
    assert!(service != probe && service != control && probe != control);
    fs::write(root.join("http.addr"), service.to_string()).unwrap();
    let token_file = root.join("token");
    fs::write(&token_file, TOKEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let supervisor = Command::new(env!("CARGO_BIN_EXE_ku"))
        .args([
            "service",
            "--probe-addr",
            &probe.to_string(),
            "--control-addr",
            &control.to_string(),
            "--control-token-file",
        ])
        .arg(token_file)
        .args([
            "--startup-timeout-ms",
            "5000",
            "--probe-timeout-ms",
            "100",
            "--probe-interval-ms",
            "100",
            "--failure-threshold",
            "3",
            "--drain-timeout-ms",
            "200",
            "--",
        ])
        .arg(root.join(executable))
        .env("KU_EVENT_LOOPS", "4")
        .env("KU_BLOCKING_THREADS", "4")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut fixture = Fixture {
        supervisor,
        control,
        service,
        root: preparation.0.take().unwrap(),
    };
    wait_status(&mut fixture, |status| {
        status.contains("\"phase\":\"ready\"")
    });
    let mut partial_requests = Vec::with_capacity(64);
    for _ in 0..64 {
        let mut stream =
            TcpStream::connect_timeout(&fixture.service, Duration::from_secs(1)).unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Hold:")
            .unwrap();
        partial_requests.push(stream);
    }
    let deadline = Instant::now() + Duration::from_millis(1400);
    while Instant::now() < deadline {
        let state = request(fixture.control, "GET", "/status", Some(TOKEN)).unwrap();
        assert!(
            state.contains("\"generation\":1,"),
            "capacity pressure restarted native service: {state}"
        );
        assert!(
            state.contains("\"phase\":\"ready\""),
            "reserved probe stopped progressing: {state}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    drop(partial_requests);
    assert!(request(fixture.service, "GET", "/", None)
        .unwrap()
        .ends_with("Ku HTTP OK"));
}

#[test]
fn resident_process_fixture() {
    let Ok(address) = std::env::var("KU_SERVICE_TEST_ADDR") else {
        return;
    };
    let mode = std::env::var("KU_SERVICE_TEST_MODE").unwrap();
    if mode == "exit" {
        std::process::exit(73);
    }
    if mode == "exit75" {
        std::process::exit(75);
    }
    let token = std::env::var("KU_SERVICE_CONTROL_TOKEN").unwrap();
    let metrics_launch = if mode == "metrics" {
        let path = PathBuf::from(std::env::var("KU_SERVICE_TEST_STATE").unwrap());
        let number = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        fs::write(path, number.to_string()).unwrap();
        number
    } else {
        0
    };
    let mut metrics_calls = 0_u64;
    let first_liveness_failure = if mode.starts_with("first_liveness_failure") {
        let path = PathBuf::from(std::env::var("KU_SERVICE_TEST_STATE").unwrap());
        let first = !path.exists();
        fs::write(path, "started").unwrap();
        first
    } else {
        false
    };
    let listener = TcpListener::bind(address).unwrap();
    for connection in listener.incoming() {
        let mut stream = connection.unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut request = Vec::with_capacity(4096);
        let mut chunk = [0_u8; 512];
        while request.len() < 4096 {
            let count = stream.read(&mut chunk).unwrap_or(0);
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        let path = request.split_whitespace().nth(1).unwrap_or("");
        if path == "/crash" {
            std::process::exit(74);
        }
        let authenticated = request.contains(&format!("X-Ku-Control-Token: {token}\r\n"));
        let internal = path.starts_with("/__ku/");
        let code = if internal && !authenticated {
            403
        } else if path == "/__ku/health/live" && first_liveness_failure {
            503
        } else {
            200
        };
        let metrics_body = if mode == "metrics" && path == "/__ku/metrics" && authenticated {
            metrics_calls += 1;
            Some(format!(
                "{{\"launch\":{metrics_launch},\"calls\":{metrics_calls},\"padding\":\"{}\"}}",
                "x".repeat(9000)
            ))
        } else {
            None
        };
        let body = if let Some(body) = metrics_body.as_deref() {
            body
        } else if internal && authenticated {
            token.as_str()
        } else if internal {
            "forbidden"
        } else {
            "value=17"
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {code} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        if path == "/__ku/drain" && authenticated && !mode.contains("ignore_drain") {
            std::process::exit(0);
        }
    }
}
