#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::service::{
    spawn_bounded_service, spawn_bounded_service_with_fault, BoundedService, ServiceFault,
};
use bounded_process::{run_bounded, FailureKind, OutputLimits};
use std::io::{self, Write};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHILD_MODE: &str = "KU_TEST_BOUNDED_PROCESS_CHILD_MODE";
const FIXTURE_NAME: &str = "bounded_process_fixture_child";
const SERVICE_READY: &[u8] = b"bounded-service-ready";
const SERVICE_TAIL_READY: &[u8] = b"bounded-service-tail-ready";
const SERVICE_TAIL_DIAGNOSTIC: &str = "bounded-service-late-stderr";

fn fixture_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", FIXTURE_NAME, "--nocapture"])
        .env(CHILD_MODE, mode);
    command
}

fn missing_service_executable() -> std::path::PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must be after the Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "ku-bounded-service-missing-{}-{timestamp}.exe",
        std::process::id()
    ));
    assert!(
        !path.exists(),
        "missing-executable fixture unexpectedly exists"
    );
    path
}

fn flush_service_marker(marker: &[u8]) {
    let mut stdout = io::stdout().lock();
    stdout.write_all(marker).expect("write service marker");
    stdout.write_all(b"\n").expect("write marker newline");
    stdout.flush().expect("flush service marker");
}

fn wait_for_service_marker(service: &BoundedService, marker: &[u8]) {
    let started = Instant::now();
    loop {
        let (stdout, stderr) = service.output_snapshot();
        if stdout.windows(marker.len()).any(|bytes| bytes == marker) {
            return;
        }
        assert!(
            !service.is_finished(),
            "service ended before its marker; stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "service did not produce its marker; stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        // Poll a flushed marker, not an assumed startup delay.
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_service_completion(service: &BoundedService) {
    let started = Instant::now();
    while !service.is_finished() {
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "service supervisor did not finish within the bounded fixture wait"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[allow(clippy::zombie_processes)] // The outer helper deliberately owns and kills this process tree.
fn bounded_process_fixture_child() {
    match std::env::var(CHILD_MODE).as_deref() {
        Ok("normal") => print!("bounded-normal-output"),
        Ok("nonzero") => std::process::exit(23),
        Ok("large") => {
            let block = [b'x'; 8 * 1024];
            let mut stdout = io::stdout().lock();
            for _ in 0..1024 {
                if stdout.write_all(&block).is_err() {
                    break;
                }
            }
            let _ = stdout.flush();
        }
        Ok("timeout") => std::thread::sleep(Duration::from_secs(10)),
        Ok("service_alive") => {
            flush_service_marker(SERVICE_READY);
            std::thread::sleep(Duration::from_secs(10));
        }
        Ok("service_exit_zero") => {
            flush_service_marker(SERVICE_READY);
            std::process::exit(0);
        }
        Ok("service_exit_nonzero") => {
            flush_service_marker(SERVICE_READY);
            std::process::exit(23);
        }
        Ok("service_tail_stderr") => {
            flush_service_marker(SERVICE_READY);
            {
                let mut stderr = io::stderr().lock();
                writeln!(stderr, "{SERVICE_TAIL_DIAGNOSTIC}")
                    .expect("write diagnostic after ready");
                stderr.flush().expect("flush diagnostic after ready");
            }
            // Observing this marker proves that the child wrote its diagnostic
            // before the parent asks the supervisor to stop it.
            flush_service_marker(SERVICE_TAIL_READY);
            std::thread::sleep(Duration::from_secs(10));
        }
        Ok("service_stderr_flood") => {
            flush_service_marker(SERVICE_READY);
            let block = [b'e'; 8 * 1024];
            let mut stderr = io::stderr().lock();
            for _ in 0..1024 {
                if stderr.write_all(&block).is_err() {
                    break;
                }
            }
            let _ = stderr.flush();
        }
        Ok("descendant_parent") => {
            // Give the outer helper time to place this process in its dedicated
            // process group/Job before the descendant inherits it.
            std::thread::sleep(Duration::from_millis(100));
            Command::new(std::env::current_exe().expect("current test executable"))
                .args(["--exact", FIXTURE_NAME, "--nocapture"])
                .env(CHILD_MODE, "descendant_wait")
                .spawn()
                .expect("spawn inherited-pipe descendant");
        }
        Ok("descendant_wait") => std::thread::sleep(Duration::from_secs(10)),
        Ok(other) => panic!("unknown bounded-process fixture mode: {other}"),
        Err(_) => {}
    }
}

#[test]
fn bounded_process_captures_normal_output() {
    let output = run_bounded(
        &mut fixture_command("normal"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("normal fixture must complete");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("bounded-normal-output"));
}

#[test]
fn bounded_process_preserves_nonzero_exit() {
    let output = run_bounded(
        &mut fixture_command("nonzero"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("nonzero exit is still a completed process");
    assert_eq!(output.status.code(), Some(23));
}

#[test]
fn bounded_process_stops_output_flood_at_fixed_limit() {
    let limits = OutputLimits::new(16 * 1024, 24 * 1024);
    let error = run_bounded(
        &mut fixture_command("large"),
        Duration::from_secs(5),
        limits,
    )
    .expect_err("large output must be rejected");
    assert_eq!(error.kind(), FailureKind::OutputLimit);
    assert!(error.stdout().len() <= limits.per_stream);
    assert!(error.stdout().len() + error.stderr().len() <= limits.total);
    let rendered = error.to_string();
    assert!(rendered.contains(FIXTURE_NAME));
    assert!(rendered.contains("truncated"));
}

#[test]
fn bounded_process_timeout_is_absolute_and_short() {
    let started = Instant::now();
    let error = run_bounded(
        &mut fixture_command("timeout"),
        Duration::from_millis(200),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect_err("sleeping child must time out");
    assert_eq!(error.kind(), FailureKind::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "timeout cleanup took too long: {:?}",
        started.elapsed()
    );
    assert!(error.to_string().contains(FIXTURE_NAME));
}

#[test]
fn bounded_process_reaps_descendant_pipe_holders_after_parent_exit() {
    let started = Instant::now();
    let output = run_bounded(
        &mut fixture_command("descendant_parent"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("a completed parent must not wait for an inherited descendant pipe");
    assert!(output.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "descendant cleanup took too long: {:?}",
        started.elapsed()
    );
}

#[test]
fn bounded_service_checked_stop_is_explicitly_forced_and_retains_output() {
    let service = spawn_bounded_service(
        fixture_command("service_alive"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("start bounded service supervisor");
    wait_for_service_marker(&service, SERVICE_READY);
    let stopped = service
        .finish_checked()
        .expect("a live quiet service permits an explicit forced stop");
    assert!(stopped.output.stderr.is_empty());
    assert!(stopped
        .output
        .stdout
        .windows(SERVICE_READY.len())
        .any(|bytes| bytes == SERVICE_READY));
    assert!(
        !stopped.output.status.success(),
        "forced termination is not a receipt for natural successful exit"
    );
}

#[test]
fn bounded_service_rejects_premature_zero_exit() {
    let service = spawn_bounded_service(
        fixture_command("service_exit_zero"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("start premature-zero-exit fixture");
    wait_for_service_completion(&service);
    let error = service
        .finish_checked()
        .expect_err("an early successful child exit is not a running service");
    assert_eq!(error.kind(), FailureKind::UnexpectedExit);
    assert!(String::from_utf8_lossy(error.stdout()).contains("bounded-service-ready"));
    assert!(error.to_string().contains("code=Some(0)"));
}

#[test]
fn bounded_service_rejects_premature_nonzero_exit_and_retains_status() {
    let service = spawn_bounded_service(
        fixture_command("service_exit_nonzero"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("start premature-nonzero-exit fixture");
    wait_for_service_completion(&service);
    let error = service
        .finish_checked()
        .expect_err("a failed service cannot become a checked-stop success");
    assert_eq!(error.kind(), FailureKind::UnexpectedExit);
    assert!(String::from_utf8_lossy(error.stdout()).contains("bounded-service-ready"));
    assert!(error.to_string().contains("code=Some(23)"));
}

#[test]
fn bounded_service_rejects_stderr_written_after_ready_before_stop() {
    let service = spawn_bounded_service(
        fixture_command("service_tail_stderr"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("start late-diagnostic fixture");
    wait_for_service_marker(&service, SERVICE_TAIL_READY);
    let error = service
        .finish_checked()
        .expect_err("business readiness must not hide a later diagnostic");
    assert!(matches!(
        error.kind(),
        FailureKind::Stderr | FailureKind::UnexpectedExit
    ));
    assert!(String::from_utf8_lossy(error.stderr()).contains(SERVICE_TAIL_DIAGNOSTIC));
}

#[test]
fn bounded_service_stops_stderr_flood_at_fixed_limit() {
    let limits = OutputLimits::new(16 * 1024, 24 * 1024);
    let service = spawn_bounded_service(
        fixture_command("service_stderr_flood"),
        Duration::from_secs(5),
        limits,
    )
    .expect("start stderr-flood fixture");
    wait_for_service_completion(&service);
    let error = service
        .finish_checked()
        .expect_err("stderr flood must not be accepted or retained without a bound");
    assert_eq!(error.kind(), FailureKind::OutputLimit);
    assert!(!error.stderr().is_empty());
    assert!(error.stdout().len() <= limits.per_stream);
    assert!(error.stderr().len() <= limits.per_stream);
    assert!(error.stdout().len() + error.stderr().len() <= limits.total);
    assert!(error.to_string().contains("truncated"));
}

#[test]
fn bounded_service_timeout_is_absolute_without_a_stop_request() {
    let started = Instant::now();
    let service = spawn_bounded_service(
        fixture_command("service_alive"),
        Duration::from_millis(200),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("start absolute-timeout fixture");
    // No finish request starts the clock: the worker must enforce the timeout
    // while the caller is still holding the service handle.
    wait_for_service_completion(&service);
    let error = service
        .finish_checked()
        .expect_err("an unfinished service must hit its original deadline");
    assert_eq!(error.kind(), FailureKind::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "absolute timeout or cleanup exceeded its bound: {:?}",
        started.elapsed()
    );
}

#[test]
fn bounded_service_drop_is_finite_for_a_live_direct_child() {
    let service = spawn_bounded_service(
        fixture_command("service_alive"),
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("start direct-child drop fixture");
    wait_for_service_marker(&service, SERVICE_READY);
    let started = Instant::now();
    drop(service);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "service Drop blocked beyond its cleanup bound: {:?}",
        started.elapsed()
    );
    // Dropping a guard is deliberately not a checked success receipt, nor does
    // this direct-child fixture establish descendant process-tree containment.
}

#[test]
fn bounded_service_preserves_spawn_failure_without_a_checked_receipt() {
    let executable = missing_service_executable();
    let filename = executable
        .file_name()
        .expect("missing fixture filename")
        .to_string_lossy()
        .into_owned();
    let mut command = Command::new(executable);
    command.arg("bounded-service-spawn-failure-contract");
    let service = spawn_bounded_service(
        command,
        Duration::from_secs(5),
        OutputLimits::new(64 * 1024, 96 * 1024),
    )
    .expect("valid budgets permit starting the supervisor, not the missing command");
    wait_for_service_completion(&service);
    let error = service
        .finish_checked()
        .expect_err("a command spawn failure must never issue a checked receipt");
    assert_eq!(error.kind(), FailureKind::Spawn);
    assert!(error.stdout().is_empty());
    assert!(error.stderr().is_empty());
    let rendered = error.to_string();
    assert!(rendered.contains(&filename));
    assert!(rendered.contains("bounded-service-spawn-failure-contract"));
}

#[test]
fn bounded_service_rejects_zero_budgets_before_starting_a_command() {
    let executable = missing_service_executable();
    for (label, timeout, limits) in [
        (
            "zero timeout",
            Duration::ZERO,
            OutputLimits::new(16 * 1024, 24 * 1024),
        ),
        (
            "zero per-stream limit",
            Duration::from_secs(5),
            OutputLimits::new(0, 24 * 1024),
        ),
        (
            "zero total limit",
            Duration::from_secs(5),
            OutputLimits::new(16 * 1024, 0),
        ),
    ] {
        let error = match spawn_bounded_service(Command::new(&executable), timeout, limits) {
            Err(error) => error,
            Ok(service) => {
                drop(service);
                panic!("{label} must be rejected before returning a supervisor");
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{label}");
    }
}

#[test]
fn bounded_service_stops_stdout_flood_at_per_stream_and_total_limits() {
    for limits in [
        OutputLimits::new(16 * 1024, 24 * 1024),
        OutputLimits::new(24 * 1024, 8 * 1024),
    ] {
        let service =
            spawn_bounded_service(fixture_command("large"), Duration::from_secs(5), limits)
                .expect("start stdout-flood fixture");
        wait_for_service_completion(&service);
        let error = service
            .finish_checked()
            .expect_err("stdout flood must not issue a checked receipt");
        assert_eq!(error.kind(), FailureKind::OutputLimit);
        assert!(!error.stdout().is_empty());
        assert!(error.stdout().len() <= limits.per_stream);
        assert!(error.stderr().len() <= limits.per_stream);
        assert!(error.stdout().len() + error.stderr().len() <= limits.total);
        assert!(error.to_string().contains("truncated"));
    }
}

#[test]
fn bounded_service_injected_boundary_failures_preserve_fault_and_cleanup_receipt() {
    for (label, fault, expected_kind, wait_for_ready) in [
        ("wait", ServiceFault::Wait, FailureKind::Wait, false),
        (
            "terminate",
            ServiceFault::Terminate,
            FailureKind::Termination,
            true,
        ),
        (
            "reader spawn",
            ServiceFault::ReaderSpawn,
            FailureKind::Reader,
            false,
        ),
        (
            "reader read",
            ServiceFault::ReaderRead,
            FailureKind::Reader,
            false,
        ),
    ] {
        let started = Instant::now();
        let service = spawn_bounded_service_with_fault(
            fixture_command("service_alive"),
            Duration::from_secs(5),
            OutputLimits::new(64 * 1024, 96 * 1024),
            fault,
        )
        .expect("start explicit test-only boundary-fault fixture");
        if wait_for_ready {
            wait_for_service_marker(&service, SERVICE_READY);
        } else {
            wait_for_service_completion(&service);
        }
        let error = service
            .finish_checked()
            .expect_err("an injected boundary failure must not issue a checked receipt");
        assert_eq!(error.kind(), expected_kind, "{label}");
        let rendered = error.to_string();
        assert!(rendered.contains("injected"), "{label}: {rendered}");
        assert!(rendered.contains("actual status:"), "{label}: {rendered}");
        assert!(
            !rendered.contains("actual status: unconfirmed"),
            "{label}: cleanup did not confirm the real child exit: {rendered}"
        );
        assert!(
            !rendered.contains("did not finish after cleanup"),
            "{label}: output drain was not completed: {rendered}"
        );
        assert!(
            !rendered.contains("not reaped within cleanup"),
            "{label}: direct child was not reaped: {rendered}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{label}: injected-fault cleanup exceeded its bound: {:?}",
            started.elapsed()
        );
    }
    // These are one-shot injected I/O-boundary failures with real direct-child
    // cleanup, not evidence that the kernel actually denied the operations.
}
