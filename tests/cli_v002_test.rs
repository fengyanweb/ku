use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

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
    repo_root().join("target").join("debug").join(exe)
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

fn write_temp_ku(name: &str, source: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("ku-{}-{}", std::process::id(), name));
    fs::write(&path, source).expect("failed to write temp ku file");
    path
}

#[test]
fn version_flags_print_v002() {
    for flag in ["-v", "--version", "version"] {
        let result = run_ku(&[flag]);

        assert!(!result.timed_out, "version command timed out");
        assert_eq!(result.code, Some(0), "version command failed: {flag}");
        assert!(
            result.stdout.trim().contains("0.0.6"),
            "version output should contain 0.0.6\nstdout:\n{}\nstderr:\n{}",
            result.stdout,
            result.stderr
        );
    }
}

#[test]
fn help_flags_print_commands() {
    for flag in ["-h", "-help", "--help", "help"] {
        let result = run_ku(&[flag]);

        assert!(!result.timed_out, "help command timed out");
        assert_eq!(result.code, Some(0), "help command failed: {flag}");
        let text = result.stdout.to_lowercase();
        assert!(text.contains("usage"), "help should include usage");
        assert!(text.contains("run"), "help should include run");
        assert!(text.contains("check"), "help should include check");
        assert!(text.contains("version"), "help should include version");
    }
}

#[test]
fn invalid_command_prints_help() {
    let result = run_ku(&["wat"]);

    assert_ne!(result.code, Some(0), "unknown command should fail");
    let text = format!("{}\n{}", result.stdout, result.stderr).to_lowercase();
    assert!(text.contains("unknown command"), "missing command error");
    assert!(text.contains("usage"), "missing help text");
}

#[test]
fn check_success_names_checked_file() {
    let path = path_arg(&repo_root().join("examples").join("hello.ku"));
    let result = run_ku(&["check", &path]);

    assert_eq!(
        result.code,
        Some(0),
        "check should pass\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(result.stdout.contains("check ok"), "missing check ok");
    assert!(result.stdout.contains("hello.ku"), "missing file name");
}

#[test]
fn cli_rejects_missing_and_extra_arguments() {
    for args in [
        vec!["run"],
        vec!["check"],
        vec!["run", "examples\\hello.ku", "extra"],
        vec!["check", "examples\\hello.ku", "extra"],
        vec!["examples\\hello.ku", "extra"],
        vec!["version", "extra"],
        vec!["-h", "extra"],
    ] {
        let result = run_ku(&args);
        assert_ne!(result.code, Some(0), "args should fail: {args:?}");
        let text = format!("{}\n{}", result.stdout, result.stderr).to_lowercase();
        assert!(
            text.contains("usage"),
            "bad command should include help for {args:?}\n{text}"
        );
    }
}

#[test]
fn check_error_prints_diagnostic_location() {
    let path = path_arg(&repo_root().join("examples").join("error.ku"));
    let result = run_ku(&["check", &path]);

    assert_ne!(result.code, Some(0), "check error should fail");
    let text = format!("{}\n{}", result.stdout, result.stderr);
    assert!(text.contains("error:"), "missing error heading");
    assert!(text.contains("error.ku:"), "missing file location");
    assert!(text.contains("|"), "missing source gutter");
}

#[test]
fn shorthand_file_path_runs_ku_file() {
    let path = path_arg(&repo_root().join("examples").join("hello.ku"));
    let result = run_ku(&[&path]);

    assert!(!result.timed_out, "shorthand run timed out");
    assert_eq!(result.code, Some(0), "shorthand run failed");
    assert!(result.stdout.contains("Hello Ku"));
}

#[test]
fn run_rejects_non_ku_file_before_reading_binary_as_text() {
    let exe = path_arg(&ku_binary());
    let result = run_ku(&["run", &exe]);

    assert_ne!(result.code, Some(0), "non-.ku file should fail");
    let combined = format!("{}\n{}", result.stdout, result.stderr);
    assert!(
        combined.contains("expected a .ku source file"),
        "unexpected output\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn run_v002_features_print_expected_output() {
    let path = write_temp_ku(
        "cli_v002_features.ku",
        r#"
fn main() {
    name = 'Ku'
    total:int
    add = (a, b) => {
        return a + b
    }
    total = add(10, 20)
    print(`Hello {name} {total}`)
}
"#,
    );
    let path = path_arg(&path);
    let result = run_ku(&["run", &path]);

    assert!(
        !result.timed_out,
        "v0.0.2 feature run timed out\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "v0.0.2 feature run failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result
            .stdout
            .lines()
            .any(|line| line.trim() == "Hello Ku 30"),
        "v0.0.2 output mismatch\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
}
