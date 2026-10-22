//! Resident-process lifecycle management. A failed request is never replayed.
//!
//! This is deliberately separate from finite `ku supervise` batches. Ordinary
//! lifecycle management is available on macOS; an explicitly requested memory
//! domain always uses the existing OS provider and never silently downgrades.

mod child;

use std::{
    collections::VecDeque,
    fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::error::KuError;
use crate::process_execution::platform;
use child::{ManagedChild, OrdinaryChild};
use platform::{Domain, GroupConfig};

pub use child::run_service_child;

pub const SERVICE_USAGE: &str = "ku service --probe-addr IP:PORT --control-addr 127.0.0.1:PORT \
--control-token-file PATH [--startup-timeout-ms N] [--probe-timeout-ms N] \
[--probe-interval-ms N] [--failure-threshold N] [--drain-timeout-ms N] \
[--restart-limit N] [--restart-window-ms N] [--backoff-ms N] [--max-backoff-ms N] \
[--worker-memory-bytes N --total-memory-bytes N [--cgroup-root PATH]] \
[--watch-stdin] \
-- <native-program> [args...]\n\
Native HTTP health/drain routes must validate KU_SERVICE_CONTROL_TOKEN and echo it as the body.\n\
Control: GET /status or POST /stop with X-Ku-Control-Token from the token file.\n\
Logs from the managed program go to the null device; HTTP metrics remain available.\n\
Memory limits are optional; when requested the existing strict OS provider is mandatory.\n";

const MAX_CONTROL_REQUEST: usize = 4096;
const MAX_PROBE_RESPONSE: usize = 8192;
const MAX_OPTIONS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const CONTROL_POLL: Duration = Duration::from_millis(20);
const CLEANUP_GRACE: Duration = Duration::from_secs(5);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
struct Policy {
    probe_addr: SocketAddr,
    control_addr: SocketAddr,
    control_token: String,
    startup_timeout: Duration,
    probe_timeout: Duration,
    probe_interval: Duration,
    failure_threshold: u32,
    drain_timeout: Duration,
    restart_limit: usize,
    restart_window: Duration,
    backoff: Duration,
    max_backoff: Duration,
    memory: Option<GroupConfig>,
    program: PathBuf,
    arguments: Vec<String>,
    watch_stdin: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Starting,
    Ready,
    Unready,
    Draining,
    Backoff,
    Stopped,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Unready => "unready",
            Self::Draining => "draining",
            Self::Backoff => "backoff",
            Self::Stopped => "stopped",
        }
    }
}

struct Status {
    phase: Phase,
    generation: u64,
    pid: u32,
    restarts: u64,
    failures: u32,
    last_reason: &'static str,
    started: Instant,
    ready_after_ms: Option<u128>,
    retired_after_ms: Option<u128>,
    limit_metric: Option<&'static str>,
    memory_peak_bytes: Option<u64>,
    memory_limit_observed: Option<bool>,
    memory_limit_evidence: Option<String>,
    last_exit_generation: Option<u64>,
    last_exit_code: Option<i32>,
    last_launcher_signal: Option<i32>,
    launcher_status: Option<&'static str>,
    termination_requested: bool,
    unfinished_work_possible: bool,
    database_outcome: &'static str,
}

impl Status {
    fn json(&self) -> String {
        format!(
            "{{\"phase\":\"{}\",\"generation\":{},\"pid\":{},\"restarts\":{},\"probe_failures\":{},\"last_reason\":\"{}\",\"elapsed_ms\":{},\"ready_after_ms\":{},\"retired_after_ms\":{},\"limit_metric\":{},\"memory_peak_bytes\":{},\"memory_limit_observed\":{},\"memory_limit_evidence\":{},\"last_exit_generation\":{},\"last_exit_code\":{},\"last_launcher_signal\":{},\"launcher_status\":{},\"termination_requested\":{},\"unfinished_work_possible\":{},\"database_outcome\":\"{}\",\"database_retry\":false}}\n",
            self.phase.name(), self.generation, self.pid, self.restarts, self.failures,
            self.last_reason, self.started.elapsed().as_millis(),
            self.ready_after_ms.map_or_else(|| "null".into(), |value| value.to_string()),
            self.retired_after_ms.map_or_else(|| "null".into(), |value| value.to_string()),
            self.limit_metric.map_or_else(|| "null".into(), |value| format!("\"{value}\"")),
            self.memory_peak_bytes.map_or_else(|| "null".into(), |value| value.to_string()),
            self.memory_limit_observed.map_or_else(|| "null".into(), |value| value.to_string()),
            self.memory_limit_evidence.as_deref().unwrap_or("null"),
            self.last_exit_generation.map_or_else(|| "null".into(), |value| value.to_string()),
            self.last_exit_code.map_or_else(|| "null".into(), |value| value.to_string()),
            self.last_launcher_signal.map_or_else(|| "null".into(), |value| value.to_string()),
            self.launcher_status.map_or_else(|| "null".into(), |value| format!("\"{value}\"")),
            self.termination_requested, self.unfinished_work_possible, self.database_outcome,
        )
    }
}

pub fn run_service_command(args: &[String]) -> Result<(), KuError> {
    if args.len() == 3 && matches!(args[2].as_str(), "--help" | "-h") {
        print!("{SERVICE_USAGE}");
        return Ok(());
    }
    let policy = parse_policy(args).map_err(command_error)?;
    let listener = TcpListener::bind(policy.control_addr).map_err(command_error)?;
    listener.set_nonblocking(true).map_err(command_error)?;
    // Preparing the provider happens before running application code. In
    // particular macOS refuses requested hard limits rather than launching.
    let mut domain = policy
        .memory
        .as_ref()
        .map(Domain::prepare)
        .transpose()
        .map_err(command_error)?;
    STOP_REQUESTED.store(false, Ordering::Release);
    let _signals = child::StopSignals::install().map_err(command_error)?;
    if policy.watch_stdin {
        // Optional host-wrapper lease. Closing the anonymous pipe uses the
        // same drain/termination policy as explicit stop, never a second
        // independent native-process restart owner.
        thread::Builder::new()
            .name("ku-service-host".into())
            .stack_size(64 * 1024)
            .spawn(|| {
                let mut stdin = io::stdin();
                let mut byte = [0_u8; 1];
                loop {
                    match stdin.read(&mut byte) {
                        Ok(0) | Err(_) => {
                            STOP_REQUESTED.store(true, Ordering::Release);
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            })
            .map_err(command_error)?;
    }
    let mut state = Status {
        phase: Phase::Starting,
        generation: 0,
        pid: 0,
        restarts: 0,
        failures: 0,
        last_reason: "initial_start",
        started: Instant::now(),
        ready_after_ms: None,
        retired_after_ms: None,
        limit_metric: None,
        memory_peak_bytes: None,
        memory_limit_observed: None,
        memory_limit_evidence: None,
        last_exit_generation: None,
        last_exit_code: None,
        last_launcher_signal: None,
        launcher_status: None,
        termination_requested: false,
        unfinished_work_possible: false,
        database_outcome: "not_observed",
    };
    let mut restart_times = VecDeque::with_capacity(policy.restart_limit);
    let mut backoff = policy.backoff;
    loop {
        if service_stop(&listener, &policy, &state)? {
            return Ok(());
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| command_error("service generation exhausted"))?;
        state.phase = Phase::Starting;
        state.failures = 0;
        state.ready_after_ms = None;
        let token = fresh_token().map_err(command_error)?;
        let launch = launch(&policy, domain.as_mut(), state.generation, &token);
        let failure = match launch {
            Ok(mut process) => {
                state.pid = process.id();
                match run_instance(&listener, &policy, &mut state, &mut process, &token) {
                    Ok(InstanceEnd::Stop) => return Ok(()),
                    Ok(InstanceEnd::Failure { stable }) => {
                        if stable {
                            backoff = policy.backoff;
                        }
                        false
                    }
                    Err(error) => {
                        // ManagedChild remains an owning guard on all exits.
                        process.terminate().map_err(command_error)?;
                        process
                            .cleanup(Instant::now() + CLEANUP_GRACE)
                            .map_err(command_error)?;
                        return Err(error);
                    }
                }
            }
            Err(error) => {
                if !error.cleanup_confirmed {
                    return Err(command_error(format!(
                        "launch cleanup is unconfirmed; refusing replacement: {error}"
                    )));
                }
                state.pid = 0;
                state.last_reason = "launch_failed";
                true
            }
        };
        let now = Instant::now();
        while restart_times
            .front()
            .is_some_and(|old| now.duration_since(*old) >= policy.restart_window)
        {
            restart_times.pop_front();
        }
        if restart_times.len() >= policy.restart_limit {
            state.phase = Phase::Stopped;
            return Err(command_error(format!(
                "restart intensity exceeded: {} replacements in {} ms; last_reason={}",
                policy.restart_limit,
                policy.restart_window.as_millis(),
                state.last_reason,
            )));
        }
        restart_times.push_back(now);
        state.restarts += 1;
        state.phase = Phase::Backoff;
        if wait_control(&listener, &policy, &state, now + backoff)? {
            return Ok(());
        }
        if failure || state.last_reason != "normal_exit" {
            backoff = backoff.saturating_mul(2).min(policy.max_backoff);
        }
    }
}

enum InstanceEnd {
    Stop,
    Failure { stable: bool },
}

fn launch(
    policy: &Policy,
    domain: Option<&mut Domain>,
    generation: u64,
    token: &str,
) -> Result<ManagedChild, platform::ProviderError> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__service_child")
        .arg("--")
        .arg(&policy.program)
        .args(&policy.arguments)
        .env("KU_SERVICE_CONTROL_TOKEN", token)
        .env("KU_SERVICE_PROBE_ADDR", policy.probe_addr.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let process = if let Some(domain) = domain {
        ManagedChild::Limited(domain.spawn(&mut command, generation)?)
    } else {
        ManagedChild::Ordinary(OrdinaryChild::spawn(&mut command)?)
    };
    let mut process = process;
    if let Err(error) = process.release() {
        process.terminate()?;
        process.cleanup(Instant::now() + CLEANUP_GRACE)?;
        return Err(error.into());
    }
    Ok(process)
}

fn run_instance(
    listener: &TcpListener,
    policy: &Policy,
    state: &mut Status,
    process: &mut ManagedChild,
    token: &str,
) -> Result<InstanceEnd, KuError> {
    let started = Instant::now();
    let startup_deadline = started + policy.startup_timeout;
    let mut ready_since = None;
    let mut next_probe = Instant::now();
    let mut startup_ok = false;
    loop {
        if service_stop(listener, policy, state)? {
            state.last_reason = "requested_stop";
            drain_and_retire(listener, policy, state, process, token)?;
            return Ok(InstanceEnd::Stop);
        }
        if let Some(exit) = process.try_wait().map_err(command_error)? {
            state.last_reason = if ready_since.is_none() && !exit.success() {
                "startup_exit"
            } else if exit.success() {
                "normal_exit"
            } else {
                "process_exit"
            };
            record_exit(state, exit, false);
            process
                .cleanup(Instant::now() + CLEANUP_GRACE)
                .map_err(command_error)?;
            record_metrics(state, process)?;
            state.pid = 0;
            state.retired_after_ms = Some(state.started.elapsed().as_millis());
            return Ok(InstanceEnd::Failure {
                stable: ready_since
                    .is_some_and(|at: Instant| at.elapsed() >= policy.restart_window),
            });
        }
        if Instant::now() >= next_probe {
            if !startup_ok {
                startup_ok = probe(policy, token, "GET", "/__ku/health/startup");
            }
            if startup_ok {
                let live = probe(policy, token, "GET", "/__ku/health/live");
                let ready = live && probe(policy, token, "GET", "/__ku/health/ready");
                if live {
                    state.failures = 0;
                } else {
                    state.failures = state.failures.saturating_add(1);
                }
                if ready {
                    state.phase = Phase::Ready;
                    if ready_since.is_none() {
                        ready_since = Some(Instant::now());
                        state.ready_after_ms = Some(state.started.elapsed().as_millis());
                    }
                } else {
                    state.phase = Phase::Unready;
                }
                if state.failures >= policy.failure_threshold {
                    state.last_reason = "liveness_failed";
                    drain_and_retire(listener, policy, state, process, token)?;
                    return Ok(InstanceEnd::Failure { stable: false });
                }
            }
            next_probe = Instant::now() + policy.probe_interval;
        }
        if ready_since.is_none() && Instant::now() >= startup_deadline {
            state.last_reason = "startup_timeout";
            drain_and_retire(listener, policy, state, process, token)?;
            return Ok(InstanceEnd::Failure { stable: false });
        }
        thread::sleep(CONTROL_POLL.min(next_probe.saturating_duration_since(Instant::now())));
    }
}

fn drain_and_retire(
    listener: &TcpListener,
    policy: &Policy,
    state: &mut Status,
    process: &mut ManagedChild,
    token: &str,
) -> Result<(), KuError> {
    state.phase = Phase::Draining;
    let deadline = Instant::now() + policy.drain_timeout;
    // A failed drain is not a reason to assume the native work has stopped.
    // Keep the process and domain leased until verified cleanup below.
    let _ = probe(policy, token, "POST", "/__ku/drain");
    let mut observed_exit = None;
    while Instant::now() < deadline {
        let _ = service_stop(listener, policy, state)?;
        if let Some(exit) = process.try_wait().map_err(command_error)? {
            observed_exit = Some(exit);
            break;
        }
        thread::sleep(CONTROL_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
    if observed_exit.is_none() {
        observed_exit = process.try_wait().map_err(command_error)?;
    }
    let termination_requested = observed_exit.is_none();
    if termination_requested {
        process.terminate().map_err(command_error)?;
    }
    process
        .cleanup(Instant::now() + CLEANUP_GRACE)
        .map_err(command_error)?;
    let exit = observed_exit.or(process.reaped_status().map_err(command_error)?);
    if let Some(exit) = exit {
        record_exit(state, exit, termination_requested);
    }
    record_metrics(state, process)?;
    state.pid = 0;
    state.retired_after_ms = Some(state.started.elapsed().as_millis());
    state.phase = Phase::Stopped;
    Ok(())
}

fn record_exit(state: &mut Status, exit: std::process::ExitStatus, termination_requested: bool) {
    state.last_exit_generation = Some(state.generation);
    state.last_exit_code = exit.code();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        state.last_launcher_signal = exit.signal();
    }
    #[cfg(not(unix))]
    {
        state.last_launcher_signal = None;
    }
    let category = if termination_requested {
        "termination_requested"
    } else {
        match exit.code() {
            Some(0) => "normal_exit",
            Some(75) => "runtime_unfinished_work",
            Some(76) => "runtime_join_failure",
            Some(77) => "runtime_budget_remaining",
            Some(78) => "runtime_invariant_failure",
            Some(79) => "runtime_metrics_invariant_failure",
            Some(80) => "runtime_task_retirement_failure",
            Some(_) => "nonzero_exit",
            None => "launcher_terminated_by_signal",
        }
    };
    state.launcher_status = Some(category);
    state.termination_requested = termination_requested;
    state.unfinished_work_possible = termination_requested
        || !exit.success()
        || matches!(
            state.last_reason,
            "liveness_failed" | "startup_timeout" | "os_memory_limit"
        );
    state.database_outcome = if state.unfinished_work_possible {
        "unknown"
    } else {
        "not_observed"
    };
    if !termination_requested && matches!(exit.code(), Some(75..=80)) {
        state.last_reason = category;
    }
}

fn record_metrics(state: &mut Status, process: &ManagedChild) -> Result<(), KuError> {
    if let Some(metrics) = process.metrics().map_err(command_error)? {
        record_provider_metrics(state, &metrics);
    }
    Ok(())
}

fn record_provider_metrics(state: &mut Status, metrics: &platform::Metrics) {
    state.limit_metric = Some(metrics.limit_metric);
    state.memory_peak_bytes = metrics.memory_peak_bytes;
    state.memory_limit_observed = metrics.memory_limit_event;
    state
        .memory_limit_evidence
        .clone_from(&metrics.memory_limit_evidence);
    // A cgroup's oom_kill counter can include a victim of global host OOM.
    // Only the provider's combined evidence identifies its configured limit
    // as the reason for termination; the raw count alone cannot establish it.
    if metrics.memory_limit_termination == Some(true) {
        state.last_reason = "os_memory_limit";
        state.unfinished_work_possible = true;
        state.database_outcome = "unknown";
    }
}

fn wait_control(
    listener: &TcpListener,
    policy: &Policy,
    state: &Status,
    deadline: Instant,
) -> Result<bool, KuError> {
    while Instant::now() < deadline {
        if service_stop(listener, policy, state)? {
            return Ok(true);
        }
        thread::sleep(CONTROL_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
    Ok(false)
}

fn service_stop(listener: &TcpListener, policy: &Policy, state: &Status) -> Result<bool, KuError> {
    if STOP_REQUESTED.load(Ordering::Acquire) {
        return Ok(true);
    }
    // One bounded control request per turn. A noisy local client cannot create
    // a reader thread per connection or starve process/probe progression.
    match listener.accept() {
        Ok((mut stream, address)) => {
            if !address.ip().is_loopback() {
                return Ok(false);
            }
            let response_timeout = policy.probe_timeout.min(Duration::from_millis(200));
            // Accepted sockets can inherit the listener's nonblocking mode.
            // A peer reset can also make timeout setup fail (Darwin EINVAL).
            // Both setup and failure belong to this one bounded connection,
            // not to the coordinator or its healthy managed service.
            if stream
                .set_nonblocking(false)
                .and_then(|()| stream.set_read_timeout(Some(response_timeout)))
                .and_then(|()| stream.set_write_timeout(Some(response_timeout)))
                .is_err()
            {
                return Ok(false);
            }
            let request = read_headers(&mut stream, MAX_CONTROL_REQUEST);
            let (code, body, stop) = match request {
                Ok(request) if valid_control_token(&request, &policy.control_token) => {
                    match request.lines().next() {
                        Some("GET /status HTTP/1.1") => (200, state.json(), false),
                        Some("POST /stop HTTP/1.1") => (202, "stopping\n".into(), true),
                        _ => (404, "unknown control route\n".into(), false),
                    }
                }
                Ok(_) => (403, "control token required\n".into(), false),
                Err(error) => (
                    400,
                    format!("invalid bounded control request: {error}\n"),
                    false,
                ),
            };
            let response = format!("HTTP/1.1 {code} Control\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(response.as_bytes());
            Ok(stop)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(command_error(error)),
    }
}

fn valid_control_token(request: &str, expected: &str) -> bool {
    let mut found = false;
    for line in request.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("x-ku-control-token") {
                if found || value.trim() != expected {
                    return false;
                }
                found = true;
            }
        }
    }
    found
}

fn read_headers(stream: &mut TcpStream, limit: usize) -> io::Result<String> {
    let deadline = Instant::now() + stream.read_timeout()?.unwrap_or(Duration::from_millis(200));
    let mut bytes = Vec::with_capacity(limit.min(1024));
    let mut chunk = [0_u8; 512];
    while bytes.len() < limit {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "control header deadline",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(&mut chunk[..512.min(limit - bytes.len())])?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 HTTP headers"));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "HTTP header limit or early EOF",
    ))
}

fn probe(policy: &Policy, token: &str, method: &str, path: &str) -> bool {
    probe_at(policy.probe_addr, policy.probe_timeout, token, method, path).unwrap_or(false)
}

fn probe_at(
    address: SocketAddr,
    timeout: Duration,
    token: &str,
    method: &str,
    path: &str,
) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_write_timeout(Some(
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1)),
    ))?;
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {}\r\nX-Ku-Control-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", address)?;
    let mut response = Vec::with_capacity(256);
    let mut chunk = [0_u8; 512];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || response.len() >= MAX_PROBE_RESPONSE {
            return Ok(false);
        }
        stream.set_read_timeout(Some(remaining))?;
        let limit = chunk.len().min(MAX_PROBE_RESPONSE - response.len());
        let count = stream.read(&mut chunk[..limit])?;
        if count == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..count]);
        if let Some(offset) = response.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&response[..offset]);
            let length = headers.lines().skip(1).find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            if let Some(length) = length {
                let Some(end) = (offset + 4)
                    .checked_add(length)
                    .filter(|end| *end <= MAX_PROBE_RESPONSE)
                else {
                    return Ok(false);
                };
                if response.len() >= end {
                    break;
                }
            }
        }
    }
    let Some(offset) = response.windows(4).position(|part| part == b"\r\n\r\n") else {
        return Ok(false);
    };
    let first_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    Ok(
        (first_line.starts_with(b"HTTP/1.1 200 ") || first_line.starts_with(b"HTTP/1.0 200 "))
            && response[offset + 4..] == *token.as_bytes(),
    )
}

fn fresh_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_policy(args: &[String]) -> Result<Policy, String> {
    let separator = args
        .iter()
        .position(|argument| argument == "--")
        .ok_or("service requires '--' followed by a native executable")?;
    if !(2..=MAX_OPTIONS).contains(&separator) || args.len() <= separator + 1 {
        return Err("invalid service arguments".into());
    }
    if args.iter().map(String::len).sum::<usize>() > MAX_ARGUMENT_BYTES {
        return Err("service arguments exceed 64 KiB".into());
    }
    let mut values = std::collections::BTreeMap::new();
    let mut index = 2;
    while index < separator {
        let option = &args[index];
        if option == "--watch-stdin" {
            if values.insert(option.as_str(), "true").is_some() {
                return Err(format!("duplicate service option {option}"));
            }
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .filter(|_| index + 1 < separator)
            .ok_or_else(|| format!("missing value for {option}"))?;
        if values.insert(option.as_str(), value.as_str()).is_some() {
            return Err(format!("duplicate service option {option}"));
        }
        index += 2;
    }
    let allowed = [
        "--probe-addr",
        "--control-addr",
        "--control-token-file",
        "--startup-timeout-ms",
        "--probe-timeout-ms",
        "--probe-interval-ms",
        "--failure-threshold",
        "--drain-timeout-ms",
        "--restart-limit",
        "--restart-window-ms",
        "--backoff-ms",
        "--max-backoff-ms",
        "--worker-memory-bytes",
        "--total-memory-bytes",
        "--cgroup-root",
        "--watch-stdin",
    ];
    for option in values.keys() {
        if !allowed.contains(option) {
            return Err(format!("unknown service option {option}"));
        }
    }
    let required = |name: &str| {
        values
            .get(name)
            .copied()
            .ok_or_else(|| format!("{name} is required"))
    };
    let probe_addr: SocketAddr = required("--probe-addr")?
        .parse()
        .map_err(|_| "invalid probe IP:port")?;
    let control_addr: SocketAddr = required("--control-addr")?
        .parse()
        .map_err(|_| "invalid control IP:port")?;
    if !probe_addr.ip().is_loopback()
        || !control_addr.ip().is_loopback()
        || probe_addr.port() == 0
        || control_addr.port() == 0
        || probe_addr == control_addr
    {
        return Err(
            "probe and control addresses must be distinct nonzero loopback IP:ports".into(),
        );
    }
    let token_path = PathBuf::from(required("--control-token-file")?);
    let metadata =
        fs::metadata(&token_path).map_err(|error| format!("control token file: {error}"))?;
    if !metadata.is_file() || metadata.len() > 256 {
        return Err("control token file must be a regular file at most 256 bytes".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(
                "control token file must not be accessible by group/other (use chmod 600)".into(),
            );
        }
    }
    let control_token = fs::read_to_string(&token_path)
        .map_err(|error| error.to_string())?
        .trim()
        .to_string();
    if !(32..=128).contains(&control_token.len())
        || !control_token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("control token must contain 32..=128 ASCII alphanumeric characters".into());
    }
    let number = |name: &str, default: u64, max: u64| -> Result<u64, String> {
        let value = values.get(name).map_or(Ok(default), |value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("invalid integer for {name}"))
        })?;
        if value == 0 || value > max {
            return Err(format!("{name} must be in 1..={max}"));
        }
        Ok(value)
    };
    let ms = |name, default| number(name, default, 24 * 60 * 60 * 1000).map(Duration::from_millis);
    let memory = if values.contains_key("--worker-memory-bytes")
        || values.contains_key("--total-memory-bytes")
        || values.contains_key("--cgroup-root")
    {
        if !values.contains_key("--worker-memory-bytes")
            || !values.contains_key("--total-memory-bytes")
        {
            return Err(
                "explicit memory mode requires both --worker-memory-bytes and --total-memory-bytes"
                    .into(),
            );
        }
        Some(GroupConfig {
            worker_memory_bytes: number("--worker-memory-bytes", 0, 1 << 40)?,
            total_worker_memory_bytes: number("--total-memory-bytes", 0, 16 << 40)?,
            max_workers: 1,
            cgroup_root: values.get("--cgroup-root").map(PathBuf::from),
        })
    } else {
        None
    };
    let program = fs::canonicalize(&args[separator + 1])
        .map_err(|error| format!("native executable: {error}"))?;
    if !program.is_file() {
        return Err("native executable is not a regular file".into());
    }
    let policy = Policy {
        probe_addr,
        control_addr,
        control_token,
        startup_timeout: ms("--startup-timeout-ms", 5000)?,
        probe_timeout: ms("--probe-timeout-ms", 250)?,
        probe_interval: ms("--probe-interval-ms", 1000)?,
        failure_threshold: number("--failure-threshold", 3, 100)? as u32,
        drain_timeout: ms("--drain-timeout-ms", 10000)?,
        restart_limit: number("--restart-limit", 5, 1000)? as usize,
        restart_window: ms("--restart-window-ms", 60000)?,
        backoff: ms("--backoff-ms", 100)?,
        max_backoff: ms("--max-backoff-ms", 5000)?,
        memory,
        program,
        arguments: args[separator + 2..].to_vec(),
        watch_stdin: values.contains_key("--watch-stdin"),
    };
    if policy.backoff > policy.max_backoff || policy.probe_timeout > policy.startup_timeout {
        return Err(
            "backoff must not exceed max_backoff; probe_timeout must not exceed startup_timeout"
                .into(),
        );
    }
    Ok(policy)
}

fn command_error(message: impl std::fmt::Display) -> KuError {
    KuError::message(format!("ku service: {message}"))
}

#[cfg(test)]
mod tests;
