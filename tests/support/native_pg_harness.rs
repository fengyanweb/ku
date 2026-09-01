//! Closed-loop tests for the generated native PostgreSQL connection poller.
//! The harness supplies a deterministic libpq and OS-poll surface, so no live
//! database or libpq development files are required.

#[allow(dead_code)]
#[path = "bounded_process.rs"]
mod bounded_process;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bounded_process::FailureKind;
pub use bounded_process::{run_bounded, OutputLimits};

const BUILD_TIMEOUT: Duration = Duration::from_secs(120);
pub const RUN_TIMEOUT: Duration = Duration::from_secs(20);
const BUILD_LIMITS: OutputLimits = OutputLimits::new(8 * 1024 * 1024, 12 * 1024 * 1024);
pub const RUN_LIMITS: OutputLimits = OutputLimits::new(1024 * 1024, 2 * 1024 * 1024);
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before Unix epoch")
            .as_nanos();
        let sequence = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "ku-native-pg-poll-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create PG poll test directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
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
    let executable = if cfg!(windows) { "ku.exe" } else { "ku" };
    let target = env::var("CARGO_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    [
        target.join("debug").join(executable),
        target.join("release").join(executable),
        repo_root().join("target").join("debug").join(executable),
        repo_root().join("target").join("release").join(executable),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("ku binary not found; set KU_BIN or build it before this test")
}

pub fn emit_c(directory: &Path, source: &str) -> String {
    let source_path = directory.join("main.ku");
    fs::write(&source_path, source).expect("write PG poll Ku fixture");
    let mut command = Command::new(ku_binary());
    command
        .current_dir(directory)
        .args(["build", "--native", "main.ku"]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS)
        .unwrap_or_else(|error| panic!("PG poll C emission was not bounded: {error}"));
    assert!(
        output.status.success(),
        "PG poll C emission failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(directory.join("main.c")).expect("read generated PG poll C")
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

pub fn compile_harness(directory: &Path, source: &Path, stem: &str) -> Option<PathBuf> {
    compile_harness_linked(directory, source, stem, None)
}

pub fn compile_harness_with_libpq(
    directory: &Path,
    source: &Path,
    stem: &str,
    libdir: &Path,
) -> Option<PathBuf> {
    compile_harness_linked(directory, source, stem, Some(libdir))
}

fn compile_harness_linked(
    directory: &Path,
    source: &Path,
    stem: &str,
    libpq_dir: Option<&Path>,
) -> Option<PathBuf> {
    let output = directory.join(executable_name(stem));
    let mut candidates: Vec<(PathBuf, Vec<String>)> = Vec::new();
    if let Ok(spec) = env::var("KU_CC") {
        let configured = PathBuf::from(&spec);
        if configured.exists() {
            candidates.push((configured, Vec::new()));
        } else {
            let mut words = spec.split_whitespace();
            if let Some(program) = words.next() {
                candidates.push((PathBuf::from(program), words.map(str::to_owned).collect()));
            }
        }
    }
    candidates.extend([
        (PathBuf::from("clang"), Vec::new()),
        (PathBuf::from("gcc"), Vec::new()),
        (PathBuf::from("cc"), Vec::new()),
        (PathBuf::from("zig"), vec!["cc".to_string()]),
    ]);

    for (program, prefix) in candidates {
        let mut command = Command::new(&program);
        command.args(prefix).arg(source).arg("-std=c11");
        if cfg!(windows) {
            command.arg("-lws2_32");
        } else {
            command.arg("-pthread");
        }
        if let Some(libdir) = libpq_dir {
            command.arg("-L").arg(libdir).arg("-lpq");
        }
        command.arg("-o").arg(&output);
        match run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS) {
            Ok(done) if done.status.success() => return Some(output),
            Ok(done) => panic!(
                "C compiler '{}' rejected PG poll harness:\n{}{}",
                program.display(),
                String::from_utf8_lossy(&done.stdout),
                String::from_utf8_lossy(&done.stderr)
            ),
            Err(error)
                if error.kind() == FailureKind::Spawn
                    && error.io_error_kind() == Some(std::io::ErrorKind::NotFound) => {}
            Err(error) => panic!("PG poll harness compiler was not bounded: {error}"),
        }
    }

    #[cfg(windows)]
    {
        let program_files = env::var("ProgramFiles(x86)")
            .or_else(|_| env::var("ProgramFiles"))
            .ok()?;
        let vswhere = Path::new(&program_files)
            .join("Microsoft Visual Studio")
            .join("Installer")
            .join("vswhere.exe");
        if !vswhere.exists() {
            eprintln!("skip: no C compiler available for PG poll harness");
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
        let found = run_bounded(&mut discover, RUN_TIMEOUT, BUILD_LIMITS)
            .unwrap_or_else(|error| panic!("Visual Studio discovery was not bounded: {error}"));
        assert!(
            found.status.success(),
            "Visual Studio C++ discovery failed:\n{}{}",
            String::from_utf8_lossy(&found.stdout),
            String::from_utf8_lossy(&found.stderr)
        );
        let install = String::from_utf8_lossy(&found.stdout).trim().to_owned();
        if install.is_empty() {
            eprintln!("skip: Visual Studio C++ toolchain was not found");
            return None;
        }
        let vcvars = Path::new(&install)
            .join("VC")
            .join("Auxiliary")
            .join("Build")
            .join("vcvars64.bat");
        assert!(
            vcvars.is_file(),
            "discovered Visual Studio C++ toolchain is missing its environment script: {}",
            vcvars.display()
        );
        let script = directory.join("compile-pg-poll-harness.bat");
        let object = directory.join(format!("{stem}.obj"));
        let link = libpq_dir
            .map(|libdir| format!(" /link /LIBPATH:\"{}\" libpq.lib", libdir.display()))
            .unwrap_or_default();
        fs::write(
            &script,
            format!(
                "@echo off\r\ncall \"{}\" >nul\r\nif errorlevel 1 exit /b %errorlevel%\r\ncl.exe /nologo /std:c11 /utf-8 \"{}\" /Fe:\"{}\" /Fo:\"{}\"{}\r\n",
                vcvars.display(),
                source.display(),
                output.display(),
                object.display(),
                link
            ),
        )
        .expect("write PG poll harness build script");
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/C"]).arg(&script);
        let built = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS)
            .unwrap_or_else(|error| panic!("MSVC PG poll harness was not bounded: {error}"));
        fs::remove_file(script).ok();
        fs::remove_file(object).ok();
        assert!(
            built.status.success(),
            "MSVC rejected PG poll harness:\n{}{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr)
        );
        Some(output)
    }

    #[cfg(not(windows))]
    {
        eprintln!("skip: no C compiler available for PG poll harness");
        None
    }
}
