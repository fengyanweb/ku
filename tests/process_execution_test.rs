//! Whole-program process-limit acceptance. Real Linux cases require an
//! explicitly delegated KU_TEST_CGROUP_ROOT; an absent delegation is reported
//! as unexecuted, never as evidence that the kernel provider passed.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

#[cfg(any(target_os = "linux", windows))]
use bounded_process::FailureKind;
use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const RUN_TIMEOUT: Duration = Duration::from_secs(45);
const OUTPUT_LIMITS: OutputLimits = OutputLimits::new(256 * 1024, 384 * 1024);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "ku-process-execution-{}-{stamp}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create process acceptance directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "failed to remove owned test directory {}: {error}",
                self.0.display()
            );
        }
    }
}

fn ku_command(directory: &Path) -> Command {
    let binary = env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let mut command = Command::new(binary);
    command.current_dir(directory).arg("supervise");
    command
}

fn finish(command: &mut Command) -> BoundedOutput {
    run_bounded(command, RUN_TIMEOUT, OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("supervisor exceeded its external test bounds: {error}"))
}

fn diagnostics(output: &BoundedOutput) -> String {
    format!(
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(any(target_os = "linux", windows))]
fn records(output: &BoundedOutput) -> Vec<&str> {
    std::str::from_utf8(&output.stdout)
        .expect("JSONL receipts must be UTF-8")
        .lines()
        .map(|line| {
            assert!(
                line.starts_with('{') && line.ends_with('}'),
                "incomplete JSONL record: {line}"
            );
            line
        })
        .collect()
}

#[cfg(any(target_os = "linux", windows))]
fn field<'a>(record: &'a str, name: &str) -> &'a str {
    let needle = format!("\"{name}\":");
    record
        .split_once(&needle)
        .unwrap_or_else(|| panic!("missing {name} in {record}"))
        .1
}

#[cfg(any(target_os = "linux", windows))]
fn number(record: &str, name: &str) -> u64 {
    field(record, name)
        .split([',', '}'])
        .next()
        .expect("number field")
        .parse()
        .unwrap_or_else(|error| panic!("invalid {name}: {error}; {record}"))
}

#[cfg(any(target_os = "linux", windows))]
fn text<'a>(record: &'a str, name: &str) -> &'a str {
    // These assertions inspect only generated enum/base64 fields. Entry paths
    // are deliberately not parsed by a second, partial JSON implementation.
    field(record, name)
        .strip_prefix('"')
        .and_then(|value| value.split('"').next())
        .unwrap_or_else(|| panic!("invalid string field {name}: {record}"))
}

#[cfg(any(target_os = "linux", windows))]
fn assert_finished(output: &BoundedOutput, expected_failed: u64) {
    let records = records(output);
    let summary = records
        .iter()
        .find(|record| text(record, "type") == "summary")
        .unwrap_or_else(|| panic!("missing final summary: {}", diagnostics(output)));
    assert_eq!(
        number(summary, "failed"),
        expected_failed,
        "{}",
        diagnostics(output)
    );
    assert_eq!(number(summary, "active_workers"), 0);
    assert_eq!(number(summary, "reserved_worker_bytes"), 0);
    assert_eq!(
        output.status.success(),
        expected_failed == 0,
        "{}",
        diagnostics(output)
    );
    for record in records
        .iter()
        .filter(|record| text(record, "type") == "execution")
    {
        assert_eq!(number(record, "retries"), 0);
        assert_ne!(text(record, "outcome"), "cleanup_unconfirmed", "{record}");
        if text(record, "outcome") != "completed" {
            assert_eq!(text(record, "side_effects"), "unknown");
        }
        if record.contains("\"process_count\":") {
            assert_eq!(number(record, "process_count"), 0, "{record}");
        }
    }
}

#[test]
fn supervise_help_and_invalid_limits_do_not_require_a_memory_provider() {
    let directory = TestDirectory::new();
    let mut command = ku_command(&directory.0);
    command.arg("--help");
    let help = finish(&mut command);
    assert!(help.status.success(), "{}", diagnostics(&help));
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("--worker-memory-bytes"));
    assert!(help_text.contains("--max-queued-bytes"));
    assert!(help_text.contains("failures are never retried"));
    for (arguments, expected) in [
        (vec!["--max-workers", "0", "--", "unused.ku"], "positive"),
        (vec!["--max-workers", "65", "--", "unused.ku"], "positive"),
        (
            vec!["--max-output-bytes", "0", "--", "unused.ku"],
            "positive",
        ),
        (
            vec!["--deadline-ms", "86400001", "--", "unused.ku"],
            "positive",
        ),
        (
            vec!["--worker-memory-bytes", "-1", "--", "unused.ku"],
            "positive decimal",
        ),
        (
            vec![
                "--worker-memory-bytes",
                "18446744073709551616",
                "--",
                "unused.ku",
            ],
            "integer range",
        ),
        (vec!["--native", "--native", "--", "unused"], "duplicate"),
        (
            vec!["--unknown", "1", "--", "unused.ku"],
            "unknown supervise option",
        ),
        (vec!["--"], "at least one entry"),
        (vec!["--native"], "requires '--'"),
        (vec!["--max-workers"], "missing value"),
        (
            vec![
                "--worker-memory-bytes",
                "67108864",
                "--total-memory-bytes",
                "67108864",
                "--",
                "unused.ku",
            ],
            "control reserve",
        ),
    ] {
        let mut command = ku_command(&directory.0);
        command.args(arguments);
        let output = finish(&mut command);
        assert!(!output.status.success(), "{}", diagnostics(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "expected {expected:?}: {}",
            diagnostics(&output)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("\"type\":\"execution\""));
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
#[test]
fn supervise_unsupported_platform_refuses_before_user_program_runs() {
    use std::os::unix::fs::PermissionsExt;
    let directory = TestDirectory::new();
    let program = directory.0.join("must-not-run");
    let marker = directory.0.join("started");
    fs::write(&program, "#!/bin/sh\nprintf started > started\n")
        .expect("write executable marker fixture");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))
        .expect("executable permissions");
    let mut command = ku_command(&directory.0);
    command.arg("--native").arg("--").arg(&program);
    let output = finish(&mut command);
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("strict process memory execution is unavailable"),
        "{stderr}"
    );
    assert!(stderr.contains("Linux VM"), "{stderr}");
    assert!(!marker.exists(), "unsupported provider executed user code");
    assert!(
        output.stdout.is_empty(),
        "refusal should precede execution receipts"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn supervise_linux_rejects_absent_or_ordinary_directory_provider() {
    let directory = TestDirectory::new();
    let program = directory.0.join("must-not-run");
    fs::write(&program, "not executable").expect("write refused entry");
    for root in [None, Some(directory.0.as_path())] {
        let mut command = ku_command(&directory.0);
        if let Some(root) = root {
            command.arg("--cgroup-root").arg(root);
        }
        command.arg("--native").arg("--").arg(&program);
        let output = finish(&mut command);
        assert!(!output.status.success(), "{}", diagnostics(&output));
        let expected = if root.is_some() {
            "not a cgroup v2 filesystem"
        } else {
            "requires --cgroup-root"
        };
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            diagnostics(&output)
        );
        assert!(output.stdout.is_empty());
    }
}

#[cfg(any(target_os = "linux", windows))]
struct LiveProvider {
    cgroup_root: Option<PathBuf>,
}

#[cfg(any(target_os = "linux", windows))]
impl LiveProvider {
    fn available() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            let Some(root) = env::var_os("KU_TEST_CGROUP_ROOT") else {
                eprintln!("SKIP live process-limit acceptance: KU_TEST_CGROUP_ROOT is unset; no real Linux provider was exercised");
                return None;
            };
            Some(Self {
                cgroup_root: Some(PathBuf::from(root)),
            })
        }
        #[cfg(windows)]
        {
            Some(Self { cgroup_root: None })
        }
    }

    fn command(&self, directory: &Path, native: bool) -> Command {
        let mut command = ku_command(directory);
        command.args([
            "--worker-memory-bytes",
            "67108864",
            "--total-memory-bytes",
            "167772160",
            "--max-workers",
            "1",
            "--max-output-bytes",
            "4096",
            "--deadline-ms",
            "10000",
        ]);
        if let Some(root) = &self.cgroup_root {
            command.arg("--cgroup-root").arg(root);
        }
        if native {
            command.arg("--native");
        }
        command
    }

    fn owned_groups(&self) -> Vec<PathBuf> {
        let Some(root) = &self.cgroup_root else {
            return Vec::new();
        };
        let mut groups = fs::read_dir(root)
            .expect("read explicitly supplied cgroup delegation")
            .map(|entry| entry.expect("read cgroup entry").path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("ku-execution-"))
            })
            .collect::<Vec<_>>();
        groups.sort();
        groups
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn supervise_real_provider_contains_failures_and_releases_every_reservation() {
    let Some(provider) = LiveProvider::available() else {
        return;
    };
    let directory = TestDirectory::new();
    let groups_before = provider.owned_groups();

    let source = directory.0.join("source.ku");
    fs::write(&source, "fn main() { println(\"supervised source\") }\n")
        .expect("write Ku source fixture");
    let mut command = provider.command(&directory.0, false);
    command.arg("--").arg(&source);
    let output = finish(&mut command);
    assert_finished(&output, 0);
    let rows = records(&output);
    let execution = rows
        .iter()
        .find(|row| text(row, "type") == "execution")
        .expect("source receipt");
    assert_eq!(text(execution, "stdout_base64"), "c3VwZXJ2aXNlZCBzb3VyY2UK");

    let Some(fixture) = compile_fixture(&directory.0) else {
        panic!("live process-limit acceptance requires a C compiler; source execution passed, remaining acceptance was not executed");
    };
    let ok = fixture_entry(&fixture, "ok");
    let failure = fixture_entry(&fixture, "exit");
    let flood = fixture_entry(&fixture, "flood");
    let memory = fixture_entry(&fixture, "memory");

    for failing in [&failure, &flood] {
        let mut command = provider.command(&directory.0, true);
        command.arg("--").arg(failing).arg(&ok);
        let output = finish(&mut command);
        assert_finished(&output, 1);
        let rows = records(&output);
        let executions = rows
            .iter()
            .filter(|row| text(row, "type") == "execution")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(executions.len(), 2);
        assert_eq!(
            text(executions[0], "outcome"),
            if failing == &failure {
                "worker_exit"
            } else {
                "output_limit"
            }
        );
        assert_eq!(text(executions[1], "outcome"), "completed");
        assert_eq!(text(executions[1], "stdout_base64"), "QQD/Cg==");
        assert_eq!(text(executions[1], "stderr_base64"), "ZGlhZwo=");
        assert_eq!(invocations(failing), 1, "failing execution was retried");
        if failing == &flood {
            assert!(executions[0].contains("\"output_truncated\":true"));
            assert!(number(executions[0], "output_seen_bytes") > 4096);
            assert!(text(executions[0], "stdout_base64").len() <= 5464);
        }
        assert_eq!(
            provider.owned_groups(),
            groups_before,
            "owned cgroup survived a completed batch"
        );
    }

    // Repeated third-party C allocator failures run in fresh workers. A later
    // ordinary unit must still execute, without retries or retained budgets.
    let mut command = provider.command(&directory.0, true);
    command
        .arg("--")
        .arg(&memory)
        .arg(&memory)
        .arg(&memory)
        .arg(&ok);
    let output = finish(&mut command);
    assert_finished(&output, 3);
    let rows = records(&output);
    let executions = rows
        .iter()
        .filter(|row| text(row, "type") == "execution")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(executions.len(), 4);
    for row in &executions[..3] {
        assert!(
            matches!(text(row, "outcome"), "memory_limit" | "worker_exit"),
            "allocation pressure did not fail in its worker: {row}"
        );
        assert!(number(row, "memory_peak_bytes") > 0);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                text(row, "limit_metric"),
                "linux_cgroup_v2_charged_memory_bytes"
            );
            assert!(
                row.contains("\"memory_limit_event\":true"),
                "kernel memory event absent: {row}"
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                text(row, "limit_metric"),
                "windows_job_committed_memory_bytes"
            );
            // Windows can deny commit without terminating the process; the
            // fixture reports malloc's explicit NULL result and exits 23.
            assert_eq!(number(row, "exit_code"), 23, "{row}");
            assert_eq!(text(row, "stdout_base64"), "QUxMT0NfREVOSUVECg==");
        }
    }
    assert_eq!(text(executions[3], "outcome"), "completed");
    assert_eq!(invocations(&memory), 3);
    assert_eq!(provider.owned_groups(), groups_before);

    check_descendant_retirement(&provider, &directory.0, &fixture);
    check_concurrent_reservations(&provider, &directory.0, &fixture);
    check_queue_and_absolute_deadline(&provider, &directory.0, &fixture);
    assert_eq!(
        provider.owned_groups(),
        groups_before,
        "resource domains accumulated across failures"
    );
}

#[cfg(any(target_os = "linux", windows))]
fn check_descendant_retirement(provider: &LiveProvider, directory: &Path, fixture: &Path) {
    let groups_before = provider.owned_groups();
    let descendant = fixture_entry(fixture, "descendant");
    let ok = fixture_entry(fixture, "after-descendant");
    let mut command = provider.command(directory, true);
    command.arg("--").arg(&descendant).arg(&ok);
    let output = finish(&mut command);
    assert_finished(&output, 1);
    let rows = records(&output);
    let executions = rows
        .iter()
        .filter(|row| text(row, "type") == "execution")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(executions.len(), 2);
    assert_eq!(text(executions[0], "outcome"), "worker_exit");
    assert_eq!(number(executions[0], "exit_code"), 19);
    assert_eq!(number(executions[0], "process_count"), 0);
    assert_eq!(text(executions[1], "outcome"), "completed");
    assert_eq!(text(executions[1], "stdout_base64"), "QQD/Cg==");
    let basename = descendant
        .file_name()
        .expect("descendant filename")
        .to_string_lossy();
    let child_pid = fs::read_to_string(directory.join(format!("{basename}.child-pid")))
        .expect("parent must record a successfully started descendant PID");
    assert!(child_pid.trim().parse::<u64>().expect("descendant PID") > 0);
    assert_eq!(
        fs::read(directory.join(format!("{basename}.child-ready")))
            .expect("descendant reached its wait loop"),
        b"R"
    );
    assert_eq!(
        invocations(&descendant),
        1,
        "child branch must not impersonate another submission"
    );
    assert_eq!(invocations(&ok), 1);
    // process_count=0 comes from the kernel resource domain, so a brief Unix
    // zombie entry outside the domain is not mistaken for a surviving worker.
    assert_eq!(
        provider.owned_groups(),
        groups_before,
        "descendant resource domain was retained"
    );
}

#[cfg(any(target_os = "linux", windows))]
fn check_concurrent_reservations(provider: &LiveProvider, directory: &Path, fixture: &Path) {
    let entries = (1..=4)
        .map(|index| fixture_entry(fixture, &format!("hold-{index}")))
        .collect::<Vec<_>>();
    let mut command = ku_command(directory);
    command.args([
        "--worker-memory-bytes",
        "67108864",
        "--total-memory-bytes",
        "167772160",
        "--max-workers",
        "4",
        "--max-output-bytes",
        "4096",
        "--deadline-ms",
        "10000",
        "--native",
    ]);
    if let Some(root) = &provider.cgroup_root {
        command.arg("--cgroup-root").arg(root);
    }
    command.arg("--").args(&entries);
    let output = finish(&mut command);
    assert_finished(&output, 0);
    let rows = records(&output);
    let config = rows
        .iter()
        .find(|row| text(row, "type") == "configuration")
        .expect("configuration");
    let summary = rows
        .iter()
        .find(|row| text(row, "type") == "summary")
        .expect("summary");
    assert_eq!(number(config, "max_workers"), 4);
    assert_eq!(
        number(config, "max_running"),
        2,
        "aggregate budget must constrain the configured concurrency"
    );
    assert_eq!(
        number(summary, "peak_active_workers"),
        2,
        "fixture barrier requires two real simultaneous workers"
    );
    assert_eq!(number(summary, "peak_reserved_worker_bytes"), 2 * 67108864);
    assert_eq!(
        number(config, "worker_group_total_bytes") + number(config, "control_reserve_bytes"),
        167772160
    );
    assert!(number(config, "worker_group_total_bytes") < 3 * 67108864);
    for entry in entries {
        assert_eq!(invocations(&entry), 1);
    }
}

#[cfg(any(target_os = "linux", windows))]
fn check_queue_and_absolute_deadline(provider: &LiveProvider, directory: &Path, fixture: &Path) {
    let never = fixture_entry(fixture, "never");
    for (option, value, expected) in [
        ("--max-queued-jobs", "1", "max-queued-jobs"),
        ("--max-queued-bytes", "1", "max-queued-bytes"),
    ] {
        let mut command = provider.command(directory, true);
        command
            .arg(option)
            .arg(value)
            .arg("--")
            .arg(&never)
            .arg(&never);
        let output = finish(&mut command);
        assert!(!output.status.success(), "{}", diagnostics(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            diagnostics(&output)
        );
        assert_eq!(invocations(&never), 0, "queue rejection started user code");
        assert!(output.stdout.is_empty());
    }
    let sleeper = fixture_entry(fixture, "sleep");
    let mut command = ku_command(directory);
    command.args([
        "--worker-memory-bytes",
        "67108864",
        "--total-memory-bytes",
        "167772160",
        "--max-workers",
        "1",
        "--max-output-bytes",
        "4096",
        "--deadline-ms",
        "300",
        "--native",
    ]);
    if let Some(root) = &provider.cgroup_root {
        command.arg("--cgroup-root").arg(root);
    }
    command.arg("--").arg(&sleeper).arg(&never);
    let output = finish(&mut command);
    assert_finished(&output, 2);
    let rows = records(&output);
    let executions = rows
        .iter()
        .filter(|row| text(row, "type") == "execution")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(executions.len(), 2);
    assert_eq!(text(executions[0], "outcome"), "timeout");
    assert_eq!(text(executions[1], "outcome"), "queue_timeout");
    assert_eq!(invocations(&sleeper), 1);
    assert_eq!(invocations(&never), 0, "expired queued program ran");
}

#[cfg(any(target_os = "linux", windows))]
fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.into()
    }
}

#[cfg(any(target_os = "linux", windows))]
fn fixture_entry(fixture: &Path, stem: &str) -> PathBuf {
    let entry = fixture.with_file_name(executable_name(stem));
    fs::copy(fixture, &entry).expect("copy fixture executable for execution mode");
    entry
}

#[cfg(any(target_os = "linux", windows))]
fn invocations(entry: &Path) -> usize {
    let marker = entry.with_file_name(format!(
        "{}.runs",
        entry.file_name().expect("entry filename").to_string_lossy()
    ));
    match fs::read(marker) {
        Ok(bytes) => {
            assert!(
                bytes.iter().all(|byte| *byte == b'1'),
                "invalid invocation marker"
            );
            bytes.len()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read invocation marker: {error}"),
    }
}

#[cfg(any(target_os = "linux", windows))]
fn compile_fixture(directory: &Path) -> Option<PathBuf> {
    let source = directory.join("worker.c");
    let output = directory.join(executable_name("fixture"));
    fs::write(&source, WORKER_C).expect("write bounded C worker fixture");
    let mut candidates = Vec::<(PathBuf, Vec<String>)>::new();
    if let Some(spec) = env::var_os("KU_CC") {
        let path = PathBuf::from(&spec);
        if path.exists() {
            candidates.push((path, Vec::new()));
        } else {
            let spec = spec.to_string_lossy();
            let mut words = spec.split_whitespace();
            if let Some(program) = words.next() {
                candidates.push((PathBuf::from(program), words.map(str::to_owned).collect()));
            }
        }
    }
    for compiler in ["clang", "gcc", "cc"] {
        candidates.push((compiler.into(), Vec::new()));
    }
    candidates.push(("zig".into(), vec!["cc".into()]));
    for (compiler, prefix) in candidates {
        let mut command = Command::new(&compiler);
        command
            .current_dir(directory)
            .args(prefix)
            .arg(&source)
            .args(["-std=c11", "-O0"])
            .arg("-o")
            .arg(&output);
        match run_bounded(&mut command, Duration::from_secs(90), OUTPUT_LIMITS) {
            Ok(done) => {
                assert!(
                    done.status.success(),
                    "fixture compiler {} failed: {}",
                    compiler.display(),
                    diagnostics(&done)
                );
                return Some(output);
            }
            Err(error)
                if error.kind() == FailureKind::Spawn
                    && error.io_error_kind() == Some(std::io::ErrorKind::NotFound) => {}
            Err(error) => panic!("fixture compilation exceeded its bounds: {error}"),
        }
    }
    #[cfg(windows)]
    {
        compile_msvc_fixture(directory, &source, &output)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
fn compile_msvc_fixture(directory: &Path, source: &Path, output: &Path) -> Option<PathBuf> {
    let program_files = env::var_os("ProgramFiles(x86)").or_else(|| env::var_os("ProgramFiles"))?;
    let vswhere =
        PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    if !vswhere.exists() {
        return None;
    }
    let mut discover = Command::new(vswhere);
    discover.args([
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
    ]);
    let found = run_bounded(&mut discover, Duration::from_secs(15), OUTPUT_LIMITS)
        .expect("bounded Visual Studio discovery");
    assert!(
        found.status.success(),
        "Visual Studio discovery failed: {}",
        diagnostics(&found)
    );
    let install = String::from_utf8_lossy(&found.stdout);
    let vcvars = Path::new(install.trim()).join("VC/Auxiliary/Build/vcvars64.bat");
    if !vcvars.exists() {
        return None;
    }
    let script = directory.join("compile-worker.bat");
    fs::write(&script, format!(
        "@echo off\r\ncall \"{}\" >nul\r\nif errorlevel 1 exit /b %errorlevel%\r\ncl.exe /nologo /std:c11 /Od /utf-8 \"{}\" /Fe:\"{}\" /Fo:\"{}\"\r\n",
        vcvars.display(), source.display(), output.display(), directory.join("worker.obj").display()
    )).expect("write owned MSVC fixture build script");
    let mut command = Command::new("cmd.exe");
    command
        .current_dir(directory)
        .args(["/D", "/C"])
        .arg(&script);
    let done = run_bounded(&mut command, Duration::from_secs(90), OUTPUT_LIMITS)
        .expect("bounded MSVC fixture compilation");
    assert!(
        done.status.success(),
        "MSVC fixture failed: {}",
        diagnostics(&done)
    );
    Some(output.to_owned())
}

#[cfg(any(target_os = "linux", windows))]
const WORKER_C: &str = r#"
#define _POSIX_C_SOURCE 200809L
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#ifdef _WIN32
#include <windows.h>
#include <fcntl.h>
#include <io.h>
static void pause_ms(unsigned ms) { Sleep(ms); }
#else
#include <time.h>
#include <unistd.h>
static void pause_ms(unsigned ms) {
    struct timespec delay = { (time_t)(ms / 1000), (long)(ms % 1000) * 1000000L };
    for (unsigned retries = 0; retries < 128; ++retries) {
        if (nanosleep(&delay, &delay) == 0 || errno != EINTR) return;
    }
}
#endif
static int exists(const char *name) {
    FILE *file = fopen(name, "rb");
    if (!file) return 0;
    fclose(file);
    return 1;
}
static int descendant_wait(const char *name) {
    char ready[256];
    if (snprintf(ready, sizeof(ready), "%s.child-ready", name) >= (int)sizeof(ready)) return 78;
    FILE *file = fopen(ready, "wb");
    if (!file) return 79;
    if (fputc('R', file) == EOF || fclose(file) != 0) return 80;
    /* The coordinator's kernel domain and the external test deadline bound
       this intentionally nonterminating descendant, including setsid(). */
    for (;;) pause_ms(1000);
}
static int descendant_is_ready(const char *name) {
    FILE *file = fopen(name, "rb");
    if (!file) return 0;
    int value = fgetc(file);
    fclose(file);
    return value == 'R';
}
int main(int argc, char **argv) {
#ifdef _WIN32
    if (_setmode(_fileno(stdout), _O_BINARY) < 0 || _setmode(_fileno(stderr), _O_BINARY) < 0) return 69;
#endif
    const char *name = argv[0];
    for (const char *p = argv[0]; *p; ++p) if (*p == '/' || *p == '\\') name = p + 1;
    /* A Windows descendant enters before the submission's .runs marker. */
    if (argc == 2 && strcmp(argv[1], "--fixture-descendant") == 0) return descendant_wait(name);
    char marker[256];
    if (snprintf(marker, sizeof(marker), "%s.runs", name) >= (int)sizeof(marker)) return 70;
    FILE *file = fopen(marker, "ab");
    if (!file) return 71;
    if (fputc('1', file) == EOF || fclose(file) != 0) return 72;
    if (strncmp(name, "descendant", 10) == 0) {
        unsigned long child_pid;
#ifdef _WIN32
        char command_line[32768];
        if (snprintf(command_line, sizeof(command_line), "\"%s\" --fixture-descendant", argv[0]) >= (int)sizeof(command_line)) return 81;
        STARTUPINFOA startup = {0};
        PROCESS_INFORMATION process = {0};
        startup.cb = sizeof(startup);
        /* Job membership is inherited by default. Quoting preserves fixture
           paths containing spaces without changing the application path. */
        if (!CreateProcessA(argv[0], command_line, NULL, NULL, TRUE, 0, NULL, NULL, &startup, &process)) return 82;
        child_pid = (unsigned long)process.dwProcessId;
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
#else
        pid_t child = fork();
        if (child < 0) return 82;
        if (child == 0) {
            if (setsid() < 0) return 83;
            return descendant_wait(name);
        }
        child_pid = (unsigned long)child;
#endif
        char ready[256], pid_marker[256];
        if (snprintf(ready, sizeof(ready), "%s.child-ready", name) >= (int)sizeof(ready)) return 84;
        if (snprintf(pid_marker, sizeof(pid_marker), "%s.child-pid", name) >= (int)sizeof(pid_marker)) return 85;
        unsigned attempts = 0;
        while (!descendant_is_ready(ready) && attempts++ < 800) pause_ms(10);
        if (!descendant_is_ready(ready)) return 86;
        file = fopen(pid_marker, "wb");
        if (!file) return 87;
        if (fprintf(file, "%lu\n", child_pid) < 0 || fclose(file) != 0) return 88;
        return 19;
    }
    if (strncmp(name, "exit", 4) == 0) { fputs("requested_failure\n", stderr); return 17; }
    if (strncmp(name, "sleep", 5) == 0) { pause_ms(30000); return 0; }
    if (strncmp(name, "hold-", 5) == 0) {
#ifdef _WIN32
        const char *one = "hold-1.exe.runs", *two = "hold-2.exe.runs";
#else
        const char *one = "hold-1.runs", *two = "hold-2.runs";
#endif
        unsigned attempts = 0;
        while (!(exists(one) && exists(two)) && attempts++ < 800) pause_ms(10);
        if (!(exists(one) && exists(two))) return 73;
        pause_ms(40);
        return 0;
    }
    if (strncmp(name, "flood", 5) == 0) {
        /* Each stream fits 4096; only their combined 6000 bytes exceed it. */
        char bytes[3000]; memset(bytes, 'x', sizeof(bytes));
        if (fwrite(bytes, 1, sizeof(bytes), stdout) != sizeof(bytes)) return 74;
        if (fwrite(bytes, 1, sizeof(bytes), stderr) != sizeof(bytes)) return 74;
        return fflush(stdout) == 0 && fflush(stderr) == 0 ? 0 : 75;
    }
    if (strncmp(name, "memory", 6) == 0) {
        void *allocations[128] = {0};
        for (unsigned i = 0; i < 128; ++i) {
            allocations[i] = malloc(2u * 1024u * 1024u);
            if (!allocations[i]) { puts("ALLOC_DENIED"); return 23; }
            volatile unsigned char *bytes = (volatile unsigned char *)allocations[i];
            for (size_t offset = 0; offset < 2u * 1024u * 1024u; offset += 4096) bytes[offset] = (unsigned char)i;
        }
        for (unsigned i = 0; i < 128; ++i) free(allocations[i]);
        return 76; /* The complete 256 MiB must not fit a 64 MiB worker. */
    }
    const unsigned char binary[] = { 'A', 0, 255, '\n' };
    if (fwrite(binary, 1, sizeof(binary), stdout) != sizeof(binary)) return 77;
    fputs("diag\n", stderr);
    return 0;
}
"#;
