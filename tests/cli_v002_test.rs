use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
    run_with_timeout_in(repo_root(), bin, args, timeout)
}

fn run_with_timeout_in(cwd: PathBuf, bin: &Path, args: &[&str], timeout: Duration) -> RunResult {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
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

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    env::temp_dir().join(format!("ku-{}-{}-{}", std::process::id(), name, nanos))
}

#[test]
fn async_tasks_start_immediately_and_await_once() {
    let path = write_temp_ku(
        "async-task-await-once.ku",
        r#"
async fn load(value:int): int! {
    return ok(value)
}

async fn main(): null! {
    first = load(1)
    second = load(2)
    println("request started")
    a = await first?
    b = await second?
    println(a + b)
    return ok(null)
}
"#,
    );
    let path_text = path_arg(&path);
    let result = run_with_timeout(&ku_binary(), &["run", &path_text], Duration::from_secs(3));
    fs::remove_file(&path).ok();

    assert!(
        !result.timed_out,
        "async await test must not deadlock\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.code,
        Some(0),
        "async await run failed\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    let lines = result.stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines, vec!["request started", "3"]);
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
fn create_init_and_template_commands_manage_projects() {
    let root = unique_temp_dir("project-commands");
    fs::create_dir_all(&root).expect("create temp root");
    let bin = ku_binary();

    let list = run_with_timeout_in(
        root.clone(),
        &bin,
        &["template", "list"],
        Duration::from_secs(2),
    );
    assert_eq!(list.code, Some(0), "template list failed: {}", list.stderr);
    assert!(list.stdout.contains("basic"));
    assert!(list.stdout.contains("http"));

    let created = run_with_timeout_in(
        root.clone(),
        &bin,
        &["create", "my-api", "--template", "http"],
        Duration::from_secs(2),
    );
    assert_eq!(
        created.code,
        Some(0),
        "create failed\nstdout:\n{}\nstderr:\n{}",
        created.stdout,
        created.stderr
    );
    let project = root.join("my-api");
    assert!(project.join("ku.mod").exists(), "missing ku.mod");
    assert!(
        project.join("src").join("main.ku").exists(),
        "missing main.ku"
    );
    let manifest = fs::read_to_string(project.join("ku.mod")).expect("read ku.mod");
    assert!(
        manifest.contains("name = \"my-api\""),
        "manifest should preserve valid lowercase package name:\n{manifest}"
    );

    let check = run_with_timeout_in(project.clone(), &bin, &["check"], Duration::from_secs(2));
    assert_eq!(
        check.code,
        Some(0),
        "project check failed\nstdout:\n{}\nstderr:\n{}",
        check.stdout,
        check.stderr
    );

    let duplicate = run_with_timeout_in(
        root.clone(),
        &bin,
        &["create", "my-api"],
        Duration::from_secs(2),
    );
    assert_ne!(duplicate.code, Some(0), "duplicate create should fail");
    assert!(
        duplicate.stderr.contains("E1001"),
        "duplicate create missing code: {}",
        duplicate.stderr
    );

    let mixed_case = run_with_timeout_in(
        root.clone(),
        &bin,
        &["create", "HelloWorld", "--template", "http"],
        Duration::from_secs(2),
    );
    assert_eq!(
        mixed_case.code,
        Some(0),
        "mixed-case create failed\nstdout:\n{}\nstderr:\n{}",
        mixed_case.stdout,
        mixed_case.stderr
    );
    let mixed_project = root.join("HelloWorld");
    let mixed_manifest = fs::read_to_string(mixed_project.join("ku.mod")).expect("read ku.mod");
    assert!(
        mixed_manifest.contains("name = \"helloworld\""),
        "mixed-case project names should lower-case the package name:\n{mixed_manifest}"
    );
    let mixed_check = run_with_timeout_in(
        mixed_project.clone(),
        &bin,
        &["check"],
        Duration::from_secs(2),
    );
    assert_eq!(
        mixed_check.code,
        Some(0),
        "mixed-case project check failed\nstdout:\n{}\nstderr:\n{}",
        mixed_check.stdout,
        mixed_check.stderr
    );

    let init_dir = root.join("existing");
    fs::create_dir_all(&init_dir).expect("create init dir");
    let init = run_with_timeout_in(
        init_dir.clone(),
        &bin,
        &["init", "--template", "cli"],
        Duration::from_secs(2),
    );
    assert_eq!(
        init.code,
        Some(0),
        "init failed\nstdout:\n{}\nstderr:\n{}",
        init.stdout,
        init.stderr
    );
    let run = run_with_timeout_in(init_dir.clone(), &bin, &["run"], Duration::from_secs(2));
    assert_eq!(run.code, Some(0), "project run failed: {}", run.stderr);
    assert!(run.stdout.contains("Ku CLI tool"));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn create_unknown_template_reports_available_templates() {
    let root = unique_temp_dir("bad-template");
    fs::create_dir_all(&root).expect("create temp root");
    let result = run_with_timeout_in(
        root.clone(),
        &ku_binary(),
        &["create", "demo", "--template", "web"],
        Duration::from_secs(2),
    );
    fs::remove_dir_all(&root).ok();

    assert_ne!(result.code, Some(0), "unknown template should fail");
    assert!(
        result.stderr.contains("E1003"),
        "missing E1003: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("available templates"),
        "missing template help: {}",
        result.stderr
    );
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
        (
            "task-await-once.ku",
            "async fn load(): int! { return ok(1) }\nasync fn main(): null! {\n    task = load()\n    first = await task?\n    second = await task?\n    return ok(null)\n}\n",
            "E0804",
            "store the awaited value",
        ),
        (
            "http-handler-arity.ku",
            "import \"std.http\"\nfn main() {\n    app = http.service()\n    app.get(\"/\", fn(req, res) { return http.text(\"bad\") })\n}\n",
            "E0701",
            "fn(req)",
        ),
        (
            "http-handler-return.ku",
            "import \"std.http\"\nfn main() {\n    app = http.service()\n    app.get(\"/\", fn() { return \"bad\" })\n}\n",
            "E0702",
            "HttpResponse",
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
fn check_json_import_errors_include_actionable_help() {
    let path = write_temp_ku(
        "json-import-diagnostic.ku",
        "import { Task } from \"std\"\nfn main() { print(1) }\n",
    );
    let path_text = path_arg(&path);
    let result = run_ku(&["check", "--json", &path_text]);
    fs::remove_file(&path).ok();

    assert_ne!(result.code, Some(0), "invalid import should fail");
    let line = result
        .stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("expected JSON diagnostic");
    for field in [
        "\"code\":\"E0601\"",
        "unknown std module",
        "\"line\":1",
        "\"column\":10",
        "standard library module names are lowercase",
        "import { task, time } from \\\"std\\\"",
    ] {
        assert!(
            line.contains(field),
            "missing {field} in JSON line:\n{line}"
        );
    }
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
fn check_rejects_unused_imports_by_default_with_json_help() {
    let err_path = write_temp_ku(
        "unused-import.ku",
        r#"
import { http } from "std"

fn main() {
    println("ok")
}
"#,
    );
    let err_text = path_arg(&err_path);
    let result = run_ku(&["check", "--json", &err_text]);
    fs::remove_file(&err_path).ok();

    assert_ne!(result.code, Some(0), "unused import should fail");
    assert!(
        result.stderr.contains("\"code\":\"E0603\""),
        "missing unused import code:\n{}",
        result.stderr
    );
    assert!(
        result.stderr.contains("unused import 'http'"),
        "missing unused import message:\n{}",
        result.stderr
    );
    assert!(
        result.stderr.contains("alias it with a leading `_`"),
        "missing unused import help:\n{}",
        result.stderr
    );

    let ok_root = unique_temp_dir("unused-import-discard");
    fs::create_dir_all(&ok_root).expect("create unused import temp dir");
    fs::write(
        ok_root.join("helper.ku"),
        r#"
fn Helper(): int {
    return 1
}
"#,
    )
    .expect("write helper module");
    let ok_path = ok_root.join("main.ku");
    fs::write(
        &ok_path,
        r#"
import { Helper as _Helper } from "./helper.ku"

fn main() {
    println("ok")
}
"#,
    )
    .expect("write main module");
    let ok_text = path_arg(&ok_path);
    let result = run_ku(&["check", &ok_text]);
    fs::remove_dir_all(&ok_root).ok();
    assert_eq!(result.code, Some(0), "discard import alias should pass");
}

#[test]
fn check_deny_unused_reports_local_bindings_with_help() {
    let path = write_temp_ku(
        "deny-unused-local.ku",
        r#"
fn main() {
    used = 1
    print(used)
    unused = 2
}
"#,
    );
    let path_text = path_arg(&path);
    let result = run_ku(&["check", "--deny-unused", &path_text]);
    fs::remove_file(&path).ok();

    assert_ne!(result.code, Some(0), "unused binding should fail");
    assert!(
        result.stderr.contains("unused local binding 'unused'"),
        "missing unused error:\n{}",
        result.stderr
    );
    assert!(
        result.stderr.contains("rename it with a leading `_`"),
        "missing help:\n{}",
        result.stderr
    );
}

#[test]
fn check_deny_unused_json_and_discard_prefix_work() {
    let ok_path = write_temp_ku(
        "deny-unused-ok.ku",
        r#"
fn main() {
    _ignored = 1
    used = 2
    print(`value {used}`)
}
"#,
    );
    let ok_text = path_arg(&ok_path);
    let result = run_ku(&["check", "--deny-unused", &ok_text]);
    fs::remove_file(&ok_path).ok();
    assert_eq!(result.code, Some(0), "discard-prefixed binding should pass");

    let err_path = write_temp_ku(
        "deny-unused-json.ku",
        r#"
fn main() {
    unused = 1
}
"#,
    );
    let err_text = path_arg(&err_path);
    let result = run_ku(&["check", "--json", "--deny-unused", &err_text]);
    fs::remove_file(&err_path).ok();

    assert_ne!(result.code, Some(0), "unused JSON check should fail");
    let line = result
        .stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("expected JSON diagnostic");
    for field in [
        "\"code\":\"E0905\"",
        "unused local binding 'unused'",
        "strict unused checks are enabled",
        "rename it with a leading `_`",
    ] {
        assert!(
            line.contains(field),
            "missing {field} in JSON line:\n{line}"
        );
    }
}

#[test]
fn check_deny_unused_does_not_count_reassignment_or_self_recursion_as_reads() {
    let path = write_temp_ku(
        "deny-unused-writes.ku",
        r#"
fn main() {
    value = 1
    value = 2

    fn unused_fact(n: int): int {
        if (n <= 1) return 1
        return n * unused_fact(n - 1)
    }
}
"#,
    );
    let path_text = path_arg(&path);
    let result = run_ku(&["check", "--deny-unused", &path_text]);
    fs::remove_file(&path).ok();

    assert_ne!(result.code, Some(0), "unused writes should fail");
    assert!(
        result.stderr.contains("unused local binding 'value'")
            || result.stderr.contains("unused local binding 'unused_fact'"),
        "missing unused binding error:\n{}",
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
    add = (a: int, b: int) => {
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
