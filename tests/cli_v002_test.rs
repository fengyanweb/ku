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
fn async_task_timeout_cancel_and_status_are_bounded() {
    let path = write_temp_ku(
        "async-task-lifecycle.ku",
        r#"
async fn spin(): int! {
    while (true) {
    }
    return ok(1)
}

async fn main(): null! {
    tasks = [
        spin(), spin(), spin(), spin(),
        spin(), spin(), spin(), spin(),
        spin(), spin(), spin(), spin()
    ]
    for task in tasks.clone() {
        println(task.await_timeout(0))
        task.cancel()
        println(task.status())
    }
    for task in tasks {
        println(await task)
    }
    return ok(null)
}
"#,
    );
    let path_text = path_arg(&path);
    let result = run_with_timeout(&ku_binary(), &["run", &path_text], Duration::from_secs(3));
    fs::remove_file(&path).ok();

    assert!(
        !result.timed_out,
        "async lifecycle test must not deadlock\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "async lifecycle run failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.contains("timeout"),
        "timeout should be a structured task error: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("cancelled"),
        "cancelled task should wake waiters and expose its state: {}",
        result.stdout
    );
    let lines = result.stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        36,
        "unexpected stress output:\n{}",
        result.stdout
    );
    for status in lines.iter().take(24).skip(1).step_by(2) {
        assert!(
            matches!(*status, "cancelling" | "cancelled"),
            "unexpected task status {status:?}"
        );
    }
    assert!(
        lines.iter().skip(24).all(|line| line.contains("cancelled")),
        "every cancelled task should be awaitable:\n{}",
        result.stdout
    );
}

#[test]
fn version_flags_print_v002() {
    for flag in ["-v", "--version", "version"] {
        let result = run_ku(&[flag]);

        assert!(!result.timed_out, "version command timed out");
        assert_eq!(result.code, Some(0), "version command failed: {flag}");
        assert!(
            result.stdout.trim().contains(env!("CARGO_PKG_VERSION")),
            "version output should contain {}\nstdout:\n{}\nstderr:\n{}",
            env!("CARGO_PKG_VERSION"),
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
    assert!(text.contains("error["), "missing error code heading");
    assert!(text.contains("error.ku:"), "missing file location");
    assert!(text.contains("|"), "missing source gutter");
}

#[test]
fn check_errors_include_codes_notes_and_help() {
    let cases = [
        (
            "let.ku",
            "fn main() { let name = \"Ku\" }",
            "E0105",
            "remove `let`",
        ),
        (
            "switch.ku",
            "fn main() { value = switch 1 { 1 => 1 } }",
            "E0104",
            "replace `switch` with `match`",
        ),
        (
            "condition.ku",
            "fn main() { if (\"yes\") { print(\"bad\") } }",
            "E0302",
            "truthy/falsy",
        ),
        (
            "question.ku",
            "import \"std.fs\"\nfn main() { text = fs.try_read(\"x\")? }",
            "E0401",
            "return `T!`",
        ),
    ];

    for (name, source, code, help) in cases {
        let path = write_temp_ku(name, source);
        let path_text = path_arg(&path);
        let result = run_ku(&["check", &path_text]);
        fs::remove_file(&path).ok();
        let text = format!("{}\n{}", result.stdout, result.stderr);
        assert_ne!(result.code, Some(0), "{name} should fail");
        assert!(text.contains(code), "{name} missing {code}:\n{text}");
        assert!(text.contains(help), "{name} missing help/note:\n{text}");
    }
}

#[test]
fn check_json_emits_stable_json_lines_diagnostic() {
    let path = write_temp_ku(
        "json-diagnostic.ku",
        "fn main() {\n    if (\"yes\") {\n        print(\"bad\")\n    }\n}\n",
    );
    let path_text = path_arg(&path);
    let result = run_ku(&["check", "--json", &path_text]);
    fs::remove_file(&path).ok();

    assert_ne!(result.code, Some(0), "invalid source should fail");
    assert!(
        result.stdout.trim().is_empty(),
        "JSON check stdout should be empty"
    );
    let lines = result
        .stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "expected one JSON diagnostic line");
    let line = lines[0];
    for field in [
        "\"level\":\"error\"",
        "\"code\":\"E0302\"",
        "condition must be bool",
        "\"file\":",
        "\"line\":2",
        "\"column\":5",
        "\"endLine\":",
        "\"endColumn\":",
        "\"notes\":[",
        "\"helps\":[",
    ] {
        assert!(
            line.contains(field),
            "missing {field} in JSON line:\n{line}"
        );
    }
    assert!(line.starts_with('{') && line.ends_with('}'));
}

#[test]
fn check_json_success_is_silent_and_rejects_extra_arguments() {
    let path = path_arg(&repo_root().join("examples").join("hello.ku"));
    let result = run_ku(&["check", "--json", &path]);
    assert_eq!(result.code, Some(0), "valid JSON check should pass");
    assert!(
        result.stdout.is_empty(),
        "successful JSON check must be silent"
    );
    assert!(
        result.stderr.is_empty(),
        "successful JSON check must be silent"
    );

    let result = run_ku(&["check", "--json", &path, "extra"]);
    assert_ne!(
        result.code,
        Some(0),
        "extra JSON check argument should fail"
    );
    assert!(
        result.stderr.contains("too many arguments"),
        "missing argument error:\n{}",
        result.stderr
    );
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
