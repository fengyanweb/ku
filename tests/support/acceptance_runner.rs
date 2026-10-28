//! Bound the whole disposable fixture, including its database descendants.
//! Individual pg_ctl invocations must not own this boundary: its server must
//! remain alive until the Python fixture finishes its checks and cleanup.
#[allow(dead_code)]
#[path = "bounded_process.rs"]
mod bounded_process;

use bounded_process::{run_bounded, FailureKind, OutputLimits};
use std::{env, io::Write, process::Command, time::Duration};

fn main() {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    let seconds = arguments
        .first()
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=300).contains(value));
    if arguments.len() < 2 || seconds.is_none() {
        eprintln!("usage: acceptance-runner <1..300 seconds> <program> [arguments...]");
        std::process::exit(2);
    }
    let mut command = Command::new(&arguments[1]);
    command.args(&arguments[2..]);
    let result = run_bounded(
        &mut command,
        Duration::from_secs(seconds.unwrap()),
        OutputLimits::new(1024 * 1024, 2 * 1024 * 1024),
    );
    match result {
        Ok(output) => {
            let _ = std::io::stdout().write_all(&output.stdout);
            let _ = std::io::stderr().write_all(&output.stderr);
            std::process::exit(output.status.code().unwrap_or(1));
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(if error.kind() == FailureKind::Timeout {
                124
            } else {
                1
            });
        }
    }
}
