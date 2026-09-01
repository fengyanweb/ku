#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, FailureKind, OutputLimits};
use std::io::{self, Write};
use std::process::Command;
use std::time::{Duration, Instant};

const CHILD_MODE: &str = "KU_TEST_BOUNDED_PROCESS_CHILD_MODE";
const FIXTURE_NAME: &str = "bounded_process_fixture_child";

fn fixture_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", FIXTURE_NAME, "--nocapture"])
        .env(CHILD_MODE, mode);
    command
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
