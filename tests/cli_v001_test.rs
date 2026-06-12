use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct RunResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
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
    let candidates = [
        repo_root().join("target").join("debug").join(exe),
        repo_root().join("target").join("release").join(exe),
    ];

    candidates
        .iter()
        .find(|path| path.exists())
        .cloned()
        .expect("ku binary not found; set KU_BIN or build the ku binary first")
}

fn example(name: &str) -> PathBuf {
    repo_root().join("examples").join(name)
}

fn run_with_timeout(bin: &Path, args: &[&str], timeout: Duration) -> RunResult {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(repo_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn ku binary");

    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .expect("failed to poll ku process")
            .is_some()
        {
            let output = child
                .wait_with_output()
                .expect("failed to collect ku output");
            return RunResult {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: false,
            };
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed-out ku output");
            return RunResult {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: true,
            };
        }

        thread::sleep(Duration::from_millis(20));
    }
}

fn run_ku(args: &[&str]) -> RunResult {
    let bin = ku_binary();
    run_with_timeout(&bin, args, Duration::from_secs(2))
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn run_hello_prints_expected_greeting() {
    let path = path_arg(&example("hello.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "hello example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "hello example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.contains("Hello Ku"),
        "hello output should contain greeting\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_fib_prints_tenth_fibonacci_number() {
    let path = path_arg(&example("fib.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "fib example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "fib example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.lines().any(|line| line.trim() == "55"),
        "fib(10) should print 55\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_function_prints_return_value() {
    let path = path_arg(&example("function.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "function example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "function example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.lines().any(|line| line.trim() == "30"),
        "add(10, 20) should print 30\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_loop_terminates_and_prints_expected_sequence() {
    let path = path_arg(&example("loop.ku"));
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "loop example did not terminate within timeout\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "loop example failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );

    let values: Vec<_> = result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        values,
        ["0", "1", "2", "3", "4"],
        "loop should print 0 through 4 once\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn check_error_reports_clear_type_error() {
    let path = path_arg(&example("error.ku"));
    let result = run_ku(&["check", &path]);

    assert!(
        !result.timed_out,
        "check error example timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_ne!(
        result.code,
        Some(0),
        "type error should fail check\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );

    let combined = format!("{}\n{}", result.stdout, result.stderr).to_lowercase();
    assert!(
        combined.contains("type") || combined.contains("类型") || combined.contains("error"),
        "error output should clearly identify the failure\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}
