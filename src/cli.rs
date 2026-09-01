use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[cfg(test)]
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::{
    ast::*,
    backend,
    checker::Checker,
    error::{KuError, KuResult},
    interpreter::Interpreter,
    ir,
    lexer::Lexer,
    package::{self, DependencyResolveMode, PackageContext},
    parser::Parser,
    span::Span,
    stdlib,
};

const KU_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SOURCE_BYTES: u64 = 1_000_000;
const MAX_IMPORT_MODULES: usize = 4_096;
const MAX_IMPORT_DEPTH: usize = 32;
const MAX_IMPORT_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMPORT_EXPANDED_ITEMS: usize = 65_536;
const MAX_IMPORT_CLONED_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMPORT_EDGES: usize = 16_384;
const MAX_IMPORT_BINDINGS: usize = 16_384;
const MAX_RLIB_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES: usize = 128;
const MAX_PINNED_LINK_LIBRARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_NATIVE_LINK_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMPORT_LIBRARY_INSPECTION_BYTES: u64 = 64 * 1024 * 1024;
const BUILD_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
const C_COMPILER_PROCESS_TIMEOUT: Duration = Duration::from_secs(120);
const NATIVE_LINK_TOTAL_TIMEOUT: Duration = Duration::from_secs(300);
const RUSTC_PROCESS_TIMEOUT: Duration = Duration::from_secs(300);
const BUILD_TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const COMPILER_TARGET_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const BUILD_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const BUILD_PROCESS_CLEANUP_GRACE: Duration = Duration::from_secs(2);
#[cfg(windows)]
const WINDOWS_BUILD_THREAD_SCAN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const MAX_WINDOWS_BUILD_THREAD_SCAN_ENTRIES: usize = 65_536;
const MAX_BUILD_TOOL_CAPTURE_BYTES: usize = 1024 * 1024;
const MAX_COMPILER_TARGET_BYTES: usize = 4 * 1024;
const BUILD_PROCESS_CLEANUP_UNCONFIRMED: &str = "build subprocess cleanup could not be confirmed";
// Large enough that the MAX_CALL_DEPTH=512 guard trips (a clean Ku runtime
// error) before the interpreter's own Rust eval recursion overflows the OS
// thread stack. ~16KB of Rust stack per Ku call frame; 64MB clears 512 with
// wide margin. Reserved address space on Windows, not committed memory.
const INTERPRETER_STACK_SIZE: usize = 64 * 1024 * 1024;

fn remaining_build_phase_timeout(
    deadline: Instant,
    phase_limit: Duration,
    phase: &str,
) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining == Duration::ZERO {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("native link total deadline expired before {phase}"),
        ));
    }
    Ok(remaining.min(phase_limit))
}

fn run_build_process_bounded(command: &mut Command, timeout: Duration) -> io::Result<ExitStatus> {
    if timeout == Duration::ZERO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "build subprocess timeout must be positive",
        ));
    }
    let command_text = format!("{command:?}");
    command.stdin(Stdio::null());
    let (mut child, process_tree) = spawn_contained_build_process(command)?;
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Err(detail) = process_tree.terminate_descendants() {
                    return Err(io::Error::other(format!(
                        "{BUILD_PROCESS_CLEANUP_UNCONFIRMED}: {detail}"
                    )));
                }
                return Ok(status);
            }
            Ok(None) => {}
            Err(error) => {
                let cleanup = merge_build_cleanup(
                    process_tree.terminate(&mut child),
                    reap_build_process(&mut child),
                );
                return Err(with_build_cleanup_error(error, cleanup));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            let cleanup = merge_build_cleanup(
                process_tree.terminate(&mut child),
                reap_build_process(&mut child),
            );
            let cleanup = build_cleanup_suffix(cleanup);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("build subprocess exceeded {timeout:?}: {command_text}{cleanup}"),
            ));
        }
        thread::sleep(BUILD_PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn reap_build_process(child: &mut std::process::Child) -> Result<(), String> {
    let started = Instant::now();
    let deadline = started
        .checked_add(BUILD_PROCESS_CLEANUP_GRACE)
        .unwrap_or(started);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Err(error) => return Err(error.to_string()),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(BUILD_PROCESS_POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                return match child.try_wait() {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) => Err("child was still running after the cleanup grace".to_string()),
                    Err(error) => Err(error.to_string()),
                };
            }
        }
    }
}

fn build_cleanup_suffix(cleanup: Result<(), String>) -> String {
    cleanup.map_or_else(
        |detail| format!("; {BUILD_PROCESS_CLEANUP_UNCONFIRMED}: {detail}"),
        |()| String::new(),
    )
}

fn with_build_cleanup_error(error: io::Error, cleanup: Result<(), String>) -> io::Error {
    match cleanup {
        Ok(()) => error,
        Err(detail) => io::Error::new(
            error.kind(),
            format!("{error}; {BUILD_PROCESS_CLEANUP_UNCONFIRMED}: {detail}"),
        ),
    }
}

fn build_cleanup_is_unconfirmed(error: &io::Error) -> bool {
    error
        .to_string()
        .contains(BUILD_PROCESS_CLEANUP_UNCONFIRMED)
}

fn unique_build_tool_path(label: &str, extension: &str) -> io::Result<PathBuf> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| io::Error::other(format!("secure random failed: {error}")))?;
    let nonce = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(env::temp_dir().join(format!(
        ".ku-{label}-{}-{nonce}.{extension}",
        std::process::id()
    )))
}

struct BuildTemporaryPath {
    path: PathBuf,
    armed: bool,
}

impl BuildTemporaryPath {
    fn new(label: &str, extension: &str) -> io::Result<Self> {
        Ok(Self {
            path: unique_build_tool_path(label, extension)?,
            armed: false,
        })
    }

    fn as_path(&self) -> &Path {
        &self.path
    }

    fn arm(&mut self) {
        self.armed = true;
    }
}

impl Drop for BuildTemporaryPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn run_build_process_capture_stdout(
    command: &mut Command,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<(ExitStatus, Vec<u8>)> {
    if timeout == Duration::ZERO || max_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "captured build subprocess requires positive timeout and output limit",
        ));
    }
    let command_text = format!("{command:?}");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (mut child, process_tree) = spawn_contained_build_process(command)?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let cleanup = merge_build_cleanup(
                process_tree.terminate(&mut child),
                reap_build_process(&mut child),
            );
            return Err(with_build_cleanup_error(
                io::Error::other("captured build subprocess did not expose its stdout pipe"),
                cleanup,
            ));
        }
    };
    let (capture_rx, output_limit_exceeded) =
        match spawn_bounded_build_output_reader(stdout, max_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                let cleanup = merge_build_cleanup(
                    process_tree.terminate(&mut child),
                    reap_build_process(&mut child),
                );
                return Err(with_build_cleanup_error(error, cleanup));
            }
        };
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    loop {
        if output_limit_exceeded.load(Ordering::Acquire) {
            let cleanup = merge_build_cleanup(
                merge_build_cleanup(
                    process_tree.terminate(&mut child),
                    reap_build_process(&mut child),
                ),
                receive_bounded_build_output(&capture_rx).map(|_| ()),
            );
            let cleanup = build_cleanup_suffix(cleanup);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("build tool output exceeded {max_bytes} bytes: {command_text}{cleanup}"),
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let descendants = process_tree.terminate_descendants();
                let captured = receive_bounded_build_output(&capture_rx);
                if let Err(detail) = merge_build_cleanup(
                    descendants,
                    captured.as_ref().map(|_| ()).map_err(Clone::clone),
                ) {
                    return Err(io::Error::other(format!(
                        "{BUILD_PROCESS_CLEANUP_UNCONFIRMED}: {detail}"
                    )));
                }
                let captured = captured.expect("successful capture cleanup has output");
                if captured.exceeded_limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("build tool output exceeded {max_bytes} bytes: {command_text}"),
                    ));
                }
                return Ok((status, captured.bytes));
            }
            Ok(None) => {}
            Err(error) => {
                let cleanup = merge_build_cleanup(
                    merge_build_cleanup(
                        process_tree.terminate(&mut child),
                        reap_build_process(&mut child),
                    ),
                    receive_bounded_build_output(&capture_rx).map(|_| ()),
                );
                return Err(with_build_cleanup_error(error, cleanup));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            let cleanup = merge_build_cleanup(
                merge_build_cleanup(
                    process_tree.terminate(&mut child),
                    reap_build_process(&mut child),
                ),
                receive_bounded_build_output(&capture_rx).map(|_| ()),
            );
            let cleanup = build_cleanup_suffix(cleanup);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("captured build subprocess exceeded {timeout:?}: {command_text}{cleanup}"),
            ));
        }
        thread::sleep(BUILD_PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

struct BoundedBuildOutput {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

fn spawn_bounded_build_output_reader(
    mut stdout: std::process::ChildStdout,
    max_bytes: usize,
) -> io::Result<(Receiver<io::Result<BoundedBuildOutput>>, Arc<AtomicBool>)> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let exceeded_limit = Arc::new(AtomicBool::new(false));
    let reader_exceeded_limit = Arc::clone(&exceeded_limit);
    thread::Builder::new()
        .name("ku-build-output".to_string())
        .spawn(move || {
            let result = (|| -> io::Result<BoundedBuildOutput> {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve(max_bytes.min(8 * 1024))
                    .map_err(|error| {
                        io::Error::other(format!("failed to reserve build output buffer: {error}"))
                    })?;
                let mut buffer = [0_u8; 8 * 1024];
                let mut exceeded = false;
                loop {
                    let read = stdout.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    let remaining = max_bytes.saturating_sub(bytes.len());
                    let kept = remaining.min(read);
                    bytes.try_reserve(kept).map_err(|error| {
                        io::Error::other(format!("failed to grow build output buffer: {error}"))
                    })?;
                    bytes.extend_from_slice(&buffer[..kept]);
                    if kept < read {
                        exceeded = true;
                        reader_exceeded_limit.store(true, Ordering::Release);
                    }
                }
                Ok(BoundedBuildOutput {
                    bytes,
                    exceeded_limit: exceeded,
                })
            })();
            let _ = sender.send(result);
        })?;
    Ok((receiver, exceeded_limit))
}

fn receive_bounded_build_output(
    receiver: &Receiver<io::Result<BoundedBuildOutput>>,
) -> Result<BoundedBuildOutput, String> {
    match receiver.recv_timeout(BUILD_PROCESS_CLEANUP_GRACE) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(format!("stdout reader failed: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("stdout reader was still blocked after the cleanup grace".to_string())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("stdout reader stopped without returning output".to_string())
        }
    }
}

fn merge_build_cleanup(
    first: Result<(), String>,
    second: Result<(), String>,
) -> Result<(), String> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(format!("{first}; {second}")),
    }
}

fn terminate_direct_build_child(child: &mut std::process::Child) -> Result<(), String> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(format!("failed to terminate build child: {kill_error}")),
            Err(wait_error) => Err(format!(
                "failed to terminate build child: {kill_error}; status check failed: {wait_error}"
            )),
        },
    }
}

fn spawn_contained_build_process(
    command: &mut Command,
) -> io::Result<(std::process::Child, BuildProcessTree)> {
    #[cfg(all(test, windows))]
    let delay_before_job_attach = command.get_envs().any(|(name, value)| {
        name == "KU_TEST_DELAY_BUILD_JOB_ATTACH" && value.is_some_and(|value| value == "1")
    });
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        // std continues to own argument quoting, environment, cwd, and stdio;
        // the primary thread cannot execute compiler code before Job assignment.
        command.creation_flags(CREATE_SUSPENDED);
    }

    let mut child = command.spawn()?;
    #[cfg(all(test, windows))]
    if delay_before_job_attach {
        // A deterministic regression window: CREATE_SUSPENDED must prevent the
        // fixture from running even though Job assignment is deliberately late.
        thread::sleep(Duration::from_millis(150));
    }
    match BuildProcessTree::attach(&child) {
        Ok(process_tree) => {
            #[cfg(windows)]
            if let Err(resume_error) = process_tree.resume_suspended_child(&child) {
                let cleanup = merge_build_cleanup(
                    process_tree.terminate(&mut child),
                    reap_build_process(&mut child),
                );
                return Err(io::Error::other(format!(
                    "build subprocess containment failed while resuming its assigned thread: {resume_error}{}",
                    build_cleanup_suffix(cleanup)
                )));
            }
            Ok((child, process_tree))
        }
        Err(attach_error) => {
            let cleanup = merge_build_cleanup(
                terminate_direct_build_child(&mut child),
                reap_build_process(&mut child),
            );
            Err(io::Error::other(format!(
                "build subprocess containment failed: {attach_error}{}",
                build_cleanup_suffix(cleanup)
            )))
        }
    }
}

#[cfg(unix)]
struct BuildProcessTree {
    process_group: Option<i32>,
}

#[cfg(unix)]
impl BuildProcessTree {
    fn attach(child: &std::process::Child) -> Result<Self, String> {
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "child process group could not be represented".to_string())?;
        Ok(Self {
            process_group: Some(process_group),
        })
    }

    fn terminate(&self, child: &mut std::process::Child) -> Result<(), String> {
        merge_build_cleanup(
            self.terminate_descendants(),
            terminate_direct_build_child(child),
        )
    }

    fn terminate_descendants(&self) -> Result<(), String> {
        let process_group = self
            .process_group
            .ok_or_else(|| "child process group could not be represented".to_string())?;
        // SAFETY: the child was placed in its own process group and the
        // validated negative pid targets that group only.
        let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!("failed to terminate build process group: {error}"))
        }
    }
}

#[cfg(windows)]
struct BuildProcessTree {
    job: Option<BuildWindowsJob>,
    attach_error: Option<String>,
}

#[cfg(windows)]
impl BuildProcessTree {
    fn attach(child: &std::process::Child) -> Result<Self, String> {
        BuildWindowsJob::attach(child).map(|job| Self {
            job: Some(job),
            attach_error: None,
        })
    }

    fn terminate(&self, child: &mut std::process::Child) -> Result<(), String> {
        merge_build_cleanup(
            self.terminate_descendants(),
            terminate_direct_build_child(child),
        )
    }

    fn resume_suspended_child(&self, child: &std::process::Child) -> Result<(), String> {
        self.job
            .as_ref()
            .ok_or_else(|| {
                "Windows Job assignment was unavailable before compiler resume".to_string()
            })?
            .resume_suspended_child(child.id())
    }

    fn terminate_descendants(&self) -> Result<(), String> {
        if let Some(job) = &self.job {
            job.terminate()
        } else {
            Err(self.attach_error.clone().unwrap_or_else(|| {
                "Windows Job assignment was unavailable, so descendant cleanup is unconfirmed"
                    .to_string()
            }))
        }
    }
}

#[cfg(windows)]
struct BuildWindowsJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
struct BuildWindowsHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for BuildWindowsHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: each guard is created only for one newly owned Windows handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
impl BuildWindowsJob {
    fn attach(child: &std::process::Child) -> Result<Self, String> {
        use std::mem::{size_of, zeroed};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let information_size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| "Windows Job information size overflowed u32".to_string())?;
        // SAFETY: every pointer is null or points to a correctly sized value
        // for the duration of the call. The owned handle closes on every path.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed: {}",
                    io::Error::last_os_error()
                ));
            }
            let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                information_size,
            ) == 0
            {
                let error = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(format!("SetInformationJobObject failed: {error}"));
            }
            if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                let error = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(format!("AssignProcessToJobObject failed: {error}"));
            }
            Ok(Self { handle })
        }
    }

    fn terminate(&self) -> Result<(), String> {
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject, TerminateJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };
        // SAFETY: this value owns a valid job handle until Drop.
        let result = unsafe { TerminateJobObject(self.handle, 1) };
        if result == 0 {
            return Err(format!(
                "TerminateJobObject failed: {}",
                io::Error::last_os_error()
            ));
        }
        let information_size =
            u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                .map_err(|_| "Windows Job accounting size overflowed u32".to_string())?;
        let started = Instant::now();
        let deadline = started
            .checked_add(BUILD_PROCESS_CLEANUP_GRACE)
            .unwrap_or(started);
        loop {
            let mut information: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            // SAFETY: the buffer matches the requested Job information class.
            let queried = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    information_size,
                    std::ptr::null_mut(),
                )
            };
            if queried == 0 {
                return Err(format!(
                    "QueryInformationJobObject failed after termination: {}",
                    io::Error::last_os_error()
                ));
            }
            if information.ActiveProcesses == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "Windows Job still reported {} active build processes after termination",
                    information.ActiveProcesses
                ));
            }
            thread::sleep(BUILD_PROCESS_POLL_INTERVAL);
        }
    }

    fn resume_suspended_child(&self, process_id: u32) -> Result<(), String> {
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            GetProcessIdOfThread, OpenThread, ResumeThread, THREAD_QUERY_LIMITED_INFORMATION,
            THREAD_SUSPEND_RESUME,
        };

        let entry_size = u32::try_from(size_of::<THREADENTRY32>())
            .map_err(|_| "Windows thread snapshot entry size overflowed u32".to_string())?;
        let scan_started = Instant::now();
        let scan_deadline = scan_started
            .checked_add(WINDOWS_BUILD_THREAD_SCAN_TIMEOUT)
            .unwrap_or(scan_started);
        // ToolHelp is synchronous and Windows exposes no cancellation handle
        // for these calls. Bound the entry count and fail closed after every
        // syscall returns if total observed elapsed time exceeded the deadline;
        // this does not claim to preempt a kernel call that itself stalls.
        // SAFETY: this creates a new snapshot handle, owned by the guard below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateToolhelp32Snapshot failed before compiler resume: {}",
                io::Error::last_os_error()
            ));
        }
        if Instant::now() >= scan_deadline {
            return Err("Windows thread snapshot creation exceeded its 5 second bound".to_string());
        }
        let snapshot = BuildWindowsHandle(snapshot);
        let mut entry: THREADENTRY32 = unsafe { zeroed() };
        entry.dwSize = entry_size;
        // SAFETY: the snapshot is valid and entry has the required size.
        if unsafe { Thread32First(snapshot.0, &mut entry) } == 0 {
            return Err(format!(
                "Thread32First failed before compiler resume: {}",
                io::Error::last_os_error()
            ));
        }
        if Instant::now() >= scan_deadline {
            return Err("Windows first thread lookup exceeded its 5 second bound".to_string());
        }
        let mut thread_id = None;
        let mut scanned_entries = 0_usize;
        loop {
            scanned_entries += 1;
            if scanned_entries > MAX_WINDOWS_BUILD_THREAD_SCAN_ENTRIES
                || Instant::now() >= scan_deadline
            {
                return Err(format!(
                    "suspended compiler thread lookup exceeded its {} entry / {} second bound",
                    MAX_WINDOWS_BUILD_THREAD_SCAN_ENTRIES,
                    WINDOWS_BUILD_THREAD_SCAN_TIMEOUT.as_secs()
                ));
            }
            if entry.th32OwnerProcessID == process_id
                && thread_id.replace(entry.th32ThreadID).is_some()
            {
                return Err(
                    "suspended compiler unexpectedly exposed multiple threads before Job-contained resume"
                        .to_string(),
                );
            }
            entry.dwSize = entry_size;
            // SAFETY: the snapshot remains valid and entry size is reset for
            // every iteration as required by ToolHelp.
            let next = unsafe { Thread32Next(snapshot.0, &mut entry) };
            if Instant::now() >= scan_deadline {
                return Err("Windows thread enumeration exceeded its 5 second bound".to_string());
            }
            if next == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    break;
                }
                return Err(format!(
                    "Thread32Next failed before compiler resume: {error}"
                ));
            }
        }
        let thread_id = thread_id.ok_or_else(|| {
            "suspended compiler thread was absent from the bounded ToolHelp snapshot".to_string()
        })?;
        // SAFETY: OpenThread validates the snapshot-provided id. The handle is
        // closed by the guard on every path.
        let thread = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                0,
                thread_id,
            )
        };
        if thread.is_null() {
            return Err(format!(
                "OpenThread failed before compiler resume: {}",
                io::Error::last_os_error()
            ));
        }
        let thread = BuildWindowsHandle(thread);
        if Instant::now() >= scan_deadline {
            return Err("Windows compiler thread open exceeded its 5 second bound".to_string());
        }
        // SAFETY: the opened handle has query access. Rechecking the owner
        // narrows the snapshot-to-open thread-id reuse window.
        let owner = unsafe { GetProcessIdOfThread(thread.0) };
        if Instant::now() >= scan_deadline {
            return Err(
                "Windows compiler thread owner verification exceeded its 5 second bound"
                    .to_string(),
            );
        }
        if owner != process_id {
            return Err(format!(
                "suspended compiler thread owner changed before resume (expected {process_id}, got {owner})"
            ));
        }
        // SAFETY: the thread handle has suspend/resume access. CREATE_SUSPENDED
        // establishes an exact initial suspend count of one for the primary
        // thread; every other result fails closed and the Job is terminated.
        let previous_count = unsafe { ResumeThread(thread.0) };
        if Instant::now() >= scan_deadline {
            return Err("Windows compiler thread resume exceeded its 5 second bound".to_string());
        }
        if previous_count != 1 {
            return Err(format!(
                "ResumeThread returned unexpected suspend count {previous_count}"
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for BuildWindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: the handle is owned here and closed exactly once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct BuildProcessTree;

#[cfg(not(any(unix, windows)))]
impl BuildProcessTree {
    fn attach(_child: &std::process::Child) -> Result<Self, String> {
        Err("process-tree containment is unavailable on this host".to_string())
    }

    fn terminate(&self, child: &mut std::process::Child) -> Result<(), String> {
        merge_build_cleanup(
            self.terminate_descendants(),
            terminate_direct_build_child(child),
        )
    }

    fn terminate_descendants(&self) -> Result<(), String> {
        Err("process-tree containment is unavailable on this host".to_string())
    }
}
const HELP: &str = "\
ku - simple, small, fast language tool

Usage:
  ku <file.ku>          Run a Ku source file
  ku create <name>      Create a new Ku project directory
  ku create <name> --template <template>
                        Create a project from a built-in template
  ku create --list      List built-in project templates
  ku init               Initialize the current directory as a Ku project
  ku init --template <template>
                        Initialize the current directory from a template
  ku template list      List built-in project templates
  ku run [--locked|--offline] [file.ku]
                        Run a package entry or Ku source file
  ku check [--locked|--offline] [file.ku]
                        Check a package entry or Ku source file without running it
  ku check --deny-unused [file.ku]
                        Treat unused local bindings as errors
  ku check --json       Check nearest ku.mod package and emit JSON Lines diagnostics
  ku check --json [--deny-unused] <file.ku>
                        Check and emit JSON Lines diagnostics
  ku ir <file.ku>       Print checked Ku IR draft
  ku llvm <file.ku>     Emit prototype LLVM text IR
  ku build [--locked|--offline] [file.ku]
                        Build a runnable executable package
  ku build .            Build the nearest ku.mod package
  ku build -o <path> [file.ku]
                        Build to an explicit executable path
  ku build --release [file.ku]
                        Build with release profile
  ku build --profile <debug|release|small|fast> [file.ku]
  ku build --emit-c [file.ku]
                        Also emit prototype native C source under .ku/build
  ku build --emit-ir [file.ku]
                        Also emit checked Ku IR draft under .ku/build
  ku build --backend c [--target <target>] [file.ku]
                        Build one native binary for host, x86_64-linux,
                        x86_64-windows, or aarch64-darwin
  ku build --native [--locked|--offline] <file.ku>
                        Compatibility form: emit prototype native C source beside file
  ku package gc [path]
                        Remove unused package cache entries for a package
  ku package pack [path]
                        Create a deterministic source package artifact
  ku package publish [path]
                        Publish through the configured HTTPS registry
  ku package yank [path]
                        Withdraw one published version without deleting its artifact
  ku package resolve [path] [--locked|--offline]
                        Resolve and cache the complete dependency graph
  ku version            Print version
  ku -v                 Print version
  ku -h                 Print this help
  ku -help              Print this help
  ku --help             Print this help
  ku help               Print this help

Examples:
  ku create hello
  ku create HelloWorld --template http
  ku init --template cli
  ku template list
  ku run
  ku run examples\\hello.ku
  ku check
  ku check examples\\error.ku
  ku ir examples\\function.ku
  ku llvm examples\\function.ku
  ku build examples\\hello.ku
  ku build --release -o dist\\hello.exe examples\\hello.ku
  ku build --backend c --release --target x86_64-linux .
  ku build --native examples\\function.ku
  ku package gc .
  ku package pack .
  ku package publish .
  ku package yank .
  ku package resolve . --locked
";

pub fn run_cli(args: Vec<String>) -> Result<(), KuError> {
    match args.get(1).map(String::as_str) {
        Some("create") => run_create_command(&args),
        Some("init") => run_init_command(&args),
        Some("template") => run_template_command(&args),
        Some("run") => {
            if args.get(2).is_some_and(|arg| arg == "build") {
                return Err(command_error(
                    "`ku run build` was removed; use the single build command `ku build`",
                ));
            }
            let (path, source, dependency_mode) =
                source_arg_or_project_with_dependency_mode(&args, "run")?;
            run_source_with_dependency_mode(&path_string(&path), &source, dependency_mode)
        }
        Some("check") => run_check_command(&args),
        Some("ir") => {
            let path = exact_path(&args, "ir")?;
            let source = read_ku_file(path)?;
            let program = parse_and_check(path, &source)?;
            let lowered = ir::lower_program(&program)?;
            print!("{}", ir::optimize_program(&lowered));
            Ok(())
        }
        Some("llvm") => {
            let path = exact_path(&args, "llvm")?;
            let source = read_ku_file(path)?;
            let output = build_llvm_ir(path, &source)?;
            println!("llvm ir ok: {}", output.display());
            Ok(())
        }
        Some("build") => {
            if args.get(2).is_some_and(|arg| arg == "--native") {
                // With -o/--output, `ku build --native <file> -o <out>` produces a
                // standalone native binary (identical to `--backend c`). Without an
                // output path it stays the compatibility form that only emits C.
                let wants_binary = args
                    .iter()
                    .skip(3)
                    .any(|arg| arg == "-o" || arg == "--output");
                if wants_binary {
                    let mut rewritten = vec![
                        args[0].clone(),
                        "build".to_string(),
                        "--backend".to_string(),
                        "c".to_string(),
                    ];
                    rewritten.extend(args.iter().skip(3).cloned());
                    run_build_command(&rewritten)?;
                } else {
                    let (path, dependency_mode) = parse_native_compat_args(&args)?;
                    let source = read_ku_path(&path)?;
                    let path = path_string(&path);
                    let output =
                        build_native_c_with_dependency_mode(&path, &source, dependency_mode)?;
                    println!("native c ok: {}", output.display());
                }
            } else {
                run_build_command(&args)?;
            }
            Ok(())
        }
        Some("package") => {
            let subcommand = args
                .get(2)
                .map(String::as_str)
                .ok_or_else(|| command_error("missing package command"))?;
            match subcommand {
                "gc" => {
                    let package = package_context_arg(&args, "package gc")?;
                    let removed = package::gc_cache(&package, 64)?;
                    println!("package gc ok: removed {removed} cache entries");
                    Ok(())
                }
                "pack" => {
                    let package = package_context_arg(&args, "package pack")?;
                    let artifact = package::pack_package(&package)?;
                    println!(
                        "package pack ok: {} {} {} bytes",
                        artifact.path.display(),
                        artifact.checksum,
                        artifact.size
                    );
                    Ok(())
                }
                "publish" => {
                    let package = package_context_arg(&args, "package publish")?;
                    let token = registry_token()?;
                    let receipt = package::publish_package(&package, &token)?;
                    println!("{}", package_publish_success_message(&receipt));
                    Ok(())
                }
                "yank" => {
                    let package = package_context_arg(&args, "package yank")?;
                    let token = registry_token()?;
                    let receipt = package::yank_package(&package, &token)?;
                    println!("{}", package_yank_success_message(&receipt));
                    Ok(())
                }
                "resolve" => {
                    let mut path = None::<PathBuf>;
                    let mut mode = package::DependencyResolveMode::Refresh;
                    for arg in args.iter().skip(3) {
                        match arg.as_str() {
                            "--locked" if mode == package::DependencyResolveMode::Refresh => {
                                mode = package::DependencyResolveMode::Locked;
                            }
                            "--offline" if mode == package::DependencyResolveMode::Refresh => {
                                mode = package::DependencyResolveMode::Offline;
                            }
                            value if value.starts_with('-') => {
                                return Err(command_error(format!(
                                    "unknown or conflicting package resolve option '{value}'"
                                )));
                            }
                            value if path.is_none() => path = Some(PathBuf::from(value)),
                            _ => {
                                return Err(command_error(
                                    "too many paths for 'ku package resolve'",
                                ));
                            }
                        }
                    }
                    let mut package = package_context_from_path(
                        path.as_deref().unwrap_or_else(|| Path::new(".")),
                    )?;
                    let deadline = package::package_operation_deadline();
                    let _usage_lease =
                        package::acquire_package_usage_lease_until(&package, deadline)?;
                    package::resolve_remote_dependencies_with_mode_until(
                        &mut package,
                        mode,
                        deadline,
                    )?;
                    if mode == package::DependencyResolveMode::Refresh {
                        package::write_lock(&package)?;
                    }
                    println!(
                        "package resolve ok: {} registry packages",
                        package.resolved_registry_dependencies.len()
                    );
                    Ok(())
                }
                _ => Err(command_error(format!(
                    "unknown package command '{subcommand}'"
                ))),
            }
        }
        Some("version") | Some("--version") | Some("-V") | Some("-v") => {
            reject_extra_args(&args, 2, "version")?;
            println!("ku {KU_VERSION}");
            Ok(())
        }
        Some("-h") | Some("-help") | Some("--help") | Some("help") => {
            reject_extra_args(&args, 2, "help")?;
            println!("{HELP}");
            Ok(())
        }
        Some(path) if is_ku_file(path) => {
            reject_extra_args(&args, 2, "run")?;
            let source = read_ku_file(path)?;
            run_source(path, &source)
        }
        Some(path) if looks_like_file_path(path) => {
            Err(command_error(expected_ku_file_message(path)))
        }
        Some(command) => Err(command_error(format!("unknown command '{command}'"))),
        None => Err(command_error("missing command")),
    }
}

pub(crate) fn package_publish_success_message(receipt: &package::PackagePublishReceipt) -> String {
    format!(
        "package publish ok: {}@{} {} {}",
        receipt.name, receipt.version, receipt.checksum, receipt.registry
    )
}

pub(crate) fn package_yank_success_message(receipt: &package::PackageYankReceipt) -> String {
    format!(
        "package yank ok: {}@{} {}",
        receipt.name, receipt.version, receipt.registry
    )
}

pub fn help_text() -> &'static str {
    HELP
}

fn registry_token() -> KuResult<String> {
    env::var(package::REGISTRY_TOKEN_ENV).map_err(|_| {
        command_error(format!(
            "missing or non-UTF-8 {} environment variable",
            package::REGISTRY_TOKEN_ENV
        ))
    })
}

fn package_context_arg(args: &[String], command: &str) -> KuResult<PackageContext> {
    if args.len() > 4 {
        return Err(command_error(format!(
            "too many arguments for 'ku {command}'"
        )));
    }
    let path = args
        .get(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    package_context_from_path(&path)
}

fn package_context_from_path(path: &Path) -> KuResult<PackageContext> {
    if !path.exists() {
        return Err(command_error(format!(
            "package path does not exist: '{}'",
            path.display()
        )));
    }
    let package = if path.is_dir() {
        package::discover_from_dir(path)?
    } else {
        package::discover_for_file(path)?
    };
    package.ok_or_else(|| KuError::message(format!("no ku.mod found for '{}'", path.display())))
}

#[derive(Debug, Clone, Copy)]
struct CheckOptions {
    deny_unused: bool,
    dependency_mode: DependencyResolveMode,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            deny_unused: false,
            dependency_mode: DependencyResolveMode::Update,
        }
    }
}

fn run_check_command(args: &[String]) -> Result<(), KuError> {
    let mut json = false;
    let mut options = CheckOptions::default();
    let mut selected_dependency_mode = None;
    let mut path = None::<String>;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--json" => json = true,
            "--deny-unused" => options.deny_unused = true,
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, "check")?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!("unknown check option '{value}'")));
            }
            value => {
                if path.is_some() {
                    return Err(command_error("too many arguments for 'ku check'"));
                }
                path = Some(value.to_string());
            }
        }
    }
    options.dependency_mode = selected_dependency_mode.unwrap_or(DependencyResolveMode::Update);

    let (path, source) = match path {
        Some(path) => {
            let path = PathBuf::from(path);
            let source = read_ku_path(&path).map_err(|err| {
                if json {
                    KuError::message(diagnostic_json_line(&err, &path_string(&path), ""))
                } else {
                    err
                }
            })?;
            (path, source)
        }
        None => project_entry_source(if json { "check --json" } else { "check" })?,
    };
    let path_text = path_string(&path);
    if json {
        parse_and_check_with_options(&path_text, &source, options)
            .map(|_| ())
            .map_err(|err| KuError::message(diagnostic_json_line(&err, &path_text, &source)))
    } else {
        check_source_with_options(&path_text, &source, options)?;
        println!("check ok: {path_text}");
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ProjectTemplate {
    name: &'static str,
    description: &'static str,
    is_lib: bool,
}

const PROJECT_TEMPLATES: &[ProjectTemplate] = &[
    ProjectTemplate {
        name: "basic",
        description: "minimal Ku project",
        is_lib: false,
    },
    ProjectTemplate {
        name: "cli",
        description: "command line tool",
        is_lib: false,
    },
    ProjectTemplate {
        name: "http",
        description: "HTTP server",
        is_lib: false,
    },
    ProjectTemplate {
        name: "json",
        description: "JSON processing example",
        is_lib: false,
    },
    ProjectTemplate {
        name: "fs",
        description: "file processing example",
        is_lib: false,
    },
    ProjectTemplate {
        name: "lib",
        description: "library project",
        is_lib: true,
    },
];

fn run_create_command(args: &[String]) -> Result<(), KuError> {
    if args.len() == 3 && args[2] == "--list" {
        return list_project_templates();
    }
    if args.len() < 3 {
        return Err(project_command_error(
            "create needs a project name",
            "help: use `ku create hello` or `ku create my-api --template http`",
        ));
    }
    let name = &args[2];
    let template = parse_template_option(args, 3, "create")?;
    validate_project_name(name)?;
    let path = PathBuf::from(name);
    if path.exists() {
        return Err(project_command_error(
            format!(
                "error[E1001]: project directory already exists\n   |\n   | ku create {name}\n   |           {}\n   |",
                "^".repeat(name.len().max(1))
            ),
            "help: choose another name, or use `ku init` inside the existing directory",
        ));
    }
    write_project_template(&path, name, template)?;
    println!("create ok: {}", path.display());
    println!("next: cd {name} && ku run");
    Ok(())
}

fn run_init_command(args: &[String]) -> Result<(), KuError> {
    let template = parse_template_option(args, 2, "init")?;
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let manifest = cwd.join("ku.mod");
    if manifest.exists() {
        return Err(project_command_error(
            "error[E1002]: Ku project already exists\n   |\nnote: found `ku.mod` in current directory",
            "help: use `ku run`, `ku build`, or remove `ku.mod` before running `ku init`",
        ));
    }
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_valid_project_name(name))
        .unwrap_or("ku_app");
    write_project_template(&cwd, name, template)?;
    println!("init ok: {}", cwd.display());
    println!("next: ku run");
    Ok(())
}

fn run_template_command(args: &[String]) -> Result<(), KuError> {
    match args.get(2).map(String::as_str) {
        Some("list") => {
            reject_extra_args(args, 3, "template list")?;
            list_project_templates()
        }
        Some(other) => Err(project_command_error(
            format!("unknown template command '{other}'"),
            "help: use `ku template list`",
        )),
        None => Err(project_command_error(
            "missing template command",
            "help: use `ku template list`",
        )),
    }
}

fn parse_template_option<'a>(
    args: &'a [String],
    mut index: usize,
    command: &str,
) -> Result<&'a ProjectTemplate, KuError> {
    let mut template = "basic";
    while index < args.len() {
        match args[index].as_str() {
            "--template" | "-t" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(project_command_error(
                        format!("missing template after {}", args[index - 1]),
                        format!("help: use `ku {command} --template http`"),
                    ));
                };
                template = value;
            }
            value if command == "create" && value == "--list" => {
                return Err(project_command_error(
                    "`ku create --list` does not take a project name",
                    "help: use `ku create --list` or `ku create <name> --template http`",
                ));
            }
            value => {
                return Err(project_command_error(
                    format!("unknown {command} option '{value}'"),
                    format!("help: use `ku {command} --template http`"),
                ));
            }
        }
        index += 1;
    }
    find_project_template(template).ok_or_else(|| {
        project_command_error(
            format!("error[E1003]: unknown template `{template}`\n   |"),
            "help: available templates: basic, cli, http, json, fs, lib",
        )
    })
}

fn find_project_template(name: &str) -> Option<&'static ProjectTemplate> {
    PROJECT_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}

fn list_project_templates() -> Result<(), KuError> {
    for template in PROJECT_TEMPLATES {
        println!("{:<8} {}", template.name, template.description);
    }
    Ok(())
}

fn write_project_template(
    path: &Path,
    name: &str,
    template: &ProjectTemplate,
) -> Result<(), KuError> {
    let manifest_path = path.join("ku.mod");
    let main_path = path.join("src").join("main.ku");
    if manifest_path.exists() || main_path.exists() {
        return Err(project_command_error(
            "project template target already exists",
            "help: move existing ku.mod/src/main.ku aside, or choose another project directory",
        ));
    }
    fs::create_dir_all(path.join("src")).map_err(|err| {
        KuError::message(format!(
            "failed to create project directory '{}': {err}",
            path.display()
        ))
    })?;
    let package_name = package_name_from_project_name(name);
    let manifest = project_manifest(&package_name, template);
    let main = project_main_source(template);
    fs::write(&manifest_path, manifest).map_err(|err| {
        KuError::message(format!(
            "failed to write '{}': {err}",
            manifest_path.display()
        ))
    })?;
    fs::write(&main_path, main).map_err(|err| {
        KuError::message(format!("failed to write '{}': {err}", main_path.display()))
    })?;
    Ok(())
}

fn project_manifest(name: &str, template: &ProjectTemplate) -> String {
    let mut manifest = format!(
        "name = \"{name}\"\nversion = \"0.1.0\"\nroot = \"src\"\ncache = \".ku/cache\"\nout = \".ku/build\"\n"
    );
    manifest.push_str("main = \"main.ku\"\n");
    manifest.push_str(&format!("template = \"{}\"\n", template.name));
    if template.is_lib {
        manifest.push_str("type = \"lib\"\n");
    }
    manifest
}

fn project_main_source(template: &ProjectTemplate) -> &'static str {
    match template.name {
        "basic" => {
            r#"fn main() {
    // `println` prints one line.
    println("Hello Ku")
}
"#
        }
        "cli" => {
            r#"fn main() {
    // Command line arguments will get a dedicated std API later.
    println("Ku CLI tool")
}
"#
        }
        "http" => {
            r#"import { http, time } from "std"

fn health() {
    return http.text("Ku HTTP OK")
}

fn index() {
    return http.text("Ku HTTP 123")
}

fn main(): null! {
    app = http.service()

    app.get("/", health)
    app.get("/index", index)
    app.get("/json", fn(req) {
        return http.json({
            code: 0,
            msg: "ok",
            data: {
                path: req.path.clone(),
                now_ms: time.millis()
            }
        })
    })
    app.get("/user/{id}", fn(req) {
        return http.json({
            code: 0,
            msg: "ok",
            data: {
                id: req.params.id.clone(),
                q: req.query.get_or("q", ""),
                method: req.method.clone()
            }
        })
    })
    app.post("/echo", fn(req) {
        return http.text(req.body.clone())
    })

    println("Ku HTTP server listening on http://127.0.0.1:8080")
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
"#
        }
        "json" => {
            r#"import { json } from "std"

fn main(): null! {
    data = {
        code: 0,
        msg: "ok",
        data: { name: "Ku" }
    }
    println(json.stringify(data)?)
    return ok(null)
}
"#
        }
        "fs" => {
            r#"import { fs } from "std"

fn main(): null! {
    fs.write("hello.txt", "Hello Ku")?
    text = fs.read("hello.txt")?
    println(text)
    return ok(null)
}
"#
        }
        "lib" => {
            r#"fn Add(a:int, b:int): int {
    return a + b
}

fn main() {
    println(Add(1, 2))
}
"#
        }
        _ => unreachable!("unknown built-in template"),
    }
}

fn validate_project_name(name: &str) -> Result<(), KuError> {
    if is_valid_project_name(name) {
        Ok(())
    } else {
        Err(project_command_error(
            format!("invalid project name '{name}'"),
            "help: use names like `hello`, `HelloWorld`, `my-api`, or `data_tool`",
        ))
    }
}

fn is_valid_project_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn package_name_from_project_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn project_command_error(message: impl Into<String>, help: impl Into<String>) -> KuError {
    KuError::message(format!("{}\n{}", message.into(), help.into()))
}

fn source_arg_or_project_with_dependency_mode(
    args: &[String],
    command: &str,
) -> Result<(PathBuf, String, DependencyResolveMode), KuError> {
    let mut path = None::<PathBuf>;
    let mut selected_dependency_mode = None;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, command)?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!("unknown {command} option '{value}'")));
            }
            value => {
                if path.is_some() {
                    return Err(command_error(format!(
                        "too many arguments for 'ku {command}'"
                    )));
                }
                path = Some(PathBuf::from(value));
            }
        }
    }

    let (path, source) = match path {
        Some(path) => {
            let source = read_ku_path(&path)?;
            (path, source)
        }
        None => project_entry_source(command)?,
    };
    Ok((
        path,
        source,
        selected_dependency_mode.unwrap_or(DependencyResolveMode::Update),
    ))
}

fn parse_native_compat_args(args: &[String]) -> Result<(PathBuf, DependencyResolveMode), KuError> {
    let mut path = None::<PathBuf>;
    let mut selected_dependency_mode = None;
    for arg in args.iter().skip(3) {
        match arg.as_str() {
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, "build --native")?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!(
                    "unknown build --native option '{value}'"
                )));
            }
            value => {
                if path.is_some() {
                    return Err(command_error(
                        "ku build --native accepts exactly one .ku file",
                    ));
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path =
        path.ok_or_else(|| command_error("missing .ku file path for 'ku build --native'"))?;
    Ok((
        path,
        selected_dependency_mode.unwrap_or(DependencyResolveMode::Update),
    ))
}

fn select_dependency_mode(
    selected: &mut Option<DependencyResolveMode>,
    flag: &str,
    command: &str,
) -> Result<(), KuError> {
    if selected.is_some() {
        return Err(command_error(format!(
            "ku {command} accepts only one of --locked or --offline"
        )));
    }
    *selected = Some(match flag {
        "--locked" => DependencyResolveMode::Locked,
        "--offline" => DependencyResolveMode::Offline,
        _ => unreachable!("dependency mode is selected only from known flags"),
    });
    Ok(())
}

fn project_entry_source(command: &str) -> Result<(PathBuf, String), KuError> {
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let package = package::discover_from_dir(&cwd)?.ok_or_else(|| {
        command_error(format!(
            "ku {command} needs a .ku file or a ku.mod package in the current directory"
        ))
    })?;
    let entry = package_entry_path(&package);
    let source = read_ku_path(&entry)?;
    Ok((entry, source))
}

fn exact_path<'a>(args: &'a [String], command: &str) -> Result<&'a str, KuError> {
    if args.len() < 3 {
        return Err(command_error(format!(
            "missing .ku file path for 'ku {command}'"
        )));
    }
    reject_extra_args(args, 3, command)?;
    Ok(args[2].as_str())
}

fn reject_extra_args(args: &[String], expected_len: usize, command: &str) -> Result<(), KuError> {
    if args.len() > expected_len {
        Err(command_error(format!(
            "too many arguments for 'ku {command}'"
        )))
    } else {
        Ok(())
    }
}

fn read_ku_file(path: &str) -> Result<String, KuError> {
    read_ku_path(Path::new(path))
}

fn read_ku_path(path: &Path) -> Result<String, KuError> {
    if !is_ku_path(path) {
        return Err(expected_ku_file(&path_string(path)));
    }
    reject_large_file(path, Span::default())?;
    fs::read_to_string(path)
        .map_err(|e| KuError::message(format!("failed to read {}: {e}", path.display())))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildProfile {
    Debug,
    Release,
    Small,
    Fast,
}

impl BuildProfile {
    fn parse(value: &str) -> Result<Self, KuError> {
        match value {
            "debug" => Ok(Self::Debug),
            "release" => Ok(Self::Release),
            "small" => Ok(Self::Small),
            "fast" => Ok(Self::Fast),
            _ => Err(command_error(format!(
                "unknown build profile '{value}'; expected debug, release, small, or fast"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
            Self::Small => "small",
            Self::Fast => "fast",
        }
    }

    fn rustc_opt_level(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("2"),
            Self::Small => Some("s"),
            Self::Fast => Some("3"),
        }
    }

    fn msvc_opt_flag(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("/O2"),
            Self::Small => Some("/O1"),
            Self::Fast => Some("/O2"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildBackend {
    Runner,
    C,
    Llvm,
}

impl BuildBackend {
    fn parse(value: &str) -> Result<Self, KuError> {
        match value {
            "runner" | "interp" | "interpreter" => Ok(Self::Runner),
            "c" | "native-c" => Ok(Self::C),
            "llvm" | "ll" => Ok(Self::Llvm),
            _ => Err(command_error(format!(
                "unknown build backend '{value}'; expected runner, c, or llvm"
            ))),
        }
    }
}

#[derive(Debug)]
struct BuildOptions {
    entry: Option<PathBuf>,
    output: Option<PathBuf>,
    profile: BuildProfile,
    target: Option<String>,
    backend: BuildBackend,
    emit_c: bool,
    emit_ir: bool,
    emit_llvm: bool,
    clean: bool,
    verbose: bool,
    lto: bool,
    strip: bool,
    static_link: bool,
    dependency_mode: DependencyResolveMode,
}

#[derive(Debug)]
struct BuildPlan {
    entry: PathBuf,
    source: String,
    out_root: PathBuf,
    build_dir: PathBuf,
    output: PathBuf,
    ir_output: PathBuf,
    native_c_output: PathBuf,
    llvm_output: PathBuf,
    root_lock_path: PathBuf,
    output_lock_path: PathBuf,
    target: Option<BuildTarget>,
}

#[derive(Clone, Copy)]
enum BuildLockMode {
    Shared,
    Exclusive,
}

struct BuildFileLock {
    file: fs::File,
}

impl Drop for BuildFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn acquire_build_file_lock_until(
    path: &Path,
    mode: BuildLockMode,
    deadline: Instant,
) -> Result<BuildFileLock, KuError> {
    let file = package::open_validated_package_operation_lock_file(path)?;
    loop {
        let result = match mode {
            BuildLockMode::Shared => file.try_lock_shared(),
            BuildLockMode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => return Ok(BuildFileLock { file }),
            Err(fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(command_error(format!(
                        "build output remained busy for {} seconds\nhelp: wait for the other build using '{}' to finish, then retry",
                        BUILD_LOCK_TIMEOUT.as_secs(),
                        path.display()
                    )));
                }
                thread::sleep(
                    BUILD_LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
                );
            }
            Err(fs::TryLockError::Error(err)) => {
                return Err(KuError::message(format!(
                    "failed to lock build output '{}': {err}",
                    path.display()
                )));
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RunnerBuildConfig<'a> {
    profile: BuildProfile,
    target: Option<&'a BuildTarget>,
    lto: bool,
    strip: bool,
    verbose: bool,
    dependency_mode: DependencyResolveMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BuildTarget {
    slug: String,
    rust_triple: &'static str,
    c_triple: &'static str,
    is_windows: bool,
    binary_format: NativeBinaryFormat,
}

impl BuildTarget {
    fn matches_host(&self) -> bool {
        match self.binary_format {
            NativeBinaryFormat::ElfX86_64 => {
                cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")
            }
            NativeBinaryFormat::PeX86_64 => cfg!(windows) && cfg!(target_arch = "x86_64"),
            NativeBinaryFormat::MachOArm64 => {
                cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CCompilerCandidate {
    label: String,
    program: String,
    args: Vec<String>,
    kind: CCompilerKind,
    explicitly_configured: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CCompilerKind {
    ZigCc,
    Clang,
    Preconfigured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeBinaryFormat {
    ElfX86_64,
    PeX86_64,
    MachOArm64,
}

fn run_build_command(args: &[String]) -> Result<(), KuError> {
    let options = parse_build_options(args)?;
    let plan = resolve_build_plan(&options)?;
    let lock_deadline = Instant::now() + BUILD_LOCK_TIMEOUT;
    let _root_lock = acquire_build_file_lock_until(
        &plan.root_lock_path,
        if options.clean {
            BuildLockMode::Exclusive
        } else {
            BuildLockMode::Shared
        },
        lock_deadline,
    )?;
    if options.clean && plan.out_root.exists() {
        fs::remove_dir_all(&plan.out_root).map_err(|err| {
            KuError::message(format!(
                "failed to clean build directory '{}': {err}",
                plan.out_root.display()
            ))
        })?;
    }
    fs::create_dir_all(&plan.build_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create build directory '{}': {err}",
            plan.build_dir.display()
        ))
    })?;
    let _output_lock = acquire_build_file_lock_until(
        &plan.output_lock_path,
        BuildLockMode::Exclusive,
        lock_deadline,
    )?;

    if options.verbose {
        println!("build entry: {}", plan.entry.display());
        println!("build profile: {}", options.profile.as_str());
        println!("build directory: {}", plan.build_dir.display());
    }

    if options.emit_ir {
        let output = write_checked_ir_artifact(&plan, options.dependency_mode)?;
        println!("ir ok: {}", output.display());
    }
    if options.emit_llvm {
        let output = write_llvm_ir_artifact(&plan, options.dependency_mode)?;
        println!("llvm ir ok: {}", output.display());
    }

    match options.backend {
        BuildBackend::Runner => {
            if options.emit_c {
                let output = write_native_c_artifact(&plan, options.dependency_mode)?;
                println!("native c ok: {}", output.display());
            }
            if options.static_link && options.verbose {
                println!("note: --static is reserved for native backends; runner backend embeds Ku source in a Rust wrapper");
            }
            let entry = path_string(&plan.entry);
            let output = build_executable_to(
                &entry,
                &plan.source,
                &plan.output,
                RunnerBuildConfig {
                    profile: options.profile,
                    target: plan.target.as_ref(),
                    lto: options.lto,
                    strip: options.strip,
                    verbose: options.verbose,
                    dependency_mode: options.dependency_mode,
                },
            )?;
            println!("build ok: {}", output.display());
        }
        BuildBackend::C => {
            let c_output = write_native_c_artifact(&plan, options.dependency_mode)?;
            println!("native c ok: {}", c_output.display());
            compile_c_source(
                &c_output,
                &plan.output,
                plan.target.as_ref(),
                options.profile,
                options.static_link,
                options.verbose,
            )?;
            println!("build ok: {}", plan.output.display());
        }
        BuildBackend::Llvm => {
            let llvm_output = write_llvm_ir_artifact(&plan, options.dependency_mode)?;
            println!("llvm ir ok: {}", llvm_output.display());
            return Err(KuError::message(format!(
                "LLVM backend does not link executables yet; wrote {}\nhelp: use `ku build` for a runnable wrapper, or `ku build --emit-llvm` when you only need text IR",
                llvm_output.display()
            )));
        }
    }

    Ok(())
}

fn parse_build_options(args: &[String]) -> Result<BuildOptions, KuError> {
    let mut options = BuildOptions {
        entry: None,
        output: None,
        profile: BuildProfile::Debug,
        target: None,
        backend: BuildBackend::Runner,
        emit_c: false,
        emit_ir: false,
        emit_llvm: false,
        clean: false,
        verbose: false,
        lto: false,
        strip: false,
        static_link: false,
        dependency_mode: DependencyResolveMode::Update,
    };
    let mut selected_dependency_mode = None;

    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-o" | "--output" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing output path after -o/--output"));
                };
                options.output = Some(PathBuf::from(value));
            }
            "--profile" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing profile after --profile"));
                };
                options.profile = BuildProfile::parse(value)?;
            }
            "--release" => options.profile = BuildProfile::Release,
            "--debug" => options.profile = BuildProfile::Debug,
            "--small" => options.profile = BuildProfile::Small,
            "--fast" => options.profile = BuildProfile::Fast,
            "--target" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing target after --target"));
                };
                options.target = Some(value.clone());
            }
            "--backend" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing backend after --backend"));
                };
                options.backend = BuildBackend::parse(value)?;
            }
            "--emit-c" => options.emit_c = true,
            "--emit-ir" => options.emit_ir = true,
            "--emit-llvm" | "--emit-ll" => options.emit_llvm = true,
            "--clean" => options.clean = true,
            "--verbose" | "-v" => options.verbose = true,
            "--lto" => options.lto = true,
            "--strip" => options.strip = true,
            "--static" => options.static_link = true,
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, "build")?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!("unknown build option '{value}'")));
            }
            value => {
                if options.entry.is_some() {
                    return Err(command_error(
                        "ku build accepts at most one file or project path",
                    ));
                }
                options.entry = Some(PathBuf::from(value));
            }
        }
        index += 1;
    }

    options.dependency_mode = selected_dependency_mode.unwrap_or(DependencyResolveMode::Update);

    Ok(options)
}

fn resolve_build_plan(options: &BuildOptions) -> Result<BuildPlan, KuError> {
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let (entry, package, explicit_file_entry) = match &options.entry {
        Some(path) if path.is_dir() => {
            let package = package::discover_from_dir(path)?.ok_or_else(|| {
                KuError::message(format!(
                    "no ku.mod found for project '{}'\nhelp: run `ku build <file.ku>` or add ku.mod with name/root/main",
                    path.display()
                ))
            })?;
            (package_entry_path(&package), Some(package), false)
        }
        Some(path) => {
            if !is_ku_path(path) {
                return Err(KuError::message(format!(
                    "expected a .ku source file or package directory for ku build, got '{}'\nhelp: use `ku build src/main.ku` or `ku build .`",
                    path.display()
                )));
            }
            let entry = fs::canonicalize(path).map_err(|err| {
                KuError::message(format!(
                    "failed to resolve build entry '{}': {err}",
                    path.display()
                ))
            })?;
            let package = package::discover_for_file(&entry)?;
            (entry, package, true)
        }
        None => {
            let package = package::discover_from_dir(&cwd)?.ok_or_else(|| {
                KuError::message(
                    "ku build needs a .ku file or a ku.mod package in the current directory\nhelp: use `ku build <file.ku>`, or create ku.mod with name/root/main",
                )
            })?;
            (package_entry_path(&package), Some(package), false)
        }
    };

    if !entry.exists() {
        return Err(KuError::message(format!(
            "build entry '{}' does not exist\nhelp: check ku.mod main/root, or pass an explicit .ku file",
            entry.display()
        )));
    }
    if !is_ku_path(&entry) {
        return Err(KuError::message(format!(
            "build entry '{}' is not a .ku source file\nhelp: set ku.mod main to a .ku file",
            entry.display()
        )));
    }
    reject_large_file(&entry, Span::default())?;
    let source = fs::read_to_string(&entry).map_err(|err| {
        KuError::message(format!(
            "failed to read build entry '{}': {err}",
            entry.display()
        ))
    })?;

    if explicit_file_entry && options.output.is_none() {
        if let Some(package) = package.as_ref() {
            let package_entry = package_entry_path(package);
            let package_entry = fs::canonicalize(&package_entry).unwrap_or(package_entry);
            if entry != package_entry {
                return Err(command_error(format!(
                    "building a non-main package entry requires an explicit output path\nhelp: use `ku build -o <output> {}`; use `ku build {}` for the package main entry",
                    entry.display(),
                    package.package_dir.display()
                )));
            }
        }
    }

    let package_name = package
        .as_ref()
        .map(|package| package.manifest.name.clone())
        .unwrap_or_else(|| {
            entry
                .file_stem()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("ku_app")
                .to_string()
        });
    let out_root = package
        .as_ref()
        .map(|package| {
            package.package_dir.join(
                package
                    .manifest
                    .out
                    .as_deref()
                    .unwrap_or(package::DEFAULT_BUILD_DIR),
            )
        })
        .unwrap_or_else(|| {
            entry
                .parent()
                .map(|parent| parent.join(package::DEFAULT_BUILD_DIR))
                .unwrap_or_else(|| cwd.join(package::DEFAULT_BUILD_DIR))
        });
    let target = resolve_build_target(options.target.as_deref())?;
    let build_dir = build_profile_dir(&out_root, options.profile, target.as_ref());
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| build_dir.join(&package_name));
    let output = with_executable_extension(output, target.as_ref());
    let output_digest = native_output_path_digest(&output, &cwd);
    let explicit_output = options.output.is_some();
    let ir_output = build_intermediate_artifact_path(
        &build_dir,
        "ir",
        &output,
        "ir",
        explicit_output,
        &output_digest,
    );
    let native_c_output = build_intermediate_artifact_path(
        &build_dir,
        "c",
        &output,
        "c",
        explicit_output,
        &output_digest,
    );
    let llvm_output = build_intermediate_artifact_path(
        &build_dir,
        "llvm",
        &output,
        "ll",
        explicit_output,
        &output_digest,
    );
    // Keep locks outside every build tree: `--clean` must never unlink a lock
    // that another process still relies on. A process-wide temporary root also
    // makes two projects targeting the same absolute output coordinate on the
    // same lock file.
    let lock_dir = env::temp_dir().join("ku-build-locks-v1");
    let root_lock_path = lock_dir.join(format!(
        "root-{}.lock",
        native_output_path_digest(&out_root, &cwd)
    ));
    let output_lock_path = lock_dir.join(format!("output-{output_digest}.lock"));

    Ok(BuildPlan {
        entry,
        source,
        out_root,
        build_dir,
        output,
        ir_output,
        native_c_output,
        llvm_output,
        root_lock_path,
        output_lock_path,
        target,
    })
}

fn package_entry_path(package: &PackageContext) -> PathBuf {
    let mut entry = package.import_root.join(
        package
            .manifest
            .main
            .as_deref()
            .unwrap_or(package::DEFAULT_MAIN_FILE),
    );
    if entry.extension().is_none() {
        entry.set_extension("ku");
    }
    entry
}

fn build_profile_dir(
    out_root: &Path,
    profile: BuildProfile,
    target: Option<&BuildTarget>,
) -> PathBuf {
    if let Some(target) = target {
        out_root.join(&target.slug).join(profile.as_str())
    } else {
        out_root.join(profile.as_str())
    }
}

fn with_executable_extension(mut path: PathBuf, target: Option<&BuildTarget>) -> PathBuf {
    let needs_exe = target
        .map(|target| target.is_windows)
        .unwrap_or_else(|| cfg!(windows));
    if needs_exe && path.extension().is_none() {
        path.set_extension("exe");
    }
    path
}

fn resolve_build_target(target: Option<&str>) -> Result<Option<BuildTarget>, KuError> {
    let Some(raw) = target else {
        return Ok(None);
    };
    let value = raw.trim();
    if value == "host" {
        return Ok(None);
    }
    if value.is_empty()
        || value.contains(['/', '\\', ':'])
        || value.split('-').any(|part| part == "." || part == "..")
    {
        return Err(command_error(format!(
            "invalid build target '{raw}'\nhelp: use host, x86_64-linux, x86_64-windows, or aarch64-darwin"
        )));
    }
    let target = match value {
        "x86_64-linux" => BuildTarget {
            slug: value.to_string(),
            rust_triple: "x86_64-unknown-linux-gnu",
            c_triple: "x86_64-linux-gnu",
            is_windows: false,
            binary_format: NativeBinaryFormat::ElfX86_64,
        },
        "x86_64-windows" => BuildTarget {
            slug: value.to_string(),
            rust_triple: "x86_64-pc-windows-msvc",
            c_triple: "x86_64-windows-gnu",
            is_windows: true,
            binary_format: NativeBinaryFormat::PeX86_64,
        },
        "aarch64-darwin" => BuildTarget {
            slug: value.to_string(),
            rust_triple: "aarch64-apple-darwin",
            c_triple: "aarch64-macos",
            is_windows: false,
            binary_format: NativeBinaryFormat::MachOArm64,
        },
        _ => {
            return Err(command_error(format!(
                "unsupported build target '{raw}'\nhelp: this stage supports host, x86_64-linux, x86_64-windows, and aarch64-darwin"
            )))
        }
    };
    Ok(Some(target))
}

fn supported_host_build_target() -> Option<BuildTarget> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return resolve_build_target(Some("x86_64-linux")).ok().flatten();
    }
    if cfg!(all(windows, target_arch = "x86_64")) {
        return resolve_build_target(Some("x86_64-windows")).ok().flatten();
    }
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return resolve_build_target(Some("aarch64-darwin")).ok().flatten();
    }
    None
}

fn is_ku_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write_checked_ir_artifact(
    plan: &BuildPlan,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let entry = path_string(&plan.entry);
    let program = parse_and_check_with_dependency_mode(&entry, &plan.source, dependency_mode)?;
    let output = plan.ir_output.clone();
    let lowered = ir::lower_program(&program)?;
    write_text_artifact(&output, format!("{}", ir::optimize_program(&lowered)))
}

fn write_native_c_artifact(
    plan: &BuildPlan,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let output = plan.native_c_output.clone();
    let fs_base = native_fs_base_for_output(&plan.entry, &plan.output)?;
    write_native_c_to(
        &path_string(&plan.entry),
        &plan.source,
        &output,
        fs_base,
        dependency_mode,
    )
}

fn intermediate_artifact_filename(output: &Path, extension: &str) -> OsString {
    let mut binary_name = output
        .file_name()
        .filter(|name| !name.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("main"));
    if binary_name
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        binary_name.set_extension("");
    }
    let mut filename = binary_name.into_os_string();
    filename.push(".");
    filename.push(extension);
    filename
}

fn build_intermediate_artifact_path(
    build_dir: &Path,
    kind: &str,
    output: &Path,
    extension: &str,
    explicit_output: bool,
    output_digest: &str,
) -> PathBuf {
    let root = build_dir.join(kind);
    let root = if explicit_output {
        root.join(output_digest)
    } else {
        root
    };
    root.join(intermediate_artifact_filename(output, extension))
}

fn native_output_path_digest(output: &Path, cwd: &Path) -> String {
    let absolute = if output.is_absolute() {
        output.to_path_buf()
    } else {
        cwd.join(output)
    };
    let absolute = stable_path_identity(&absolute);
    let mut hasher = Sha256::new();
    hasher.update(b"ku-native-output-path-v1\0");

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(absolute.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in absolute.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(absolute.to_string_lossy().as_bytes());

    let digest = hasher.finalize();
    encode_base64url_no_pad(&digest)
}

fn stable_path_identity(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    let mut missing = Vec::<OsString>::new();
    let mut existing = path;
    while !existing.as_os_str().is_empty() {
        if let Ok(mut canonical) = fs::canonicalize(existing) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }

    // Absolute paths always have an existing filesystem root in normal use.
    // Keep the original spelling only for unusual virtual paths where even the
    // root cannot be canonicalized; hashing still remains deterministic.
    path.to_path_buf()
}

fn encode_base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        match chunk {
            [_, second, third] => {
                encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
                encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            }
            [_, second] => {
                encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
                encoded.push(ALPHABET[((second & 0x0f) << 2) as usize] as char);
            }
            [_] => {
                encoded.push(ALPHABET[((first & 0x03) << 4) as usize] as char);
            }
            [] => unreachable!("chunks never yields an empty slice"),
            _ => unreachable!("chunks are bounded to three bytes"),
        }
    }
    encoded
}

fn native_fs_base_for_output(
    entry: &Path,
    executable: &Path,
) -> Result<backend::c::NativeFsBase, KuError> {
    let executable_dir = executable
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(executable_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create output directory '{}': {err}",
            executable_dir.display()
        ))
    })?;

    let result = (|| {
        let source_dir = entry
            .parent()
            .ok_or_else(|| "build entry has no source directory".to_string())?;
        let source_dir = fs::canonicalize(source_dir)
            .map_err(|err| format!("failed to resolve source directory: {err}"))?;
        let executable_dir = fs::canonicalize(executable_dir)
            .map_err(|err| format!("failed to resolve output directory: {err}"))?;
        executable_relative_locator(&executable_dir, &source_dir)
    })();

    Ok(match result {
        Ok(locator) => backend::c::NativeFsBase::ExecutableRelative(locator),
        Err(reason) => backend::c::NativeFsBase::Unavailable(reason),
    })
}

/// Produce a slash-separated locator from the executable directory to the
/// source directory without retaining either absolute build-machine path.
fn executable_relative_locator(executable_dir: &Path, source_dir: &Path) -> Result<String, String> {
    let mut common = executable_dir;
    let mut parent_count = 0usize;
    loop {
        if let Ok(suffix) = source_dir.strip_prefix(common) {
            let mut parts = vec!["..".to_string(); parent_count];
            for component in suffix.components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(value) => parts.push(
                        value
                            .to_str()
                            .ok_or_else(|| {
                                "source/output relative locator is not valid UTF-8".to_string()
                            })?
                            .to_string(),
                    ),
                    Component::ParentDir => parts.push("..".to_string()),
                    Component::Prefix(_) | Component::RootDir => {
                        return Err("source/output paths do not share a filesystem root".to_string())
                    }
                }
            }
            let locator = if parts.is_empty() {
                ".".to_string()
            } else {
                parts.join("/")
            };
            if locator.len() > 32 * 1024 {
                return Err("source/output relative locator exceeds 32 KiB".to_string());
            }
            return Ok(locator);
        }

        let Some(parent) = common.parent() else {
            return Err("source/output paths do not share a filesystem root".to_string());
        };
        if parent == common || parent_count >= 32 * 1024 {
            return Err("source/output relative locator cannot make bounded progress".to_string());
        }
        common = parent;
        parent_count += 1;
    }
}

fn write_llvm_ir_artifact(
    plan: &BuildPlan,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let output = plan.llvm_output.clone();
    write_llvm_ir_to(
        &path_string(&plan.entry),
        &plan.source,
        &output,
        dependency_mode,
    )
}

fn write_text_artifact(output: &Path, text: String) -> Result<PathBuf, KuError> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            KuError::message(format!(
                "failed to create artifact directory '{}': {err}",
                parent.display()
            ))
        })?;
    }
    fs::write(output, text).map_err(|err| {
        KuError::message(format!(
            "failed to write artifact '{}': {err}",
            output.display()
        ))
    })?;
    Ok(output.to_path_buf())
}

fn build_executable_to(
    path: &str,
    source: &str,
    output: &Path,
    config: RunnerBuildConfig<'_>,
) -> Result<PathBuf, KuError> {
    check_source_with_dependency_mode(path, source, config.dependency_mode)?;
    validate_native_output_name(output)?;
    let output_directory = link_output_directory(output);
    fs::create_dir_all(output_directory).map_err(|err| {
        KuError::message(format!(
            "failed to create output directory '{}': {err}",
            output_directory.display()
        ))
    })?;
    let output_staging = LinkOutputStaging::create(output)?;
    let temporary_output = output_staging.path();
    let embedded_path = fs::canonicalize(path)
        .unwrap_or_else(|_| Path::new(path).to_path_buf())
        .to_string_lossy()
        .to_string();
    let rust_source = build_runner_source(&embedded_path, source, config.dependency_mode);
    let temp_guard = TempBuildDir::create_private("runner").map_err(|err| {
        KuError::message(format!("failed to create private build directory: {err}"))
    })?;
    let temp_dir = temp_guard.path().to_path_buf();
    let runner = temp_dir.join("runner.rs");
    let mut runner_options = fs::OpenOptions::new();
    runner_options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        runner_options.mode(0o600);
    }
    let mut runner_file = runner_options.open(&runner).map_err(|err| {
        KuError::message(format!(
            "failed to create private build runner {}: {err}",
            runner.display()
        ))
    })?;
    runner_file
        .write_all(rust_source.as_bytes())
        .map_err(|err| {
            KuError::message(format!(
                "failed to write build runner {}: {err}",
                runner.display()
            ))
        })?;
    drop(runner_file);

    let exe_dir = env::current_exe()
        .map_err(|err| KuError::message(format!("failed to locate ku executable: {err}")))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| KuError::message("failed to locate ku executable directory"))?;
    let target_dir = if exe_dir.file_name().is_some_and(|name| name == "deps") {
        exe_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| KuError::message("failed to locate ku target directory"))?
    } else {
        exe_dir
    };
    let lib = find_ku_rlib(&target_dir)?;
    let dependency_dirs = find_dependency_dirs(&target_dir);
    let mut command = Command::new("rustc");
    command
        .arg("--edition=2021")
        .arg(&runner)
        .arg("--extern")
        .arg(format!("ku={}", lib.display()))
        .arg("-o")
        .arg(temporary_output);
    for deps in &dependency_dirs {
        command
            .arg("-L")
            .arg(format!("dependency={}", deps.display()));
    }
    if let Some(target) = config.target {
        command.arg("--target").arg(target.rust_triple);
    }
    if let Some(opt_level) = config.profile.rustc_opt_level() {
        command.arg("-C").arg(format!("opt-level={opt_level}"));
        command.arg("-C").arg("debuginfo=0");
    }
    if config.lto {
        command.arg("-C").arg("lto=fat");
    }
    if config.strip {
        command.arg("-C").arg("strip=symbols");
    }
    if config.verbose {
        println!("rustc command: {command:?}");
    }
    let status = match run_build_process_bounded(&mut command, RUSTC_PROCESS_TIMEOUT) {
        Ok(status) => status,
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to run bounded rustc for ku build: {err}"
            )));
        }
    };
    temp_guard.cleanup();
    if !status.success() {
        return Err(KuError::message(format!(
            "ku build failed: rustc exited with {status}\nhelp: make sure Rust is installed and libku.rlib plus its dependency directory match the selected target"
        )));
    }
    let verified = validate_runner_output_candidate(temporary_output, config.target)?;
    install_verified_link_output(verified, &output_staging)?;
    Ok(output.to_path_buf())
}

fn build_native_c_with_dependency_mode(
    path: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let output = Path::new(path).with_extension("c");
    write_native_c_to(
        path,
        source,
        &output,
        backend::c::NativeFsBase::ExecutableRelative(".".to_string()),
        dependency_mode,
    )
}

fn write_native_c_to(
    path: &str,
    source: &str,
    output: &Path,
    fs_base: backend::c::NativeFsBase,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let program = parse_and_expand_with_dependency_mode(path, source, dependency_mode)?;
    reject_native_async(&program)?;
    Checker::new().check(&program)?;
    let lowered = ir::lower_program(&program)?;
    let optimized = ir::optimize_program(&lowered);
    let c_source = backend::c::generate_c_source_with_options(
        &optimized,
        &backend::c::CBackendOptions {
            fs_base,
            // Test-only, generation-time opt-in. This environment is read by
            // the isolated `ku build` child used by native OOM tests; the
            // backend API itself remains deterministic and defaults to false.
            object_oom_fault_injection: env::var("KU_NATIVE_TEST_OBJECT_OOM_ENABLE").as_deref()
                == Ok("1"),
        },
    )?;
    write_text_artifact(output, c_source)
}

fn build_llvm_ir(path: &str, source: &str) -> Result<PathBuf, KuError> {
    let output = Path::new(path).with_extension("ll");
    write_llvm_ir_to(path, source, &output, DependencyResolveMode::Update)
}

fn write_llvm_ir_to(
    path: &str,
    source: &str,
    output: &Path,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let program = parse_and_expand_with_dependency_mode(path, source, dependency_mode)?;
    reject_compiled_async(
        &program,
        "LLVM text prototype does not support async/await yet; use the interpreter runtime",
    )?;
    Checker::new().check(&program)?;
    let lowered = ir::lower_program(&program)?;
    let optimized = ir::optimize_program(&lowered);
    let llvm_ir = backend::llvm::generate_llvm_ir(&optimized)?;
    write_text_artifact(output, llvm_ir)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CSourceFeatures {
    winsock: bool,
    pthreads: bool,
    libpq: bool,
    libmysql: bool,
}

impl CSourceFeatures {
    fn inspect(source: &Path) -> Result<Self, KuError> {
        let text = fs::read_to_string(source).map_err(|err| {
            KuError::message(format!(
                "failed to inspect generated native C '{}': {err}",
                source.display()
            ))
        })?;
        Ok(Self {
            winsock: text.contains("#include <winsock2.h>"),
            pthreads: text.contains("#include <pthread.h>"),
            libpq: text
                .lines()
                .any(|line| line == "#define KU_FEATURE_LIBPQ 1"),
            libmysql: text
                .lines()
                .any(|line| line == "#define KU_FEATURE_LIBMYSQL 1"),
        })
    }
}

fn validate_c_target_features(
    features: CSourceFeatures,
    target: Option<&BuildTarget>,
) -> Result<(), KuError> {
    let Some(target) = target else {
        return Ok(());
    };
    if features.libmysql && !target.matches_host() {
        return Err(KuError::message(format!(
            "native target '{}' cannot automatically link std.mysql: Ku has no portable target-library contract for libmysqlclient yet\nhelp: use a host build with KU_MYSQL_LIB, or link the target-specific emitted C yourself with the matching client library",
            target.slug
        )));
    }
    Ok(())
}

/// Locate a host libmysqlclient/MariaDB client library. `KU_MYSQL_LIB` is the
/// explicit portable contract; Windows also discovers conventional installs.
/// Selection remains deferred until the compiler ABI is known.
fn explicit_libmysql_directory(
    configured: Option<OsString>,
) -> Result<Option<LibmysqlDirectorySnapshot>, KuError> {
    let Some(configured) = configured else {
        return Ok(None);
    };
    if configured.is_empty() {
        return Err(KuError::message(
            "KU_MYSQL_LIB is set but empty\nhelp: set it to an absolute, dedicated directory containing the target-compatible shared/import MySQL client library",
        ));
    }
    let configured = PathBuf::from(configured);
    if !configured.is_absolute() {
        return Err(KuError::message(format!(
            "KU_MYSQL_LIB must name an absolute directory, got '{}'",
            configured.display()
        )));
    }
    let dir = fs::canonicalize(&configured).map_err(|error| {
        KuError::message(format!(
            "failed to resolve KU_MYSQL_LIB directory '{}': {error}",
            configured.display()
        ))
    })?;
    if !path_is_plain_directory(&dir) {
        return Err(KuError::message(format!(
            "KU_MYSQL_LIB must name a plain directory, got '{}'",
            configured.display()
        )));
    }
    snapshot_libmysql_directory(&dir)?.map_or_else(
        || {
            Err(KuError::message(format!(
                "KU_MYSQL_LIB directory '{}' changed while being inspected; refusing to fall back",
                configured.display()
            )))
        },
        |snapshot| Ok(Some(snapshot)),
    )
}

fn detect_libmysql_directory() -> Result<Option<LibmysqlDirectorySnapshot>, KuError> {
    if let Some(snapshot) = explicit_libmysql_directory(env::var_os("KU_MYSQL_LIB"))? {
        return Ok(Some(snapshot));
    }
    for base in [r"C:\Program Files\MySQL", r"D:\Program Files\MySQL"] {
        if let Ok(entries) = fs::read_dir(base) {
            let mut dirs: Vec<PathBuf> = entries
                .take(MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES)
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("lib"))
                .filter(|dir| path_is_plain_directory(dir))
                .collect();
            sort_install_dirs_by_version(&mut dirs);
            while let Some(dir) = dirs.pop() {
                let dir = fs::canonicalize(&dir).map_err(|error| {
                    KuError::message(format!(
                        "failed to canonicalize discovered MySQL library directory '{}': {error}",
                        dir.display()
                    ))
                })?;
                if let Some(snapshot) = snapshot_libmysql_directory(&dir)? {
                    if find_libmysql_library_in_snapshot(&snapshot, LibpqLibraryFormat::WindowsMsvc)
                        .is_some()
                    {
                        return Ok(Some(snapshot));
                    }
                }
            }
        }
    }
    Ok(None)
}

fn mysql_header_in(dir: &Path) -> bool {
    path_is_plain_regular_file(&dir.join("mysql.h"))
        || path_is_plain_regular_file(&dir.join("mysql").join("mysql.h"))
        || path_is_plain_regular_file(&dir.join("mariadb").join("mysql.h"))
}

/// MYSQL_BIND is a versioned public struct and must come from the matching
/// development header. Never synthesize its layout in generated C.
fn explicit_libmysql_include_dir(configured: Option<OsString>) -> Result<Option<PathBuf>, KuError> {
    if let Some(dir) = configured {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() || !path_is_plain_directory(&dir) || !mysql_header_in(&dir) {
            return Err(KuError::message(format!(
                "KU_MYSQL_INCLUDE must name an absolute plain directory containing mysql.h, got '{}'",
                dir.display()
            )));
        }
        return fs::canonicalize(&dir).map(Some).map_err(|error| {
            KuError::message(format!(
                "failed to canonicalize KU_MYSQL_INCLUDE '{}': {error}",
                dir.display()
            ))
        });
    }
    Ok(None)
}

fn detect_libmysql_include_dir(library_dir: Option<&Path>) -> Result<Option<PathBuf>, KuError> {
    if let Some(dir) = explicit_libmysql_include_dir(env::var_os("KU_MYSQL_INCLUDE"))? {
        return Ok(Some(dir));
    }
    if let Some(root) = library_dir.and_then(Path::parent) {
        for candidate in [root.join("include"), root.join("include").join("mysql")] {
            if path_is_plain_directory(&candidate) && mysql_header_in(&candidate) {
                return fs::canonicalize(&candidate).map(Some).map_err(|error| {
                    KuError::message(format!(
                        "failed to canonicalize MySQL include directory '{}': {error}",
                        candidate.display()
                    ))
                });
            }
        }
        return Err(KuError::message(format!(
            "MySQL library directory '{}' has no matching sibling include/mysql.h\nhelp: set KU_MYSQL_INCLUDE to the absolute include directory from the same client installation",
            library_dir.expect("library root exists").display()
        )));
    }
    Ok([
        PathBuf::from("/usr/include/mysql"),
        PathBuf::from("/usr/include/mariadb"),
        PathBuf::from("/usr/local/include/mysql"),
        PathBuf::from("/usr/local/include/mariadb"),
    ]
    .into_iter()
    .find(|candidate| path_is_plain_directory(candidate) && mysql_header_in(candidate)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpqLibraryPlatform {
    Windows,
    Linux,
    Darwin,
}

impl LibpqLibraryPlatform {
    fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Darwin
        } else {
            Self::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpqLibraryFormat {
    WindowsMsvc,
    WindowsMingw,
    Linux,
    Darwin,
}

fn libpq_link_platform(target: Option<&BuildTarget>) -> LibpqLibraryPlatform {
    match target {
        None => LibpqLibraryPlatform::host(),
        Some(target) if target.is_windows => LibpqLibraryPlatform::Windows,
        Some(target) if target.rust_triple.ends_with("-apple-darwin") => {
            LibpqLibraryPlatform::Darwin
        }
        Some(_) => LibpqLibraryPlatform::Linux,
    }
}

fn compiler_uses_windows_mingw_abi(candidate: &CCompilerCandidate) -> bool {
    if candidate.kind == CCompilerKind::ZigCc {
        return true;
    }
    let program_name = Path::new(&candidate.program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&candidate.program)
        .to_ascii_lowercase();
    let configured_command = std::iter::once(candidate.program.as_str())
        .chain(candidate.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    program_name == "cc"
        || program_name == "cc.exe"
        || program_name.contains("gcc")
        || configured_command.contains("mingw")
        || configured_command.contains("windows-gnu")
}

fn libpq_library_format(
    platform: LibpqLibraryPlatform,
    candidate: &CCompilerCandidate,
) -> LibpqLibraryFormat {
    match platform {
        LibpqLibraryPlatform::Windows if compiler_uses_windows_mingw_abi(candidate) => {
            LibpqLibraryFormat::WindowsMingw
        }
        LibpqLibraryPlatform::Windows => LibpqLibraryFormat::WindowsMsvc,
        LibpqLibraryPlatform::Linux => LibpqLibraryFormat::Linux,
        LibpqLibraryPlatform::Darwin => LibpqLibraryFormat::Darwin,
    }
}

fn windows_libpq_format_from_target(target: &str) -> Result<LibpqLibraryFormat, KuError> {
    let target = target.to_ascii_lowercase();
    if target.contains("windows-gnu") || target.contains("mingw") {
        return Ok(LibpqLibraryFormat::WindowsMingw);
    }
    if target.contains("windows-msvc") {
        return Ok(LibpqLibraryFormat::WindowsMsvc);
    }
    Err(KuError::message(format!(
        "Windows clang reported unsupported target triple '{target}'\nhelp: set KU_CC to clang with an explicit x86_64 windows-msvc or windows-gnu target",
    )))
}

fn libpq_library_format_for_compiler(
    platform: LibpqLibraryPlatform,
    candidate: &CCompilerCandidate,
    target: Option<&BuildTarget>,
    probed_clang_target: Option<&str>,
) -> Result<LibpqLibraryFormat, KuError> {
    let default = libpq_library_format(platform, candidate);
    if platform == LibpqLibraryPlatform::Windows {
        if let Some(declared_target) = compiler_declared_target(candidate)? {
            return windows_libpq_format_from_target(declared_target);
        }
        if target.is_some() {
            return Ok(match candidate.kind {
                CCompilerKind::Clang => LibpqLibraryFormat::WindowsMsvc,
                CCompilerKind::ZigCc => LibpqLibraryFormat::WindowsMingw,
                CCompilerKind::Preconfigured => default,
            });
        }
    }
    let unqualified_host_clang = platform == LibpqLibraryPlatform::Windows
        && target.is_none()
        && candidate.kind == CCompilerKind::Clang;
    if !unqualified_host_clang {
        return Ok(default);
    }
    let reported = probed_clang_target.ok_or_else(|| {
        KuError::message(
            "Windows clang ABI could not be determined\nhelp: set KU_CC to clang with an explicit --target ending in windows-msvc or windows-gnu",
        )
    })?;
    windows_libpq_format_from_target(reported)
}

fn numeric_library_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn libpq_library_version(name: &str, format: LibpqLibraryFormat) -> Option<&str> {
    match format {
        LibpqLibraryFormat::Linux => name.strip_prefix("libpq.so."),
        LibpqLibraryFormat::Darwin => name
            .strip_prefix("libpq.")
            .and_then(|value| value.strip_suffix(".dylib")),
        LibpqLibraryFormat::WindowsMsvc | LibpqLibraryFormat::WindowsMingw => None,
    }
}

fn normalize_numeric_component(part: &str) -> &str {
    let trimmed = part.trim_start_matches('0');
    if trimmed.is_empty() {
        "0"
    } else {
        trimmed
    }
}

fn compare_numeric_dotted(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left_parts = left.split('.');
    let mut right_parts = right.split('.');
    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(left), Some(right)) => {
                let left = normalize_numeric_component(left);
                let right = normalize_numeric_component(right);
                let order = left.len().cmp(&right.len()).then_with(|| left.cmp(right));
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (None, None) => return std::cmp::Ordering::Equal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkLibraryIdentity {
    identity_high: u64,
    identity_low: u64,
    length: u64,
    modified: Option<SystemTime>,
}

fn same_open_link_library_contents(
    left: &LinkLibraryIdentity,
    right: &LinkLibraryIdentity,
) -> bool {
    left.identity_high == right.identity_high
        && left.identity_low == right.identity_low
        && left.length == right.length
        && left.modified == right.modified
}

#[derive(Debug, Clone)]
struct PinnedLinkLibrary {
    path: PathBuf,
    identity: LinkLibraryIdentity,
    handle: Arc<fs::File>,
}

struct StagedLinkLibrary {
    _directory: TempBuildDir,
    path: PathBuf,
}

impl StagedLinkLibrary {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl PinnedLinkLibrary {
    fn capture(path: &Path, library: &str) -> Result<Self, KuError> {
        let canonical = fs::canonicalize(path).map_err(|error| {
            KuError::message(format!(
                "failed to resolve selected {library} library '{}': {error}",
                path.display()
            ))
        })?;
        let metadata = fs::symlink_metadata(&canonical).map_err(|error| {
            KuError::message(format!(
                "failed to inspect selected {library} library '{}': {error}",
                canonical.display()
            ))
        })?;
        if !is_plain_regular_file(&metadata) || metadata.len() == 0 {
            return Err(KuError::message(format!(
                "selected {library} library '{}' must resolve to a non-empty plain regular file",
                path.display()
            )));
        }
        let file = fs::File::open(&canonical).map_err(|error| {
            KuError::message(format!(
                "failed to open selected {library} library '{}': {error}",
                canonical.display()
            ))
        })?;
        let identity = link_library_identity(&file).map_err(|error| {
            KuError::message(format!(
                "failed to identify selected {library} library '{}': {error}",
                canonical.display()
            ))
        })?;
        if identity.length > MAX_PINNED_LINK_LIBRARY_BYTES {
            return Err(KuError::message(format!(
                "selected {library} library '{}' exceeds the {MAX_PINNED_LINK_LIBRARY_BYTES}-byte link-input limit",
                canonical.display()
            )));
        }
        Ok(Self {
            path: canonical,
            identity,
            handle: Arc::new(file),
        })
    }

    fn has_ar_archive_magic(&self, library: &str) -> Result<bool, KuError> {
        let mut file = self.handle.try_clone().map_err(|error| {
            KuError::message(format!(
                "failed to inspect selected {library} library '{}': {error}",
                self.path.display()
            ))
        })?;
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            KuError::message(format!(
                "failed to inspect selected {library} library '{}': {error}",
                self.path.display()
            ))
        })?;
        let mut magic = [0u8; 8];
        match file.read_exact(&mut magic) {
            Ok(()) => Ok(matches!(&magic, b"!<arch>\n" | b"!<thin>\n")),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
            Err(error) => Err(KuError::message(format!(
                "failed to inspect selected {library} library '{}': {error}",
                self.path.display()
            ))),
        }
    }

    fn stage_for_link(&self, library: &str) -> Result<StagedLinkLibrary, KuError> {
        let before = link_library_identity(&self.handle).map_err(|error| {
            KuError::message(format!(
                "failed to re-identify selected {library} library '{}': {error}",
                self.path.display()
            ))
        })?;
        if !same_open_link_library_contents(&before, &self.identity) || before.length == 0 {
            return Err(KuError::message(format!(
                "selected {library} library '{}' changed before its private link copy; refusing to fall back",
                self.path.display()
            )));
        }
        if before.length > MAX_PINNED_LINK_LIBRARY_BYTES {
            return Err(KuError::message(format!(
                "selected {library} library '{}' exceeds the {MAX_PINNED_LINK_LIBRARY_BYTES}-byte link-input limit",
                self.path.display()
            )));
        }

        let directory = TempBuildDir::create_private("link-library").map_err(|error| {
            KuError::message(format!(
                "failed to create private {library} link-input directory: {error}"
            ))
        })?;
        let file_name = self.path.file_name().ok_or_else(|| {
            KuError::message(format!(
                "selected {library} library '{}' has no file name",
                self.path.display()
            ))
        })?;
        let staged_path = directory.path().join(file_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut staged = options.open(&staged_path).map_err(|error| {
            KuError::message(format!(
                "failed to reserve private {library} link input '{}': {error}",
                staged_path.display()
            ))
        })?;
        let mut source = self.handle.try_clone().map_err(|error| {
            KuError::message(format!(
                "failed to duplicate selected {library} library handle '{}': {error}",
                self.path.display()
            ))
        })?;
        source.seek(SeekFrom::Start(0)).map_err(|error| {
            KuError::message(format!(
                "failed to rewind selected {library} library '{}': {error}",
                self.path.display()
            ))
        })?;

        let mut source_digest = Sha256::new();
        let mut remaining = before.length;
        let mut buffer = [0u8; 64 * 1024];
        while remaining != 0 {
            let chunk = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("bounded link-copy chunk fits usize");
            source.read_exact(&mut buffer[..chunk]).map_err(|error| {
                KuError::message(format!(
                    "selected {library} library '{}' changed while creating its private link copy: {error}",
                    self.path.display()
                ))
            })?;
            staged.write_all(&buffer[..chunk]).map_err(|error| {
                KuError::message(format!(
                    "failed to write private {library} link input '{}': {error}",
                    staged_path.display()
                ))
            })?;
            source_digest.update(&buffer[..chunk]);
            remaining -= chunk as u64;
        }
        let mut trailing = [0u8; 1];
        if source.read(&mut trailing).map_err(|error| {
            KuError::message(format!(
                "failed to finish reading selected {library} library '{}': {error}",
                self.path.display()
            ))
        })? != 0
        {
            return Err(KuError::message(format!(
                "selected {library} library '{}' grew while creating its private link copy",
                self.path.display()
            )));
        }
        staged.flush().map_err(|error| {
            KuError::message(format!(
                "failed to flush private {library} link input '{}': {error}",
                staged_path.display()
            ))
        })?;
        drop(staged);

        let after = link_library_identity(&self.handle).map_err(|error| {
            KuError::message(format!(
                "failed to re-identify selected {library} library '{}': {error}",
                self.path.display()
            ))
        })?;
        if !same_open_link_library_contents(&after, &before) {
            return Err(KuError::message(format!(
                "selected {library} library '{}' changed while creating its private link copy",
                self.path.display()
            )));
        }

        let mut reopened = fs::File::open(&staged_path).map_err(|error| {
            KuError::message(format!(
                "failed to reopen private {library} link input '{}': {error}",
                staged_path.display()
            ))
        })?;
        let staged_metadata = reopened.metadata().map_err(|error| {
            KuError::message(format!(
                "failed to inspect private {library} link input '{}': {error}",
                staged_path.display()
            ))
        })?;
        if !staged_metadata.is_file() || staged_metadata.len() != before.length {
            return Err(KuError::message(format!(
                "private {library} link input '{}' failed its size verification",
                staged_path.display()
            )));
        }
        let mut staged_digest = Sha256::new();
        let mut verified = 0u64;
        loop {
            let read = reopened.read(&mut buffer).map_err(|error| {
                KuError::message(format!(
                    "failed to verify private {library} link input '{}': {error}",
                    staged_path.display()
                ))
            })?;
            if read == 0 {
                break;
            }
            verified = verified.checked_add(read as u64).ok_or_else(|| {
                KuError::message(format!(
                    "private {library} link input '{}' exceeds its verified size",
                    staged_path.display()
                ))
            })?;
            if verified > before.length {
                return Err(KuError::message(format!(
                    "private {library} link input '{}' grew during verification",
                    staged_path.display()
                )));
            }
            staged_digest.update(&buffer[..read]);
        }
        if verified != before.length || staged_digest.finalize() != source_digest.finalize() {
            return Err(KuError::message(format!(
                "private {library} link input '{}' failed its content verification",
                staged_path.display()
            )));
        }

        Ok(StagedLinkLibrary {
            _directory: directory,
            path: staged_path,
        })
    }
}

#[cfg(unix)]
fn link_library_identity(file: &fs::File) -> io::Result<LinkLibraryIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened library is not a regular file",
        ));
    }
    Ok(LinkLibraryIdentity {
        identity_high: metadata.dev(),
        identity_low: metadata.ino(),
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(windows)]
fn link_library_identity(file: &fs::File) -> io::Result<LinkLibraryIdentity> {
    use std::{
        mem::MaybeUninit,
        os::windows::io::{AsRawHandle, RawHandle},
    };

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: RawHandle, information: *mut FileInformation) -> i32;
    }

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened library is not a regular file",
        ));
    }
    let mut information = MaybeUninit::<FileInformation>::uninit();
    // SAFETY: the file owns a valid handle and Windows initializes the complete
    // structure when GetFileInformationByHandle succeeds.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the preceding call returned success.
    let information = unsafe { information.assume_init() };
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if information.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened library is a Windows reparse point",
        ));
    }
    Ok(LinkLibraryIdentity {
        identity_high: u64::from(information.volume_serial_number),
        identity_low: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(not(any(unix, windows)))]
fn link_library_identity(file: &fs::File) -> io::Result<LinkLibraryIdentity> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened library is not a regular file",
        ));
    }
    Ok(LinkLibraryIdentity {
        identity_high: 0,
        identity_low: 0,
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn canonical_target_is_static_archive(library: &PinnedLinkLibrary) -> bool {
    library
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".a"))
}

/// Rank linkable libpq filenames in an explicit `KU_PG_LIB` directory. Unix
/// installations may expose only the runtime SONAME (for example `libpq.so.5`),
/// while development packages normally add the unversioned `libpq.so` symlink.
/// On Windows, configuring the conventional `libpq.lib` filename is an explicit
/// caller assertion that it is an import library rather than a static archive.
fn libpq_library_name_priority(name: &str, format: LibpqLibraryFormat) -> Option<usize> {
    match format {
        LibpqLibraryFormat::WindowsMsvc => {
            if name.eq_ignore_ascii_case("libpqdll.lib") {
                Some(0)
            } else if name.eq_ignore_ascii_case("libpq.lib") {
                Some(1)
            } else {
                None
            }
        }
        LibpqLibraryFormat::WindowsMingw => {
            if name.eq_ignore_ascii_case("libpq.dll.a") {
                Some(0)
            } else {
                None
            }
        }
        LibpqLibraryFormat::Linux => match name {
            "libpq.so" => Some(0),
            _ if name
                .strip_prefix("libpq.so.")
                .is_some_and(numeric_library_version) =>
            {
                Some(1)
            }
            _ => None,
        },
        LibpqLibraryFormat::Darwin => match name {
            "libpq.dylib" => Some(0),
            _ if name
                .strip_prefix("libpq.")
                .and_then(|value| value.strip_suffix(".dylib"))
                .is_some_and(numeric_library_version) =>
            {
                Some(1)
            }
            _ => None,
        },
    }
}

/// Return an existing shared/import library (or a symlink to one), never merely
/// a caller-provided directory. Passing the selected file directly to the
/// target linker also makes that linker the authority for linkable structure
/// and architecture instead of duplicating partial ELF/Mach-O/COFF parsers here.
/// Static Unix archives are deliberately excluded because their transitive
/// dependency closure is not portable across libpq builds.
#[derive(Debug)]
struct LibpqSnapshotFile {
    name: String,
    library: PinnedLinkLibrary,
}

#[derive(Debug)]
struct LibpqDirectorySnapshot {
    dir: PathBuf,
    files: Vec<LibpqSnapshotFile>,
    static_archive: Option<PathBuf>,
}

fn known_libpq_directory_entry(name: &str) -> bool {
    name == "libpq.a"
        || [
            LibpqLibraryFormat::WindowsMsvc,
            LibpqLibraryFormat::WindowsMingw,
            LibpqLibraryFormat::Linux,
            LibpqLibraryFormat::Darwin,
        ]
        .into_iter()
        .any(|format| libpq_library_name_priority(name, format).is_some())
}

fn snapshot_libpq_directory(dir: &Path) -> Result<Option<LibpqDirectorySnapshot>, KuError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(KuError::message(format!(
                "failed to inspect libpq directory '{}': {error}",
                dir.display()
            )))
        }
    };
    let mut files = Vec::new();
    let mut static_archive = None;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES {
            return Err(KuError::message(format!(
                "libpq directory '{}' exceeds the {MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES}-entry discovery limit\nhelp: set KU_PG_LIB to a small dedicated directory containing only the target-compatible shared/import libpq library",
                dir.display()
            )));
        }
        let entry = entry.map_err(|error| {
            KuError::message(format!(
                "failed to inspect an entry in libpq directory '{}': {error}",
                dir.display()
            ))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !known_libpq_directory_entry(name) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            KuError::message(format!(
                "failed to inspect libpq directory entry '{}': {error}",
                entry.path().display()
            ))
        })?;
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if name == "libpq.a" {
            static_archive = Some(path.clone());
        }
        let library = PinnedLinkLibrary::capture(&path, "libpq")?;
        if canonical_target_is_static_archive(&library) && !name.eq_ignore_ascii_case("libpq.dll.a")
        {
            static_archive = Some(library.path.clone());
        }
        files.push(LibpqSnapshotFile {
            name: name.to_string(),
            library,
        });
    }
    Ok(Some(LibpqDirectorySnapshot {
        dir: dir.to_path_buf(),
        files,
        static_archive,
    }))
}

fn find_libpq_library_in_snapshot(
    snapshot: &LibpqDirectorySnapshot,
    format: LibpqLibraryFormat,
) -> Option<PinnedLinkLibrary> {
    let mut candidates = snapshot
        .files
        .iter()
        .filter_map(|file| {
            let priority = libpq_library_name_priority(&file.name, format)?;
            if matches!(
                format,
                LibpqLibraryFormat::Linux | LibpqLibraryFormat::Darwin
            ) && canonical_target_is_static_archive(&file.library)
            {
                return None;
            }
            Some((priority, file.name.len(), &file.name, &file.library))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| {
                match (
                    libpq_library_version(left.2, format),
                    libpq_library_version(right.2, format),
                ) {
                    // Prefer the newest runtime SONAME when an unversioned
                    // development symlink is unavailable.
                    (Some(left), Some(right)) => compare_numeric_dotted(right, left),
                    _ => std::cmp::Ordering::Equal,
                }
            })
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });
    candidates
        .into_iter()
        .next()
        .map(|(_, _, _, library)| library.clone())
}

#[cfg(test)]
fn find_libpq_library(dir: &Path, format: LibpqLibraryFormat) -> Result<Option<PathBuf>, KuError> {
    Ok(snapshot_libpq_directory(dir)?
        .as_ref()
        .and_then(|snapshot| find_libpq_library_in_snapshot(snapshot, format))
        .map(|library| library.path))
}

fn static_libpq_error(static_archive: &Path) -> KuError {
    KuError::message(format!(
        "cannot link static libpq archive '{}': its target-specific transitive libraries cannot be inferred portably\nhelp: install a target-compatible shared libpq in KU_PG_LIB, or link the emitted C yourself with the complete dependency list reported by your libpq installation",
        static_archive.display()
    ))
}

fn libpq_library_in_snapshot(
    snapshot: &LibpqDirectorySnapshot,
    format: LibpqLibraryFormat,
) -> Result<Option<PinnedLinkLibrary>, KuError> {
    if let Some(library) = find_libpq_library_in_snapshot(snapshot, format) {
        if matches!(
            format,
            LibpqLibraryFormat::Linux | LibpqLibraryFormat::Darwin
        ) && library.has_ar_archive_magic("libpq")?
        {
            return Err(static_libpq_error(&library.path));
        }
        return Ok(Some(library));
    }
    if let Some(static_archive) = &snapshot.static_archive {
        return Err(static_libpq_error(static_archive));
    }
    Ok(None)
}

#[cfg(test)]
fn libpq_library_in_dir(
    dir: &Path,
    format: LibpqLibraryFormat,
) -> Result<Option<PathBuf>, KuError> {
    let Some(snapshot) = snapshot_libpq_directory(dir)? else {
        return Ok(None);
    };
    libpq_library_in_snapshot(&snapshot, format).map(|library| library.map(|library| library.path))
}

fn explicit_libpq_directory(
    configured: Option<OsString>,
) -> Result<Option<LibpqDirectorySnapshot>, KuError> {
    let Some(configured) = configured else {
        return Ok(None);
    };
    if configured.is_empty() {
        return Err(KuError::message(
            "KU_PG_LIB is set but empty\nhelp: set it to an absolute, dedicated directory containing the target-compatible shared/import libpq library",
        ));
    }
    let configured = PathBuf::from(configured);
    if !configured.is_absolute() {
        return Err(KuError::message(format!(
            "KU_PG_LIB must be an absolute directory, got '{}'",
            configured.display()
        )));
    }
    let dir = fs::canonicalize(&configured).map_err(|error| {
        KuError::message(format!(
            "failed to resolve KU_PG_LIB directory '{}': {error}",
            configured.display()
        ))
    })?;
    if !dir.is_dir() {
        return Err(KuError::message(format!(
            "KU_PG_LIB must name a directory, got '{}'",
            configured.display()
        )));
    }
    snapshot_libpq_directory(&dir)?.map_or_else(
        || {
            Err(KuError::message(format!(
                "KU_PG_LIB directory '{}' changed while being inspected; refusing to fall back",
                configured.display()
            )))
        },
        |snapshot| Ok(Some(snapshot)),
    )
}

fn libpq_library_from_explicit_directory(
    snapshot: &LibpqDirectorySnapshot,
    format: LibpqLibraryFormat,
) -> Result<PinnedLinkLibrary, KuError> {
    match libpq_library_in_snapshot(snapshot, format)? {
        Some(library) => Ok(library),
        None => Err(KuError::message(format!(
            "KU_PG_LIB directory '{}' does not contain a target-compatible shared/import libpq library",
            snapshot.dir.display()
        ))),
    }
}

#[cfg(test)]
fn explicit_libpq_library(
    configured: Option<OsString>,
    format: LibpqLibraryFormat,
) -> Result<Option<PathBuf>, KuError> {
    let Some(snapshot) = explicit_libpq_directory(configured)? else {
        return Ok(None);
    };
    libpq_library_from_explicit_directory(&snapshot, format).map(|library| Some(library.path))
}

fn libmysql_library_version(name: &str, format: LibpqLibraryFormat) -> Option<&str> {
    match format {
        LibpqLibraryFormat::Linux => ["libmysqlclient.so.", "libmariadb.so."]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix)),
        LibpqLibraryFormat::Darwin => {
            ["libmysqlclient.", "libmariadb."]
                .into_iter()
                .find_map(|prefix| {
                    name.strip_prefix(prefix)
                        .and_then(|value| value.strip_suffix(".dylib"))
                })
        }
        LibpqLibraryFormat::WindowsMsvc | LibpqLibraryFormat::WindowsMingw => None,
    }
}

fn libmysql_library_name_priority(name: &str, format: LibpqLibraryFormat) -> Option<usize> {
    match format {
        LibpqLibraryFormat::WindowsMsvc => {
            if name.eq_ignore_ascii_case("libmysql.lib") {
                Some(0)
            } else if name.eq_ignore_ascii_case("libmariadb.lib") {
                Some(1)
            } else {
                None
            }
        }
        LibpqLibraryFormat::WindowsMingw => {
            if name.eq_ignore_ascii_case("libmysql.dll.a") {
                Some(0)
            } else if name.eq_ignore_ascii_case("libmariadb.dll.a") {
                Some(1)
            } else {
                None
            }
        }
        LibpqLibraryFormat::Linux => {
            if name == "libmysqlclient.so" {
                Some(0)
            } else if name
                .strip_prefix("libmysqlclient.so.")
                .is_some_and(numeric_library_version)
            {
                Some(1)
            } else if name == "libmariadb.so" {
                Some(2)
            } else if name
                .strip_prefix("libmariadb.so.")
                .is_some_and(numeric_library_version)
            {
                Some(3)
            } else {
                None
            }
        }
        LibpqLibraryFormat::Darwin => {
            if name == "libmysqlclient.dylib" {
                Some(0)
            } else if name
                .strip_prefix("libmysqlclient.")
                .and_then(|value| value.strip_suffix(".dylib"))
                .is_some_and(numeric_library_version)
            {
                Some(1)
            } else if name == "libmariadb.dylib" {
                Some(2)
            } else if name
                .strip_prefix("libmariadb.")
                .and_then(|value| value.strip_suffix(".dylib"))
                .is_some_and(numeric_library_version)
            {
                Some(3)
            } else {
                None
            }
        }
    }
}

fn known_libmysql_directory_entry(name: &str) -> bool {
    ["libmysqlclient.a", "libmysql.a", "libmariadb.a"].contains(&name)
        || [
            LibpqLibraryFormat::WindowsMsvc,
            LibpqLibraryFormat::WindowsMingw,
            LibpqLibraryFormat::Linux,
            LibpqLibraryFormat::Darwin,
        ]
        .into_iter()
        .any(|format| libmysql_library_name_priority(name, format).is_some())
}

fn mysql_client_family_from_canonical_path(path: &Path) -> Result<MysqlClientFamily, KuError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            KuError::message(format!(
                "selected MySQL client library '{}' has no portable canonical file name",
                path.display()
            ))
        })?;
    let lower = name.to_ascii_lowercase();
    let matches_stem = |stem: &str| {
        dynamic_library_basename_matches(&lower, stem)
            || [".lib", ".dll.a", ".a"]
                .iter()
                .any(|suffix| lower == format!("{stem}{suffix}"))
    };
    let mariadb = ["libmariadbclient", "libmariadb", "mariadbclient", "mariadb"]
        .iter()
        .any(|stem| matches_stem(stem));
    let mysql = ["libmysqlclient", "libmysql", "mysqlclient"]
        .iter()
        .any(|stem| matches_stem(stem));
    if mariadb {
        Ok(MysqlClientFamily::Mariadb)
    } else if mysql {
        Ok(MysqlClientFamily::Mysql)
    } else {
        Err(KuError::message(format!(
            "cannot determine MySQL/MariaDB family from canonical library target '{}'\nhelp: point KU_MYSQL_LIB at a library whose resolved file name identifies libmysqlclient/libmysql or libmariadb; Ku does not trust a symlink alias as ABI evidence",
            path.display()
        )))
    }
}

#[derive(Debug)]
struct LibmysqlSnapshotFile {
    name: String,
    library: PinnedLinkLibrary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlClientFamily {
    Mysql,
    Mariadb,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MysqlRuntimeDependency {
    family: MysqlClientFamily,
    loader_name: Vec<u8>,
}

fn mysql_runtime_dependency(
    library: &PinnedLinkLibrary,
    format: LibpqLibraryFormat,
) -> Result<MysqlRuntimeDependency, KuError> {
    let before = link_library_identity(&library.handle).map_err(|error| {
        KuError::message(format!(
            "failed to identify selected MySQL client library '{}': {error}",
            library.path.display()
        ))
    })?;
    if !same_open_link_library_contents(&before, &library.identity) {
        return Err(KuError::message(format!(
            "selected MySQL client library '{}' changed before loader identity inspection",
            library.path.display()
        )));
    }
    let mut file = library.handle.try_clone().map_err(|error| {
        KuError::message(format!(
            "failed to inspect selected MySQL client library '{}': {error}",
            library.path.display()
        ))
    })?;
    let loader_name = match format {
        LibpqLibraryFormat::Linux => read_elf_dynamic_metadata(&mut file, before.length)
            .map_err(|error| {
                KuError::message(format!(
                    "selected MySQL client library '{}' has invalid ELF loader metadata: {error}",
                    library.path.display()
                ))
            })?
            .soname
            .ok_or_else(|| {
                KuError::message(format!(
                    "selected MySQL client library '{}' has no bounded DT_SONAME; refusing a private staging path runtime dependency",
                    library.path.display()
                ))
            })?,
        LibpqLibraryFormat::Darwin => read_macho_dynamic_metadata(&mut file, before.length)
            .map_err(|error| {
                KuError::message(format!(
                    "selected MySQL client library '{}' has invalid Mach-O loader metadata: {error}",
                    library.path.display()
                ))
            })?
            .install_name
            .ok_or_else(|| {
                KuError::message(format!(
                    "selected MySQL client library '{}' has no bounded LC_ID_DYLIB install name; refusing a private staging path runtime dependency",
                    library.path.display()
                ))
            })?,
        LibpqLibraryFormat::WindowsMsvc | LibpqLibraryFormat::WindowsMingw => {
            read_database_import_loader_name(
                &mut file,
                before.length,
                &[DynamicLibraryFamily::Mysql, DynamicLibraryFamily::Mariadb],
                "MySQL/MariaDB",
            )
            .map_err(|error| {
                KuError::message(format!(
                    "selected MySQL client import library '{}' is invalid: {error}",
                    library.path.display()
                ))
            })?
        }
    };
    let after = link_library_identity(&library.handle).map_err(|error| {
        KuError::message(format!(
            "failed to re-identify selected MySQL client library '{}': {error}",
            library.path.display()
        ))
    })?;
    if !same_open_link_library_contents(&after, &before) {
        return Err(KuError::message(format!(
            "selected MySQL client library '{}' changed during loader identity inspection",
            library.path.display()
        )));
    }
    if dynamic_dependency_references_private_staging(&loader_name) {
        return Err(KuError::message(format!(
            "selected MySQL client library '{}' records a private Ku staging path as its loader identity",
            library.path.display()
        )));
    }
    let loader_family = if dynamic_library_matches(&loader_name, DynamicLibraryFamily::Mariadb) {
        MysqlClientFamily::Mariadb
    } else if dynamic_library_matches(&loader_name, DynamicLibraryFamily::Mysql) {
        MysqlClientFamily::Mysql
    } else {
        return Err(KuError::message(format!(
            "selected MySQL client library '{}' has an unsupported loader identity '{}'",
            library.path.display(),
            String::from_utf8_lossy(&loader_name)
        )));
    };
    let canonical_family = mysql_client_family_from_canonical_path(&library.path)?;
    if loader_family != canonical_family {
        return Err(KuError::message(format!(
            "selected MySQL client library '{}' has conflicting canonical and loader families",
            library.path.display()
        )));
    }
    Ok(MysqlRuntimeDependency {
        family: loader_family,
        loader_name,
    })
}

fn read_database_import_loader_name(
    file: &mut fs::File,
    file_len: u64,
    families: &[DynamicLibraryFamily],
    label: &str,
) -> Result<Vec<u8>, String> {
    if !(8..=MAX_IMPORT_LIBRARY_INSPECTION_BYTES).contains(&file_len) {
        return Err(format!(
            "Windows import library size is outside the 8..={MAX_IMPORT_LIBRARY_INSPECTION_BYTES} byte inspection bound"
        ));
    }
    let size = usize::try_from(file_len)
        .map_err(|_| "Windows import library does not fit this host".to_string())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|error| format!("failed to reserve bounded import library bytes: {error}"))?;
    bytes.resize(size, 0);
    read_binary_at(file, 0, &mut bytes, "Windows import library")?;
    if !matches!(bytes.get(..8), Some(b"!<arch>\n" | b"!<thin>\n")) {
        return Err(
            "expected a COFF import archive, not a static or renamed raw library".to_string(),
        );
    }
    let mut names = Vec::<Vec<u8>>::new();
    for value in bytes.split(|byte| *byte == 0) {
        let basename = value
            .rsplit(|byte| matches!(byte, b'/' | b'\\'))
            .next()
            .unwrap_or(value);
        let Some(lower) = std::str::from_utf8(basename)
            .ok()
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };
        if !lower.ends_with(".dll")
            || !families
                .iter()
                .any(|family| dynamic_library_matches(lower.as_bytes(), *family))
        {
            continue;
        }
        if !names.iter().any(|name| name == lower.as_bytes()) {
            names.push(lower.into_bytes());
            if names.len() > 1 {
                return Err(format!(
                    "Windows import library names multiple {label} loader targets"
                ));
            }
        }
    }
    names
        .pop()
        .ok_or_else(|| format!("Windows import library has no bounded {label} DLL target"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LibpqRuntimeDependency {
    loader_name: Vec<u8>,
}

fn libpq_runtime_dependency(
    library: &PinnedLinkLibrary,
    format: LibpqLibraryFormat,
) -> Result<LibpqRuntimeDependency, KuError> {
    let before = link_library_identity(&library.handle).map_err(|error| {
        KuError::message(format!(
            "failed to identify selected libpq library '{}': {error}",
            library.path.display()
        ))
    })?;
    if !same_open_link_library_contents(&before, &library.identity) {
        return Err(KuError::message(format!(
            "selected libpq library '{}' changed before loader identity inspection",
            library.path.display()
        )));
    }
    let mut file = library.handle.try_clone().map_err(|error| {
        KuError::message(format!(
            "failed to inspect selected libpq library '{}': {error}",
            library.path.display()
        ))
    })?;
    let loader_name = match format {
        LibpqLibraryFormat::Linux => read_elf_dynamic_metadata(&mut file, before.length)
            .map_err(|error| {
                KuError::message(format!(
                    "selected libpq library '{}' has invalid ELF loader metadata: {error}",
                    library.path.display()
                ))
            })?
            .soname
            .ok_or_else(|| {
                KuError::message(format!(
                    "selected libpq library '{}' has no bounded DT_SONAME; refusing a private staging path runtime dependency",
                    library.path.display()
                ))
            })?,
        LibpqLibraryFormat::Darwin => read_macho_dynamic_metadata(&mut file, before.length)
            .map_err(|error| {
                KuError::message(format!(
                    "selected libpq library '{}' has invalid Mach-O loader metadata: {error}",
                    library.path.display()
                ))
            })?
            .install_name
            .ok_or_else(|| {
                KuError::message(format!(
                    "selected libpq library '{}' has no bounded LC_ID_DYLIB install name; refusing a private staging path runtime dependency",
                    library.path.display()
                ))
            })?,
        LibpqLibraryFormat::WindowsMsvc | LibpqLibraryFormat::WindowsMingw => {
            read_database_import_loader_name(
                &mut file,
                before.length,
                &[DynamicLibraryFamily::Libpq],
                "libpq",
            )
            .map_err(|error| {
                KuError::message(format!(
                    "selected libpq import library '{}' is invalid: {error}",
                    library.path.display()
                ))
            })?
        }
    };
    let after = link_library_identity(&library.handle).map_err(|error| {
        KuError::message(format!(
            "failed to re-identify selected libpq library '{}': {error}",
            library.path.display()
        ))
    })?;
    if !same_open_link_library_contents(&after, &before) {
        return Err(KuError::message(format!(
            "selected libpq library '{}' changed during loader identity inspection",
            library.path.display()
        )));
    }
    if dynamic_dependency_references_private_staging(&loader_name)
        || !dynamic_library_matches(&loader_name, DynamicLibraryFamily::Libpq)
    {
        return Err(KuError::message(format!(
            "selected libpq library '{}' has an unsafe or unsupported loader identity '{}'",
            library.path.display(),
            String::from_utf8_lossy(&loader_name)
        )));
    }
    let canonical_name = library
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            KuError::message(format!(
                "selected libpq library '{}' has no portable canonical file name",
                library.path.display()
            ))
        })?;
    let canonical_matches = match format {
        LibpqLibraryFormat::Linux | LibpqLibraryFormat::Darwin => {
            dynamic_library_matches(canonical_name.as_bytes(), DynamicLibraryFamily::Libpq)
        }
        LibpqLibraryFormat::WindowsMsvc => {
            matches!(canonical_name.as_str(), "libpqdll.lib" | "libpq.lib")
        }
        LibpqLibraryFormat::WindowsMingw => canonical_name == "libpq.dll.a",
    };
    if !canonical_matches {
        return Err(KuError::message(format!(
            "selected libpq library '{}' has a canonical target name that conflicts with its requested ABI",
            library.path.display()
        )));
    }
    Ok(LibpqRuntimeDependency { loader_name })
}

#[derive(Debug, Clone)]
struct SelectedMysqlLibrary {
    library: PinnedLinkLibrary,
}

#[derive(Debug)]
struct LibmysqlDirectorySnapshot {
    dir: PathBuf,
    files: Vec<LibmysqlSnapshotFile>,
    static_archives: Vec<PathBuf>,
}

fn snapshot_libmysql_directory(dir: &Path) -> Result<Option<LibmysqlDirectorySnapshot>, KuError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(KuError::message(format!(
                "failed to inspect MySQL client library directory '{}': {error}",
                dir.display()
            )))
        }
    };
    let mut files = Vec::new();
    let mut static_archives = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES {
            return Err(KuError::message(format!(
                "MySQL client library directory '{}' exceeds the {MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES}-entry discovery limit\nhelp: set KU_MYSQL_LIB to a small dedicated directory containing only target-compatible shared/import client libraries",
                dir.display()
            )));
        }
        let entry = entry.map_err(|error| {
            KuError::message(format!(
                "failed to inspect an entry in MySQL client library directory '{}': {error}",
                dir.display()
            ))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !known_libmysql_directory_entry(name) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            KuError::message(format!(
                "failed to inspect MySQL client library entry '{}': {error}",
                entry.path().display()
            ))
        })?;
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let library = PinnedLinkLibrary::capture(&path, "MySQL client")?;
        mysql_client_family_from_canonical_path(&library.path)?;
        if (name.ends_with(".a") && !name.ends_with(".dll.a"))
            || (canonical_target_is_static_archive(&library) && !name.ends_with(".dll.a"))
        {
            static_archives.push(library.path.clone());
        }
        files.push(LibmysqlSnapshotFile {
            name: name.to_string(),
            library,
        });
    }
    static_archives.sort();
    static_archives.dedup();
    Ok(Some(LibmysqlDirectorySnapshot {
        dir: dir.to_path_buf(),
        files,
        static_archives,
    }))
}

fn find_libmysql_library_in_snapshot(
    snapshot: &LibmysqlDirectorySnapshot,
    format: LibpqLibraryFormat,
) -> Option<SelectedMysqlLibrary> {
    let mut candidates = snapshot
        .files
        .iter()
        .filter_map(|file| {
            let priority = libmysql_library_name_priority(&file.name, format)?;
            if matches!(
                format,
                LibpqLibraryFormat::Linux | LibpqLibraryFormat::Darwin
            ) && canonical_target_is_static_archive(&file.library)
            {
                return None;
            }
            Some((priority, file.name.len(), &file.name, file))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| {
                match (
                    libmysql_library_version(left.2, format),
                    libmysql_library_version(right.2, format),
                ) {
                    (Some(left), Some(right)) => compare_numeric_dotted(right, left),
                    _ => std::cmp::Ordering::Equal,
                }
            })
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });
    candidates
        .into_iter()
        .next()
        .map(|(_, _, _, file)| SelectedMysqlLibrary {
            library: file.library.clone(),
        })
}

fn static_libmysql_error(static_archive: &Path) -> KuError {
    KuError::message(format!(
        "cannot link static MySQL client archive '{}': its target-specific transitive libraries cannot be inferred portably\nhelp: install a target-compatible shared MySQL/MariaDB client library in KU_MYSQL_LIB, or link the emitted C yourself with the complete dependency list",
        static_archive.display()
    ))
}

fn libmysql_library_from_directory(
    snapshot: &LibmysqlDirectorySnapshot,
    format: LibpqLibraryFormat,
) -> Result<SelectedMysqlLibrary, KuError> {
    if let Some(library) = find_libmysql_library_in_snapshot(snapshot, format) {
        if matches!(
            format,
            LibpqLibraryFormat::Linux | LibpqLibraryFormat::Darwin
        ) && library.library.has_ar_archive_magic("MySQL client")?
        {
            return Err(static_libmysql_error(&library.library.path));
        }
        return Ok(library);
    }
    if let Some(static_archive) = snapshot.static_archives.first() {
        return Err(static_libmysql_error(static_archive));
    }
    Err(KuError::message(format!(
        "MySQL client library directory '{}' does not contain a target-compatible shared/import library",
        snapshot.dir.display()
    )))
}

fn missing_shared_libmysql_error() -> KuError {
    KuError::message(
        "native MySQL linking requires an exact shared/import client library\nhelp: set KU_MYSQL_LIB to an absolute, dedicated directory containing the target-compatible shared/import library; Ku does not fall back to an unverified compiler search path",
    )
}

/// Sort installation library directories by the numeric components in their
/// parent directory name. Plain lexical sorting can select an older client ABI.
fn sort_install_dirs_by_version(dirs: &mut [PathBuf]) {
    dirs.sort_by(|left, right| {
        let key = |path: &Path| {
            path.parent()
                .and_then(Path::file_name)
                .map(|name| numeric_version_key(&name.to_string_lossy()))
                .unwrap_or_default()
        };
        key(left).cmp(&key(right)).then_with(|| left.cmp(right))
    });
}

fn numeric_version_key(name: &str) -> Vec<u64> {
    name.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn validate_libpq_link_mode(needs_libpq: bool, static_link: bool) -> Result<(), KuError> {
    if needs_libpq && static_link {
        return Err(KuError::message(
            "native C build cannot safely link std.pg with --static: libpq static archives require target-specific transitive libraries that Ku cannot infer portably\nhelp: omit --static and provide a target-compatible shared libpq through KU_PG_LIB, or link the emitted C yourself with the complete dependency list reported by your libpq installation",
        ));
    }
    Ok(())
}

fn validate_libmysql_link_mode(needs_libmysql: bool, static_link: bool) -> Result<(), KuError> {
    if needs_libmysql && static_link {
        return Err(KuError::message(
            "native C build cannot safely link std.mysql with --static: MySQL/MariaDB static archives require target-specific transitive libraries that Ku cannot infer portably\nhelp: omit --static and provide a target-compatible shared/import client library through KU_MYSQL_LIB, or link the emitted C yourself with the complete dependency list",
        ));
    }
    Ok(())
}

fn missing_shared_libpq_error() -> KuError {
    KuError::message(
        "native PostgreSQL linking requires an exact shared/import libpq library\nhelp: set KU_PG_LIB to an absolute, dedicated directory containing the target-compatible shared/import library; Ku does not fall back to an unverified compiler search path",
    )
}

const LINK_STAGING_PREFIX: &str = ".ku-link-";
const MAX_LINK_STAGING_ATTEMPTS: usize = 8;

struct LinkOutputStaging {
    directory: PathBuf,
    artifact: PathBuf,
    marker: PathBuf,
    marker_identity: LinkLibraryIdentity,
    output: PathBuf,
    initial_destination: Option<LinkLibraryIdentity>,
}

impl LinkOutputStaging {
    fn create(output: &Path) -> Result<Self, KuError> {
        let parent = link_output_directory(output);
        let file_name = output.file_name().ok_or_else(|| {
            KuError::message(format!(
                "native output '{}' has no file name",
                output.display()
            ))
        })?;
        let initial_destination = capture_link_destination(output)?;
        for _ in 0..MAX_LINK_STAGING_ATTEMPTS {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| {
                KuError::message(format!(
                    "failed to reserve private native link staging: secure random failed: {error}"
                ))
            })?;
            let nonce = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let directory = parent.join(format!(
                "{LINK_STAGING_PREFIX}{}-{nonce}",
                std::process::id()
            ));
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = fs::DirBuilder::new();
            match builder.create(&directory) {
                Ok(()) => {
                    let artifact = directory.join(file_name);
                    let marker = directory.join(".owner");
                    let marker_result = (|| -> io::Result<LinkLibraryIdentity> {
                        let mut options = fs::OpenOptions::new();
                        options.create_new(true).write(true).read(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            options.mode(0o600);
                        }
                        let mut file = options.open(&marker)?;
                        file.write_all(&random)?;
                        file.flush()?;
                        link_library_identity(&file)
                    })();
                    let marker_identity = match marker_result {
                        Ok(identity) => identity,
                        Err(error) => {
                            let _ = fs::remove_file(&marker);
                            let _ = fs::remove_dir(&directory);
                            return Err(KuError::message(format!(
                                "failed to establish private native link staging ownership in '{}': {error}",
                                parent.display()
                            )));
                        }
                    };
                    return Ok(Self {
                        directory,
                        artifact,
                        marker,
                        marker_identity,
                        output: output.to_path_buf(),
                        initial_destination,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(KuError::message(format!(
                        "failed to create private native link staging in '{}': {error}",
                        parent.display()
                    )));
                }
            }
        }
        Err(KuError::message(format!(
            "failed to reserve a unique private native link staging directory in '{}'",
            parent.display()
        )))
    }

    fn path(&self) -> &Path {
        &self.artifact
    }
}

fn capture_link_destination(output: &Path) -> Result<Option<LinkLibraryIdentity>, KuError> {
    match fs::symlink_metadata(output) {
        Ok(metadata) if is_plain_regular_file(&metadata) => {
            let file = fs::File::open(output).map_err(|error| {
                KuError::message(format!(
                    "failed to open previous native output '{}': {error}",
                    output.display()
                ))
            })?;
            let identity = link_library_identity(&file).map_err(|error| {
                KuError::message(format!(
                    "failed to identify previous native output '{}': {error}",
                    output.display()
                ))
            })?;
            if identity.length != metadata.len() {
                return Err(KuError::message(format!(
                    "previous native output '{}' changed while it was inspected",
                    output.display()
                )));
            }
            Ok(Some(identity))
        }
        Ok(_) => Err(KuError::message(format!(
            "native output destination '{}' is not a regular file",
            output.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(KuError::message(format!(
            "failed to inspect previous native output '{}': {error}",
            output.display()
        ))),
    }
}

fn verify_link_destination_unchanged(
    output: &Path,
    expected: &Option<LinkLibraryIdentity>,
) -> Result<(), KuError> {
    let current = capture_link_destination(output)?;
    let unchanged = match (expected, current) {
        (None, None) => true,
        (Some(expected), Some(current)) => same_open_link_library_contents(expected, &current),
        _ => false,
    };
    if unchanged {
        Ok(())
    } else {
        Err(KuError::message(format!(
            "native output destination '{}' changed while the artifact was being built; refusing to overwrite it",
            output.display()
        )))
    }
}

impl Drop for LinkOutputStaging {
    fn drop(&mut self) {
        // Recursively clean only the exact random directory whose private
        // marker still names the file created by this guard. If an untrusted
        // output owner replaces the directory or marker, fail closed and leave
        // it untouched; never scan the surrounding user output directory.
        let owned = path_is_plain_directory(&self.directory)
            && fs::symlink_metadata(&self.marker)
                .is_ok_and(|metadata| is_plain_regular_file(&metadata))
            && fs::File::open(&self.marker)
                .ok()
                .and_then(|file| link_library_identity(&file).ok())
                .is_some_and(|identity| {
                    same_open_link_library_contents(&identity, &self.marker_identity)
                });
        if owned {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn is_plain_regular_file(metadata: &fs::Metadata) -> bool {
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    #[cfg(not(windows))]
    {
        true
    }
}

fn path_is_plain_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| is_plain_regular_file(&metadata))
}

fn path_is_plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        if !metadata.file_type().is_dir() {
            return false;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;

            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        }
        #[cfg(not(windows))]
        {
            true
        }
    })
}

fn validate_native_output_name(output: &Path) -> Result<(), KuError> {
    let reserved = output
        .file_name()
        .map(|name| name.to_string_lossy())
        .is_some_and(|name| {
            name.get(..LINK_STAGING_PREFIX.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(LINK_STAGING_PREFIX))
        });
    if reserved {
        return Err(KuError::message(format!(
            "native output '{}' uses the reserved {LINK_STAGING_PREFIX} staging namespace",
            output.display()
        )));
    }
    Ok(())
}

fn link_output_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn binary_range_end(offset: u64, size: u64, file_len: u64, what: &str) -> Result<u64, String> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| format!("{what} range overflows"))?;
    if end > file_len {
        return Err(format!("{what} extends past the linked output"));
    }
    Ok(end)
}

fn read_binary_at(
    file: &mut fs::File,
    offset: u64,
    buffer: &mut [u8],
    what: &str,
) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|err| format!("failed to seek to {what}: {err}"))?;
    file.read_exact(buffer)
        .map_err(|err| format!("{what} is truncated: {err}"))
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn verify_native_binary_target_file(
    file: &mut fs::File,
    file_len: u64,
    target: &BuildTarget,
) -> Result<(), String> {
    match target.binary_format {
        NativeBinaryFormat::ElfX86_64 => verify_elf_x86_64(file, file_len)?,
        NativeBinaryFormat::PeX86_64 => verify_pe_x86_64(file, file_len)?,
        NativeBinaryFormat::MachOArm64 => verify_macho_arm64_macos(file, file_len)?,
    }
    Ok(())
}

#[cfg(test)]
fn verify_native_binary_target(output: &Path, target: &BuildTarget) -> Result<(), String> {
    let mut file =
        fs::File::open(output).map_err(|err| format!("failed to open linked output: {err}"))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("failed to inspect linked output: {err}"))?;
    if !metadata.is_file() {
        return Err("linked output is not a regular file".to_string());
    }
    verify_native_binary_target_file(&mut file, metadata.len(), target)
}

const MAX_DYNAMIC_DEPENDENCIES: usize = 4_096;
const MAX_DYNAMIC_STRING_TABLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DYNAMIC_DEPENDENCY_NAME_BYTES: usize = 4_096;
const MAX_PE_OPTIONAL_HEADER_BYTES: u64 = 4 * 1024;

fn verify_native_binary_dynamic_dependencies_file(
    file: &mut fs::File,
    file_len: u64,
    target: &BuildTarget,
    features: CSourceFeatures,
    libpq_requirement: Option<&LibpqRuntimeDependency>,
    mysql_requirement: Option<&MysqlRuntimeDependency>,
) -> Result<(), String> {
    if !features.libpq && !features.libmysql {
        return Ok(());
    }
    let names = match target.binary_format {
        NativeBinaryFormat::ElfX86_64 => read_elf_dynamic_dependencies(file, file_len)?,
        NativeBinaryFormat::PeX86_64 => read_pe_dynamic_dependencies(file, file_len)?,
        NativeBinaryFormat::MachOArm64 => read_macho_dynamic_dependencies(file, file_len)?,
    };
    if names
        .iter()
        .any(|name| dynamic_dependency_references_private_staging(name))
    {
        return Err(
            "linked output records a private Ku staging path as a runtime dependency".to_string(),
        );
    }
    if features.libpq {
        let requirement = libpq_requirement.ok_or_else(|| {
            "native PostgreSQL dependency verification has no selected libpq loader identity"
                .to_string()
        })?;
        let libpq_names = names
            .iter()
            .filter(|name| dynamic_library_matches(name, DynamicLibraryFamily::Libpq))
            .collect::<Vec<_>>();
        if libpq_names.is_empty() {
            return Err(
                "linked output has no dynamic libpq dependency; refusing a static or unresolved fallback"
                    .to_string(),
            );
        }
        if libpq_names.iter().any(|name| {
            !dynamic_dependency_matches_selected_loader(
                name,
                &requirement.loader_name,
                target.binary_format,
            )
        }) {
            return Err(format!(
                "linked output imports a libpq loader target that differs from the selected library '{}'",
                String::from_utf8_lossy(&requirement.loader_name)
            ));
        }
    }
    if features.libmysql {
        let requirement = mysql_requirement.ok_or_else(|| {
            "native MySQL dependency verification has no selected client family".to_string()
        })?;
        let (expected, other, label) = match requirement.family {
            MysqlClientFamily::Mysql => (
                DynamicLibraryFamily::Mysql,
                DynamicLibraryFamily::Mariadb,
                "MySQL",
            ),
            MysqlClientFamily::Mariadb => (
                DynamicLibraryFamily::Mariadb,
                DynamicLibraryFamily::Mysql,
                "MariaDB",
            ),
        };
        if names
            .iter()
            .any(|name| dynamic_library_matches(name, other))
        {
            return Err(
                "linked output mixes MySQL and MariaDB dynamic client dependencies".to_string(),
            );
        }
        let client_names = names
            .iter()
            .filter(|name| dynamic_library_matches(name, expected))
            .collect::<Vec<_>>();
        if client_names.is_empty() {
            return Err(format!(
                "linked output has no dynamic {label} client dependency matching the selected import library; refusing a static, cross-family, or unresolved fallback"
            ));
        }
        if client_names.iter().any(|name| {
            !dynamic_dependency_matches_selected_loader(
                name,
                &requirement.loader_name,
                target.binary_format,
            )
        }) {
            return Err(format!(
                "linked output imports a {label} loader target that differs from the selected client library '{}'",
                String::from_utf8_lossy(&requirement.loader_name)
            ));
        }
    }
    Ok(())
}

fn dynamic_dependency_matches_selected_loader(
    actual: &[u8],
    expected: &[u8],
    format: NativeBinaryFormat,
) -> bool {
    match format {
        NativeBinaryFormat::PeX86_64 => {
            let actual = actual
                .rsplit(|byte| matches!(byte, b'/' | b'\\'))
                .next()
                .unwrap_or(actual);
            let expected = expected
                .rsplit(|byte| matches!(byte, b'/' | b'\\'))
                .next()
                .unwrap_or(expected);
            actual.eq_ignore_ascii_case(expected)
        }
        NativeBinaryFormat::ElfX86_64 | NativeBinaryFormat::MachOArm64 => actual == expected,
    }
}

fn dynamic_dependency_references_private_staging(name: &[u8]) -> bool {
    name.split(|byte| matches!(byte, b'/' | b'\\'))
        .any(|component| {
            component
                .get(..LINK_STAGING_PREFIX.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(LINK_STAGING_PREFIX.as_bytes()))
        })
}

#[cfg(test)]
fn verify_native_binary_dynamic_dependencies(
    output: &Path,
    target: &BuildTarget,
    features: CSourceFeatures,
    libpq_requirement: Option<&LibpqRuntimeDependency>,
    mysql_requirement: Option<&MysqlRuntimeDependency>,
) -> Result<(), String> {
    let mut file =
        fs::File::open(output).map_err(|err| format!("failed to open linked output: {err}"))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("failed to inspect linked output: {err}"))?;
    if !metadata.is_file() {
        return Err("linked output is not a regular file".to_string());
    }
    verify_native_binary_dynamic_dependencies_file(
        &mut file,
        metadata.len(),
        target,
        features,
        libpq_requirement,
        mysql_requirement,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DynamicLibraryFamily {
    Libpq,
    Mysql,
    Mariadb,
}

fn dynamic_library_matches(name: &[u8], family: DynamicLibraryFamily) -> bool {
    let basename = name
        .rsplit(|byte| matches!(byte, b'/' | b'\\'))
        .next()
        .unwrap_or(name);
    let Ok(basename) = std::str::from_utf8(basename) else {
        return false;
    };
    let lower = basename.to_ascii_lowercase();
    let stems: &[&str] = match family {
        DynamicLibraryFamily::Libpq => &["libpq"],
        DynamicLibraryFamily::Mysql => &["libmysqlclient", "libmysql", "mysqlclient"],
        DynamicLibraryFamily::Mariadb => {
            &["libmariadbclient", "libmariadb", "mariadbclient", "mariadb"]
        }
    };
    stems
        .iter()
        .any(|stem| dynamic_library_basename_matches(&lower, stem))
}

fn dynamic_library_basename_matches(basename: &str, stem: &str) -> bool {
    let Some(suffix) = basename.strip_prefix(stem) else {
        return false;
    };
    if matches!(suffix, ".dll" | ".so" | ".dylib") {
        return true;
    }
    if let Some(version) = suffix.strip_prefix(".so.") {
        return numeric_library_version(version);
    }
    suffix
        .strip_prefix('.')
        .and_then(|value| value.strip_suffix(".dylib"))
        .is_some_and(numeric_library_version)
}

#[derive(Clone, Copy)]
struct ElfFileSegment {
    virtual_address: u64,
    file_offset: u64,
    file_size: u64,
}

struct ElfDynamicMetadata {
    dependencies: Vec<Vec<u8>>,
    soname: Option<Vec<u8>>,
}

fn read_elf_dynamic_dependencies(
    file: &mut fs::File,
    file_len: u64,
) -> Result<Vec<Vec<u8>>, String> {
    Ok(read_elf_dynamic_metadata(file, file_len)?.dependencies)
}

fn read_elf_dynamic_metadata(
    file: &mut fs::File,
    file_len: u64,
) -> Result<ElfDynamicMetadata, String> {
    const ELF_HEADER_SIZE: usize = 64;
    const ELF_PROGRAM_HEADER_SIZE: u64 = 56;
    const MAX_PROGRAM_HEADERS: u16 = 4_096;
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: u64 = 0;
    const DT_NEEDED: u64 = 1;
    const DT_STRTAB: u64 = 5;
    const DT_STRSZ: u64 = 10;
    const DT_SONAME: u64 = 14;

    let mut header = [0u8; ELF_HEADER_SIZE];
    read_binary_at(file, 0, &mut header, "ELF64 header")?;
    if header[..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err("expected a little-endian ELF64 executable".to_string());
    }
    let program_offset = le_u64(&header, 32);
    let program_entry_size = le_u16(&header, 54) as u64;
    let program_count = le_u16(&header, 56);
    if program_entry_size != ELF_PROGRAM_HEADER_SIZE
        || program_count == 0
        || program_count > MAX_PROGRAM_HEADERS
    {
        return Err("ELF program-header table is invalid".to_string());
    }
    let table_size = program_entry_size
        .checked_mul(program_count as u64)
        .ok_or_else(|| "ELF program table size overflows".to_string())?;
    binary_range_end(program_offset, table_size, file_len, "ELF program table")?;

    let mut loads = Vec::new();
    let mut dynamic = None;
    for index in 0..program_count as u64 {
        let offset = program_offset + index * program_entry_size;
        let mut program = [0u8; ELF_PROGRAM_HEADER_SIZE as usize];
        read_binary_at(file, offset, &mut program, "ELF program header")?;
        let kind = le_u32(&program, 0);
        let file_offset = le_u64(&program, 8);
        let virtual_address = le_u64(&program, 16);
        let file_size = le_u64(&program, 32);
        if matches!(kind, PT_LOAD | PT_DYNAMIC) {
            binary_range_end(file_offset, file_size, file_len, "ELF segment")?;
        }
        match kind {
            PT_LOAD => loads.push(ElfFileSegment {
                virtual_address,
                file_offset,
                file_size,
            }),
            PT_DYNAMIC => {
                if dynamic.replace((file_offset, file_size)).is_some() {
                    return Err("ELF executable has multiple PT_DYNAMIC segments".to_string());
                }
            }
            _ => {}
        }
    }
    let Some((dynamic_offset, dynamic_size)) = dynamic else {
        return Ok(ElfDynamicMetadata {
            dependencies: Vec::new(),
            soname: None,
        });
    };
    if dynamic_size == 0
        || dynamic_size > MAX_DYNAMIC_STRING_TABLE_BYTES
        || !dynamic_size.is_multiple_of(16)
    {
        return Err("ELF PT_DYNAMIC size is invalid or exceeds the parser limit".to_string());
    }

    let mut string_address = None;
    let mut string_size = None;
    let mut needed = Vec::new();
    let mut soname = None;
    let mut terminated = false;
    for index in 0..dynamic_size / 16 {
        let mut entry = [0u8; 16];
        read_binary_at(
            file,
            dynamic_offset + index * 16,
            &mut entry,
            "ELF dynamic entry",
        )?;
        let tag = le_u64(&entry, 0);
        let value = le_u64(&entry, 8);
        match tag {
            DT_NULL => {
                terminated = true;
                break;
            }
            DT_NEEDED => {
                if needed.len() >= MAX_DYNAMIC_DEPENDENCIES {
                    return Err("ELF dependency table exceeds the parser limit".to_string());
                }
                needed.push(value);
            }
            DT_STRTAB => match string_address {
                Some(previous) if previous != value => {
                    return Err("ELF dynamic table has conflicting DT_STRTAB values".to_string())
                }
                _ => string_address = Some(value),
            },
            DT_STRSZ => match string_size {
                Some(previous) if previous != value => {
                    return Err("ELF dynamic table has conflicting DT_STRSZ values".to_string())
                }
                _ => string_size = Some(value),
            },
            DT_SONAME => match soname {
                Some(previous) if previous != value => {
                    return Err("ELF dynamic table has conflicting DT_SONAME values".to_string())
                }
                _ => soname = Some(value),
            },
            _ => {}
        }
    }
    if !terminated {
        return Err("ELF dynamic table has no bounded DT_NULL terminator".to_string());
    }
    if needed.is_empty() && soname.is_none() {
        return Ok(ElfDynamicMetadata {
            dependencies: Vec::new(),
            soname: None,
        });
    }
    let string_address = string_address
        .ok_or_else(|| "ELF dynamic dependencies are missing the DT_STRTAB address".to_string())?;
    let string_size = string_size
        .ok_or_else(|| "ELF dynamic dependencies are missing the DT_STRSZ bound".to_string())?;
    if string_size == 0 || string_size > MAX_DYNAMIC_STRING_TABLE_BYTES {
        return Err("ELF dynamic string table exceeds the parser limit".to_string());
    }
    let string_offset = elf_virtual_file_offset(string_address, string_size, &loads, file_len)?;
    let string_size = usize::try_from(string_size)
        .map_err(|_| "ELF dynamic string table does not fit this host".to_string())?;
    let mut strings = Vec::new();
    strings
        .try_reserve_exact(string_size)
        .map_err(|error| format!("failed to reserve bounded ELF string table: {error}"))?;
    strings.resize(string_size, 0);
    read_binary_at(
        file,
        string_offset,
        &mut strings,
        "ELF dynamic string table",
    )?;
    let read_string = |offset: u64, kind: &str| -> Result<Vec<u8>, String> {
        let start = usize::try_from(offset)
            .ok()
            .filter(|offset| *offset < strings.len())
            .ok_or_else(|| format!("ELF {kind} offset is outside DT_STRSZ"))?;
        let available = &strings[start..];
        let end = available
            .iter()
            .take(MAX_DYNAMIC_DEPENDENCY_NAME_BYTES + 1)
            .position(|byte| *byte == 0)
            .ok_or_else(|| format!("ELF {kind} is unterminated or too long"))?;
        if end == 0 {
            return Err(format!("ELF {kind} is empty"));
        }
        Ok(available[..end].to_vec())
    };
    let mut names = Vec::with_capacity(needed.len());
    for needed_offset in needed {
        names.push(read_string(needed_offset, "DT_NEEDED name")?);
    }
    let soname = soname
        .map(|offset| read_string(offset, "DT_SONAME name"))
        .transpose()?;
    Ok(ElfDynamicMetadata {
        dependencies: names,
        soname,
    })
}

fn elf_virtual_file_offset(
    address: u64,
    size: u64,
    loads: &[ElfFileSegment],
    file_len: u64,
) -> Result<u64, String> {
    let mut selected = None;
    for segment in loads {
        let Some(delta) = address.checked_sub(segment.virtual_address) else {
            continue;
        };
        let Some(end) = delta.checked_add(size) else {
            continue;
        };
        if end > segment.file_size {
            continue;
        }
        let offset = segment
            .file_offset
            .checked_add(delta)
            .ok_or_else(|| "ELF virtual-to-file mapping overflows".to_string())?;
        binary_range_end(offset, size, file_len, "ELF dynamic string table")?;
        if selected.is_some_and(|previous| previous != offset) {
            return Err("ELF dynamic string table has an ambiguous file mapping".to_string());
        }
        selected = Some(offset);
    }
    selected.ok_or_else(|| "ELF dynamic string table is not backed by PT_LOAD bytes".to_string())
}

#[derive(Clone, Copy)]
struct PeSectionMapping {
    virtual_address: u64,
    virtual_size: u64,
    raw_offset: u64,
    raw_size: u64,
}

fn read_pe_dynamic_dependencies(
    file: &mut fs::File,
    file_len: u64,
) -> Result<Vec<Vec<u8>>, String> {
    const COFF_HEADER_SIZE: u64 = 24;
    const SECTION_HEADER_SIZE: u64 = 40;
    const MAX_SECTIONS: u16 = 1_024;
    const IMPORT_DESCRIPTOR_SIZE: u64 = 20;

    let mut dos = [0u8; 64];
    read_binary_at(file, 0, &mut dos, "PE DOS header")?;
    if dos[..2] != *b"MZ" {
        return Err("expected a PE executable".to_string());
    }
    let pe_offset = le_u32(&dos, 60) as u64;
    binary_range_end(pe_offset, COFF_HEADER_SIZE, file_len, "PE COFF header")?;
    let mut coff = [0u8; COFF_HEADER_SIZE as usize];
    read_binary_at(file, pe_offset, &mut coff, "PE COFF header")?;
    if coff[..4] != *b"PE\0\0" {
        return Err("expected a PE executable signature".to_string());
    }
    let section_count = le_u16(&coff, 6);
    let optional_size = le_u16(&coff, 20) as u64;
    if section_count == 0
        || section_count > MAX_SECTIONS
        || !(128..=MAX_PE_OPTIONAL_HEADER_BYTES).contains(&optional_size)
    {
        return Err("PE headers cannot contain a bounded import directory".to_string());
    }
    let optional_offset = pe_offset + COFF_HEADER_SIZE;
    binary_range_end(
        optional_offset,
        optional_size,
        file_len,
        "PE optional header",
    )?;
    let mut optional = vec![0u8; optional_size as usize];
    read_binary_at(file, optional_offset, &mut optional, "PE optional header")?;
    if le_u16(&optional, 0) != 0x020b {
        return Err("expected a PE32+ optional header".to_string());
    }
    let directory_count = le_u32(&optional, 108);
    if directory_count < 2 {
        return Ok(Vec::new());
    }
    let import_rva = le_u32(&optional, 120) as u64;
    let import_size = le_u32(&optional, 124) as u64;
    if import_rva == 0 || import_size == 0 {
        return Ok(Vec::new());
    }
    if !(IMPORT_DESCRIPTOR_SIZE..=MAX_DYNAMIC_STRING_TABLE_BYTES).contains(&import_size) {
        return Err("PE import directory size is invalid or exceeds the parser limit".to_string());
    }

    let section_offset = optional_offset + optional_size;
    let section_table_size = SECTION_HEADER_SIZE
        .checked_mul(section_count as u64)
        .ok_or_else(|| "PE section-table size overflows".to_string())?;
    binary_range_end(
        section_offset,
        section_table_size,
        file_len,
        "PE section table",
    )?;
    let mut sections = Vec::with_capacity(section_count as usize);
    for index in 0..section_count as u64 {
        let mut section = [0u8; SECTION_HEADER_SIZE as usize];
        read_binary_at(
            file,
            section_offset + index * SECTION_HEADER_SIZE,
            &mut section,
            "PE section header",
        )?;
        let raw_offset = le_u32(&section, 20) as u64;
        let raw_size = le_u32(&section, 16) as u64;
        if raw_size != 0 {
            binary_range_end(raw_offset, raw_size, file_len, "PE section data")?;
        }
        sections.push(PeSectionMapping {
            virtual_address: le_u32(&section, 12) as u64,
            virtual_size: le_u32(&section, 8) as u64,
            raw_offset,
            raw_size,
        });
    }
    let header_size = le_u32(&optional, 60) as u64;
    let (import_offset, import_available) =
        pe_rva_to_file_range(import_rva, import_size, header_size, &sections, file_len)?;
    if import_available < import_size {
        return Err("PE import directory extends beyond its file-backed range".to_string());
    }

    let mut names = Vec::new();
    let mut terminated = false;
    for index in 0..import_size / IMPORT_DESCRIPTOR_SIZE {
        let mut descriptor = [0u8; IMPORT_DESCRIPTOR_SIZE as usize];
        read_binary_at(
            file,
            import_offset + index * IMPORT_DESCRIPTOR_SIZE,
            &mut descriptor,
            "PE import descriptor",
        )?;
        if descriptor.iter().all(|byte| *byte == 0) {
            terminated = true;
            break;
        }
        if names.len() >= MAX_DYNAMIC_DEPENDENCIES {
            return Err("PE import table exceeds the parser limit".to_string());
        }
        let name_rva = le_u32(&descriptor, 12) as u64;
        if name_rva == 0 {
            return Err("PE import descriptor has no DLL name".to_string());
        }
        names.push(read_pe_rva_c_string(
            file,
            name_rva,
            header_size,
            &sections,
            file_len,
        )?);
    }
    if !terminated {
        return Err("PE import directory has no bounded null descriptor".to_string());
    }
    Ok(names)
}

fn pe_rva_to_file_range(
    rva: u64,
    minimum_size: u64,
    header_size: u64,
    sections: &[PeSectionMapping],
    file_len: u64,
) -> Result<(u64, u64), String> {
    let mut selected = None;
    if rva < header_size {
        let available = header_size
            .min(file_len)
            .checked_sub(rva)
            .ok_or_else(|| "PE header RVA mapping underflows".to_string())?;
        if available >= minimum_size {
            selected = Some((rva, available));
        }
    }
    for section in sections {
        let span = section.virtual_size.max(section.raw_size);
        let Some(delta) = rva.checked_sub(section.virtual_address) else {
            continue;
        };
        if delta >= span || delta >= section.raw_size {
            continue;
        }
        let available = section.raw_size - delta;
        if available < minimum_size {
            continue;
        }
        let offset = section
            .raw_offset
            .checked_add(delta)
            .ok_or_else(|| "PE RVA mapping overflows".to_string())?;
        binary_range_end(offset, minimum_size, file_len, "PE RVA mapping")?;
        let candidate = (offset, available.min(file_len - offset));
        if selected.is_some_and(|previous| previous != candidate) {
            return Err("PE RVA has an ambiguous file mapping".to_string());
        }
        selected = Some(candidate);
    }
    selected.ok_or_else(|| "PE RVA is not backed by bounded file bytes".to_string())
}

fn read_pe_rva_c_string(
    file: &mut fs::File,
    rva: u64,
    header_size: u64,
    sections: &[PeSectionMapping],
    file_len: u64,
) -> Result<Vec<u8>, String> {
    let (offset, available) = pe_rva_to_file_range(rva, 1, header_size, sections, file_len)?;
    let read_len = available.min((MAX_DYNAMIC_DEPENDENCY_NAME_BYTES + 1) as u64) as usize;
    let mut bytes = vec![0u8; read_len];
    read_binary_at(file, offset, &mut bytes, "PE imported DLL name")?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "PE imported DLL name is unterminated or too long".to_string())?;
    if end == 0 {
        return Err("PE imported DLL name is empty".to_string());
    }
    bytes.truncate(end);
    Ok(bytes)
}

struct MachODynamicMetadata {
    dependencies: Vec<Vec<u8>>,
    install_name: Option<Vec<u8>>,
}

fn read_macho_dynamic_dependencies(
    file: &mut fs::File,
    file_len: u64,
) -> Result<Vec<Vec<u8>>, String> {
    Ok(read_macho_dynamic_metadata(file, file_len)?.dependencies)
}

fn read_macho_dynamic_metadata(
    file: &mut fs::File,
    file_len: u64,
) -> Result<MachODynamicMetadata, String> {
    const MACH_HEADER_SIZE: u64 = 32;
    const MAX_LOAD_COMMANDS: u32 = 4_096;
    const MAX_LOAD_COMMAND_BYTES: u32 = 16 * 1024 * 1024;
    let mut header = [0u8; MACH_HEADER_SIZE as usize];
    read_binary_at(file, 0, &mut header, "Mach-O 64-bit header")?;
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
        return Err("expected a little-endian Mach-O executable".to_string());
    }
    let command_count = le_u32(&header, 16);
    let command_bytes = le_u32(&header, 20);
    if command_count == 0
        || command_count > MAX_LOAD_COMMANDS
        || command_bytes == 0
        || command_bytes > MAX_LOAD_COMMAND_BYTES
    {
        return Err("Mach-O load-command table exceeds the parser limit".to_string());
    }
    let commands_end = binary_range_end(
        MACH_HEADER_SIZE,
        command_bytes as u64,
        file_len,
        "Mach-O load-command table",
    )?;
    let mut cursor = MACH_HEADER_SIZE;
    let mut names = Vec::new();
    let mut install_name = None;
    for _ in 0..command_count {
        let mut command_header = [0u8; 8];
        read_binary_at(file, cursor, &mut command_header, "Mach-O load command")?;
        let command = le_u32(&command_header, 0);
        let command_size = le_u32(&command_header, 4) as u64;
        if command_size < 8 || !command_size.is_multiple_of(8) {
            return Err("Mach-O load command has an invalid size".to_string());
        }
        let next = binary_range_end(cursor, command_size, commands_end, "Mach-O load command")?;
        let command_kind = command & !0x8000_0000;
        let is_dependency = matches!(command_kind, 0x0c | 0x18 | 0x1f | 0x20 | 0x23);
        let is_install_name = command_kind == 0x0d;
        if is_dependency || is_install_name {
            if command_size < 24 {
                return Err("Mach-O dylib load command is truncated".to_string());
            }
            if is_dependency && names.len() >= MAX_DYNAMIC_DEPENDENCIES {
                return Err("Mach-O dependency table exceeds the parser limit".to_string());
            }
            let mut dylib = [0u8; 24];
            read_binary_at(file, cursor, &mut dylib, "Mach-O dylib load command")?;
            let name_offset = le_u32(&dylib, 8) as u64;
            if name_offset < 24 || name_offset >= command_size {
                return Err("Mach-O dylib name offset is outside its load command".to_string());
            }
            let available = command_size - name_offset;
            let read_len = available.min((MAX_DYNAMIC_DEPENDENCY_NAME_BYTES + 1) as u64) as usize;
            let mut name = vec![0u8; read_len];
            read_binary_at(file, cursor + name_offset, &mut name, "Mach-O dylib name")?;
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| "Mach-O dylib name is unterminated or too long".to_string())?;
            if end == 0 {
                return Err("Mach-O dylib name is empty".to_string());
            }
            name.truncate(end);
            if is_install_name {
                if install_name.replace(name).is_some() {
                    return Err("Mach-O image has multiple LC_ID_DYLIB commands".to_string());
                }
            } else {
                names.push(name);
            }
        }
        cursor = next;
    }
    if cursor != commands_end {
        return Err("Mach-O load-command count does not consume sizeofcmds".to_string());
    }
    Ok(MachODynamicMetadata {
        dependencies: names,
        install_name,
    })
}

fn verify_elf_x86_64(file: &mut fs::File, file_len: u64) -> Result<(), String> {
    const ELF_HEADER_SIZE: usize = 64;
    const ELF_PROGRAM_HEADER_SIZE: u64 = 56;
    const MAX_PROGRAM_HEADERS: u16 = 4_096;
    let mut header = [0u8; ELF_HEADER_SIZE];
    read_binary_at(file, 0, &mut header, "ELF64 header")?;
    let os_abi = header[7];
    let file_type = le_u16(&header, 16);
    let machine = le_u16(&header, 18);
    let version = le_u32(&header, 20);
    let entry = le_u64(&header, 24);
    let program_offset = le_u64(&header, 32);
    let header_size = le_u16(&header, 52);
    let program_entry_size = le_u16(&header, 54);
    let program_count = le_u16(&header, 56);
    if header[..4] != *b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || !matches!(os_abi, 0 | 3)
        || header[8] != 0
        || !matches!(file_type, 2 | 3)
        || machine != 62
        || version != 1
        || entry == 0
        || header_size as usize != ELF_HEADER_SIZE
        || program_entry_size as u64 != ELF_PROGRAM_HEADER_SIZE
        || program_count == 0
        || program_count > MAX_PROGRAM_HEADERS
        || program_offset < ELF_HEADER_SIZE as u64
    {
        return Err(
            "expected a Linux-compatible little-endian x86_64 ELF64 executable".to_string(),
        );
    }
    let table_size = ELF_PROGRAM_HEADER_SIZE
        .checked_mul(program_count as u64)
        .ok_or_else(|| "ELF program table size overflows".to_string())?;
    binary_range_end(program_offset, table_size, file_len, "ELF program table")?;
    let mut has_load = false;
    for index in 0..program_count as u64 {
        let offset = program_offset + index * ELF_PROGRAM_HEADER_SIZE;
        let mut program = [0u8; ELF_PROGRAM_HEADER_SIZE as usize];
        read_binary_at(file, offset, &mut program, "ELF program header")?;
        if le_u32(&program, 0) != 1 {
            continue;
        }
        let segment_offset = le_u64(&program, 8);
        let file_size = le_u64(&program, 32);
        let memory_size = le_u64(&program, 40);
        if memory_size < file_size {
            return Err("ELF PT_LOAD memory size is smaller than its file size".to_string());
        }
        binary_range_end(segment_offset, file_size, file_len, "ELF PT_LOAD segment")?;
        has_load = true;
    }
    if !has_load {
        return Err("ELF executable has no PT_LOAD segment".to_string());
    }
    Ok(())
}

fn verify_pe_x86_64(file: &mut fs::File, file_len: u64) -> Result<(), String> {
    const COFF_HEADER_SIZE: u64 = 24;
    const MIN_PE32_PLUS_SIZE: usize = 112;
    const SECTION_HEADER_SIZE: u64 = 40;
    const MAX_SECTIONS: u16 = 1_024;
    let mut dos = [0u8; 64];
    read_binary_at(file, 0, &mut dos, "PE DOS header")?;
    if dos[..2] != *b"MZ" {
        return Err("expected an x86_64 PE executable (missing MZ header)".to_string());
    }
    let pe_offset = le_u32(&dos, 60) as u64;
    if pe_offset < dos.len() as u64 || pe_offset > 16 * 1024 * 1024 {
        return Err("PE header offset is outside the supported range".to_string());
    }
    binary_range_end(pe_offset, COFF_HEADER_SIZE, file_len, "PE COFF header")?;
    let mut coff = [0u8; COFF_HEADER_SIZE as usize];
    read_binary_at(file, pe_offset, &mut coff, "PE COFF header")?;
    let section_count = le_u16(&coff, 6);
    let optional_size = le_u16(&coff, 20);
    let characteristics = le_u16(&coff, 22);
    if coff[..4] != *b"PE\0\0"
        || le_u16(&coff, 4) != 0x8664
        || section_count == 0
        || section_count > MAX_SECTIONS
        || (optional_size as usize) < MIN_PE32_PLUS_SIZE
        || u64::from(optional_size) > MAX_PE_OPTIONAL_HEADER_BYTES
        || characteristics & 0x0002 == 0
        || characteristics & 0x2000 != 0
    {
        return Err("expected an x86_64 PE32+ executable image".to_string());
    }
    let optional_offset = pe_offset + COFF_HEADER_SIZE;
    binary_range_end(
        optional_offset,
        optional_size as u64,
        file_len,
        "PE32+ optional header",
    )?;
    let mut optional = vec![0u8; optional_size as usize];
    read_binary_at(
        file,
        optional_offset,
        &mut optional,
        "PE32+ optional header",
    )?;
    let directory_count = le_u32(&optional, 108) as u64;
    let required_optional_size = 112u64
        .checked_add(
            directory_count
                .checked_mul(8)
                .ok_or_else(|| "PE data-directory size overflows".to_string())?,
        )
        .ok_or_else(|| "PE optional-header size overflows".to_string())?;
    if le_u16(&optional, 0) != 0x020b
        || le_u32(&optional, 16) == 0
        || le_u32(&optional, 32) == 0
        || le_u32(&optional, 36) == 0
        || le_u32(&optional, 56) == 0
        || le_u32(&optional, 60) == 0
        || required_optional_size > optional_size as u64
    {
        return Err("PE32+ optional header is incomplete or invalid".to_string());
    }
    let section_offset = optional_offset + optional_size as u64;
    let section_table_size = SECTION_HEADER_SIZE
        .checked_mul(section_count as u64)
        .ok_or_else(|| "PE section-table size overflows".to_string())?;
    binary_range_end(
        section_offset,
        section_table_size,
        file_len,
        "PE section table",
    )?;
    let mut has_executable_section = false;
    for index in 0..section_count as u64 {
        let offset = section_offset + index * SECTION_HEADER_SIZE;
        let mut section = [0u8; SECTION_HEADER_SIZE as usize];
        read_binary_at(file, offset, &mut section, "PE section header")?;
        let virtual_size = le_u32(&section, 8) as u64;
        let raw_size = le_u32(&section, 16) as u64;
        let raw_offset = le_u32(&section, 20) as u64;
        if raw_size != 0 {
            binary_range_end(raw_offset, raw_size, file_len, "PE section data")?;
        }
        if virtual_size != 0 && le_u32(&section, 36) & 0x2000_0000 != 0 {
            has_executable_section = true;
        }
    }
    if !has_executable_section {
        return Err("PE executable has no executable section".to_string());
    }
    Ok(())
}

fn verify_macho_arm64_macos(file: &mut fs::File, file_len: u64) -> Result<(), String> {
    const MACH_HEADER_SIZE: u64 = 32;
    const MAX_LOAD_COMMANDS: u32 = 4_096;
    const MAX_LOAD_COMMAND_BYTES: u32 = 16 * 1024 * 1024;
    const LC_SEGMENT_64: u32 = 0x19;
    const LC_VERSION_MIN_MACOSX: u32 = 0x24;
    const LC_BUILD_VERSION: u32 = 0x32;
    let mut header = [0u8; MACH_HEADER_SIZE as usize];
    read_binary_at(file, 0, &mut header, "Mach-O 64-bit header")?;
    let command_count = le_u32(&header, 16);
    let command_bytes = le_u32(&header, 20);
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || le_u32(&header, 4) != 0x0100_000c
        || le_u32(&header, 12) != 2
        || command_count == 0
        || command_count > MAX_LOAD_COMMANDS
        || command_bytes == 0
        || command_bytes > MAX_LOAD_COMMAND_BYTES
    {
        return Err("expected a little-endian arm64 Mach-O executable".to_string());
    }
    let commands_end = binary_range_end(
        MACH_HEADER_SIZE,
        command_bytes as u64,
        file_len,
        "Mach-O load-command table",
    )?;
    let mut cursor = MACH_HEADER_SIZE;
    let mut has_loadable_segment = false;
    let mut has_macos_platform = false;
    for _ in 0..command_count {
        let mut command_header = [0u8; 8];
        read_binary_at(file, cursor, &mut command_header, "Mach-O load command")?;
        let command = le_u32(&command_header, 0);
        let command_size = le_u32(&command_header, 4) as u64;
        if command_size < 8 || !command_size.is_multiple_of(8) {
            return Err("Mach-O load command has an invalid size".to_string());
        }
        let next = binary_range_end(cursor, command_size, commands_end, "Mach-O load command")?;
        match command {
            LC_SEGMENT_64 => {
                if command_size < 72 {
                    return Err("Mach-O LC_SEGMENT_64 command is truncated".to_string());
                }
                let mut segment = [0u8; 72];
                read_binary_at(file, cursor, &mut segment, "Mach-O LC_SEGMENT_64")?;
                let file_offset = le_u64(&segment, 40);
                let segment_file_size = le_u64(&segment, 48);
                let segment_memory_size = le_u64(&segment, 32);
                let section_count = le_u32(&segment, 64) as u64;
                let section_bytes = section_count
                    .checked_mul(80)
                    .and_then(|size| size.checked_add(72))
                    .ok_or_else(|| "Mach-O section-table size overflows".to_string())?;
                if section_bytes > command_size || segment_memory_size < segment_file_size {
                    return Err("Mach-O segment or section table is invalid".to_string());
                }
                binary_range_end(
                    file_offset,
                    segment_file_size,
                    file_len,
                    "Mach-O segment data",
                )?;
                if segment_memory_size != 0 && le_u32(&segment, 60) & 0x4 != 0 {
                    has_loadable_segment = true;
                }
            }
            LC_BUILD_VERSION => {
                if command_size < 24 {
                    return Err("Mach-O LC_BUILD_VERSION command is truncated".to_string());
                }
                let mut build = [0u8; 24];
                read_binary_at(file, cursor, &mut build, "Mach-O LC_BUILD_VERSION")?;
                let tool_count = le_u32(&build, 20) as u64;
                let required = tool_count
                    .checked_mul(8)
                    .and_then(|size| size.checked_add(24))
                    .ok_or_else(|| "Mach-O build-tool table size overflows".to_string())?;
                if required > command_size {
                    return Err("Mach-O LC_BUILD_VERSION tool table is truncated".to_string());
                }
                if le_u32(&build, 8) == 1 {
                    has_macos_platform = true;
                }
            }
            LC_VERSION_MIN_MACOSX => {
                if command_size < 16 {
                    return Err("Mach-O LC_VERSION_MIN_MACOSX command is truncated".to_string());
                }
                has_macos_platform = true;
            }
            _ => {}
        }
        cursor = next;
    }
    if cursor != commands_end {
        return Err("Mach-O load-command count does not consume sizeofcmds".to_string());
    }
    if !has_loadable_segment {
        return Err("Mach-O executable has no executable LC_SEGMENT_64".to_string());
    }
    if !has_macos_platform {
        return Err("Mach-O executable is not marked for macOS".to_string());
    }
    Ok(())
}

#[derive(Debug)]
struct VerifiedLinkOutput {
    path: PathBuf,
    file: fs::File,
    identity: LinkLibraryIdentity,
}

fn open_link_output_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options.open(path)
}

impl VerifiedLinkOutput {
    fn open(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("failed to inspect linked output: {error}"))?;
        if !is_plain_regular_file(&metadata) || metadata.len() == 0 {
            return Err("linked output is not a non-empty plain regular file".to_string());
        }
        let file = open_link_output_file(path)
            .map_err(|error| format!("failed to open linked output: {error}"))?;
        let identity = link_library_identity(&file)
            .map_err(|error| format!("failed to identify linked output: {error}"))?;
        if identity.length == 0 || identity.length != metadata.len() {
            return Err("linked output changed while it was being opened".to_string());
        }
        if identity.length > MAX_NATIVE_LINK_OUTPUT_BYTES {
            return Err(format!(
                "linked output exceeds the {MAX_NATIVE_LINK_OUTPUT_BYTES}-byte artifact limit"
            ));
        }
        let verified = Self {
            path: path.to_path_buf(),
            file,
            identity,
        };
        verified.verify_current_path()?;
        Ok(verified)
    }

    fn verify_open_identity(&self) -> Result<(), String> {
        let current = link_library_identity(&self.file)
            .map_err(|error| format!("failed to re-identify linked output: {error}"))?;
        if !same_open_link_library_contents(&current, &self.identity) {
            return Err("linked output changed after it was opened".to_string());
        }
        Ok(())
    }

    fn verify_current_path(&self) -> Result<(), String> {
        self.verify_open_identity()?;
        let metadata = fs::symlink_metadata(&self.path)
            .map_err(|error| format!("failed to re-inspect linked output path: {error}"))?;
        if !is_plain_regular_file(&metadata) || metadata.len() != self.identity.length {
            return Err("linked output path no longer names the verified regular file".to_string());
        }
        let reopened = open_link_output_file(&self.path)
            .map_err(|error| format!("failed to reopen linked output path: {error}"))?;
        let reopened_identity = link_library_identity(&reopened)
            .map_err(|error| format!("failed to identify reopened linked output: {error}"))?;
        if !same_open_link_library_contents(&reopened_identity, &self.identity) {
            return Err("linked output path was replaced after verification".to_string());
        }
        Ok(())
    }
}

fn validate_link_output_candidate(
    temporary: &Path,
    source: &Path,
    target: Option<&BuildTarget>,
    compiler: &str,
    features: CSourceFeatures,
    libpq_requirement: Option<&LibpqRuntimeDependency>,
    mysql_requirement: Option<&MysqlRuntimeDependency>,
) -> Result<VerifiedLinkOutput, KuError> {
    let mut verified = VerifiedLinkOutput::open(temporary).map_err(|reason| {
        KuError::message(format!(
            "native C compiler '{compiler}' did not produce a safe staging artifact '{}': {reason}\nhelp: inspect generated source at {}",
            temporary.display(),
            source.display()
        ))
    })?;
    let host_target = target.is_none().then(supported_host_build_target).flatten();
    if let Some(target) = target.or(host_target.as_ref()) {
        verify_native_binary_target_file(&mut verified.file, verified.identity.length, target)
            .map_err(|reason| {
            KuError::message(format!(
                "native C compiler '{compiler}' produced an invalid '{}' artifact: {reason}\nhelp: configure a compiler/sysroot for {}, or use the target-specific C artifact at {}",
                target.slug,
                target.rust_triple,
                source.display()
            ))
        })?;
        verify_native_binary_dynamic_dependencies_file(
            &mut verified.file,
            verified.identity.length,
            target,
            features,
            libpq_requirement,
            mysql_requirement,
        )
            .map_err(|reason| {
                KuError::message(format!(
                    "native C compiler '{compiler}' produced an unsafe '{}' dependency graph: {reason}\nhelp: install a matching shared/import database client library and inspect the target-specific C artifact at {}",
                    target.slug,
                    source.display()
                ))
            })?;
    }
    verified.verify_current_path().map_err(|reason| {
        KuError::message(format!(
            "native C compiler '{compiler}' staging artifact changed during verification: {reason}\nhelp: inspect generated source at {}",
            source.display()
        ))
    })?;
    Ok(verified)
}

fn validate_runner_output_candidate(
    temporary: &Path,
    target: Option<&BuildTarget>,
) -> Result<VerifiedLinkOutput, KuError> {
    let mut verified = VerifiedLinkOutput::open(temporary).map_err(|reason| {
        KuError::message(format!(
            "rustc did not produce a safe staging artifact '{}': {reason}",
            temporary.display()
        ))
    })?;
    let host_target = target.is_none().then(supported_host_build_target).flatten();
    if let Some(target) = target.or(host_target.as_ref()) {
        verify_native_binary_target_file(&mut verified.file, verified.identity.length, target)
            .map_err(|reason| {
            KuError::message(format!(
                "rustc produced an invalid '{}' artifact: {reason}\nhelp: install the '{}' Rust target and matching linker",
                target.slug, target.rust_triple
            ))
        })?;
    }
    verified.verify_current_path().map_err(|reason| {
        KuError::message(format!(
            "rustc staging artifact changed during verification: {reason}"
        ))
    })?;
    Ok(verified)
}

fn finalize_link_output(
    output_staging: &LinkOutputStaging,
    source: &Path,
    target: Option<&BuildTarget>,
    compiler: &str,
    features: CSourceFeatures,
    libpq_requirement: Option<&LibpqRuntimeDependency>,
    mysql_requirement: Option<&MysqlRuntimeDependency>,
) -> Result<(), KuError> {
    let verified = validate_link_output_candidate(
        output_staging.path(),
        source,
        target,
        compiler,
        features,
        libpq_requirement,
        mysql_requirement,
    )?;
    install_verified_link_output(verified, output_staging)
}

fn install_verified_link_output(
    verified: VerifiedLinkOutput,
    staging: &LinkOutputStaging,
) -> Result<(), KuError> {
    if verified.path != staging.artifact {
        return Err(KuError::message(
            "failed to install native output: verified artifact is outside its private staging directory",
        ));
    }
    verified.verify_current_path().map_err(|reason| {
        KuError::message(format!(
            "failed to install native output: verified staging changed before installation: {reason}"
        ))
    })?;
    verify_link_destination_unchanged(&staging.output, &staging.initial_destination)?;
    // The private staging directory shares the destination filesystem. Existing
    // outputs use the platform replacement primitive; initially absent Unix
    // outputs use hard-link create-if-absent before unlinking the staging name.
    // Never delete a previous output as a fallback: a failed install must leave
    // the last verified artifact intact.
    replace_link_output_atomically(
        &verified.path,
        &staging.output,
        staging.initial_destination.is_some(),
    )
    .map_err(|error| {
        KuError::message(format!(
            "failed to atomically install verified native output '{}': {error}",
            staging.output.display()
        ))
    })?;
    let installed = fs::File::open(&staging.output).map_err(|error| {
        KuError::message(format!(
            "failed to reopen installed native output '{}': {error}",
            staging.output.display()
        ))
    })?;
    let installed_identity = link_library_identity(&installed).map_err(|error| {
        KuError::message(format!(
            "failed to identify installed native output '{}': {error}",
            staging.output.display()
        ))
    })?;
    if !same_open_link_library_contents(&installed_identity, &verified.identity) {
        return Err(KuError::message(format!(
            "installed native output '{}' does not match the verified artifact",
            staging.output.display()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_link_output_atomically(
    temporary: &Path,
    output: &Path,
    destination_existed: bool,
) -> io::Result<()> {
    if destination_existed {
        fs::rename(temporary, output)
    } else {
        // hard_link provides same-filesystem create-if-absent semantics: a
        // destination created after the identity check is never overwritten.
        fs::hard_link(temporary, output)?;
        fs::remove_file(temporary)
    }
}

#[cfg(windows)]
fn replace_link_output_atomically(
    temporary: &Path,
    output: &Path,
    destination_existed: bool,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native output path contains an embedded NUL",
            ));
        }
        value.push(0);
        Ok(value)
    }

    let temporary = wide_path(temporary)?;
    let output = wide_path(output)?;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let flags = MOVEFILE_WRITE_THROUGH
        | if destination_existed {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    // Same-volume MoveFileExW is the Windows rename/replace primitive. Omitting
    // REPLACE_EXISTING for an initially missing destination also fails closed
    // if another writer wins the destination race after our identity check.
    let result = unsafe { MoveFileExW(temporary.as_ptr(), output.as_ptr(), flags) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn compile_c_source(
    source: &Path,
    output: &Path,
    target: Option<&BuildTarget>,
    profile: BuildProfile,
    static_link: bool,
    verbose: bool,
) -> Result<(), KuError> {
    let link_started = Instant::now();
    let link_deadline = link_started
        .checked_add(NATIVE_LINK_TOTAL_TIMEOUT)
        .unwrap_or(link_started);
    validate_native_output_name(output)?;
    if target.is_none() && supported_host_build_target().is_none() {
        return Err(KuError::message(
            "native C linking is not validated for this host architecture\nhelp: emit C only, or use an explicit supported target: x86_64-linux, x86_64-windows, aarch64-darwin",
        ));
    }
    let output_directory = link_output_directory(output);
    fs::create_dir_all(output_directory).map_err(|err| {
        KuError::message(format!(
            "failed to create output directory '{}': {err}",
            output_directory.display()
        ))
    })?;
    let features = CSourceFeatures::inspect(source)?;
    validate_c_target_features(features, target)?;
    // Native HTTP/Redis use a portable socket layer. Its Windows branch needs
    // Winsock: MSVC auto-links through the emitted pragma, while gcc/clang/zig
    // need `-lws2_32`. Base the decision on the output target, not the build host,
    // so POSIX builds do not resolve a library hidden behind `#if _WIN32`.
    let target_is_windows = target
        .map(|value| value.is_windows)
        .unwrap_or(cfg!(windows));
    let needs_winsock = target_is_windows && features.winsock;
    let needs_pthreads = !target_is_windows && features.pthreads;
    // Every compiler receives one exact libpq path. This prevents an implicit
    // search path from selecting a static or wrong-target archive.
    let needs_libpq = features.libpq;
    validate_libpq_link_mode(needs_libpq, static_link)?;
    let needs_libmysql = features.libmysql && target.is_none_or(BuildTarget::matches_host);
    validate_libmysql_link_mode(needs_libmysql, static_link)?;
    let libmysql_directory = if needs_libmysql {
        Some(detect_libmysql_directory()?.ok_or_else(missing_shared_libmysql_error)?)
    } else {
        None
    };
    let libmysql_include = if needs_libmysql {
        detect_libmysql_include_dir(
            libmysql_directory
                .as_ref()
                .map(|snapshot| snapshot.dir.as_path()),
        )?
    } else {
        None
    };
    let libpq_platform = libpq_link_platform(target);
    let explicit_libpq_dir = if needs_libpq {
        Some(
            explicit_libpq_directory(env::var_os("KU_PG_LIB"))?
                .ok_or_else(missing_shared_libpq_error)?,
        )
    } else {
        None
    };
    let mut deferred_libpq_error = None;
    let mut deferred_libmysql_error = None;
    let mut compiler_failures = Vec::new();
    let mut tried = Vec::new();
    let env_cc = configured_c_compiler(env::var_os("KU_CC"))?;
    for candidate in c_compiler_candidates(env_cc.as_deref()) {
        let declared_target = match compiler_declared_target(&candidate) {
            Ok(target) => target,
            Err(error) => {
                if candidate.explicitly_configured {
                    return Err(error);
                }
                compiler_failures.push(format!("{}: {error}", candidate.label));
                continue;
            }
        };
        if let Some(target) = target {
            if !c_compiler_supports_explicit_target(&candidate, target) {
                continue;
            }
        } else if let (Some(host), Some(declared)) =
            (supported_host_build_target(), declared_target)
        {
            if !compiler_target_matches_build(declared, &host) {
                let error = KuError::message(format!(
                    "configured compiler target '{declared}' conflicts with this host"
                ));
                if candidate.explicitly_configured {
                    return Err(error);
                }
                compiler_failures.push(format!("{}: {error}", candidate.label));
                continue;
            }
        }
        let target_arguments = if let Some(target) = target {
            match c_compiler_target_arguments(&candidate, target) {
                Ok(arguments) => arguments,
                Err(error) => {
                    if candidate.explicitly_configured {
                        return Err(error);
                    }
                    compiler_failures.push(format!("{}: {error}", candidate.label));
                    continue;
                }
            }
        } else {
            Vec::new()
        };
        tried.push(candidate.label.clone());
        let probed_clang_target = if (needs_libpq || needs_libmysql)
            && libpq_platform == LibpqLibraryPlatform::Windows
            && target.is_none()
            && candidate.kind == CCompilerKind::Clang
            && declared_target.is_none()
        {
            match probe_clang_default_target(&candidate, link_deadline) {
                Ok(target) => Some(target),
                Err(error) if build_cleanup_is_unconfirmed(&error) => {
                    return Err(KuError::message(format!(
                        "compiler target probe cleanup failed closed: {error}"
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if candidate.explicitly_configured {
                        return Err(KuError::message(format!(
                            "failed to run configured C compiler '{}' target probe: {error}",
                            candidate.label
                        )));
                    }
                    continue;
                }
                Err(error) => {
                    if candidate.explicitly_configured {
                        return Err(KuError::message(format!(
                            "failed to determine configured C compiler '{}' target: {error}",
                            candidate.label
                        )));
                    }
                    compiler_failures
                        .push(format!("{} target probe failed: {error}", candidate.label));
                    continue;
                }
            }
        } else {
            None
        };
        let database_library_format = if needs_libpq || needs_libmysql {
            match libpq_library_format_for_compiler(
                libpq_platform,
                &candidate,
                target,
                probed_clang_target.as_deref(),
            ) {
                Ok(format) => Some(format),
                Err(error) => {
                    if candidate.explicitly_configured {
                        return Err(error);
                    }
                    if needs_libpq && deferred_libpq_error.is_none() {
                        deferred_libpq_error = Some(error.clone());
                    }
                    if needs_libmysql && deferred_libmysql_error.is_none() {
                        deferred_libmysql_error = Some(error);
                    }
                    continue;
                }
            }
        } else {
            None
        };
        let libpq_library = if needs_libpq {
            let snapshot = explicit_libpq_dir
                .as_ref()
                .expect("std.pg requires an explicit KU_PG_LIB snapshot");
            match libpq_library_from_explicit_directory(
                snapshot,
                database_library_format.expect("database library format was resolved"),
            ) {
                Ok(library) => Some(library),
                Err(error) => {
                    if candidate.explicitly_configured {
                        return Err(error);
                    }
                    if deferred_libpq_error.is_none() {
                        deferred_libpq_error = Some(error);
                    }
                    continue;
                }
            }
        } else {
            None
        };
        let libmysql_library = if needs_libmysql {
            let snapshot = libmysql_directory
                .as_ref()
                .expect("std.mysql requires an exact client library snapshot");
            match libmysql_library_from_directory(
                snapshot,
                database_library_format.expect("database library format was resolved"),
            ) {
                Ok(library) => Some(library),
                Err(error) => {
                    if candidate.explicitly_configured {
                        return Err(error);
                    }
                    if deferred_libmysql_error.is_none() {
                        deferred_libmysql_error = Some(error);
                    }
                    continue;
                }
            }
        } else {
            None
        };
        let libpq_requirement = libpq_library
            .as_ref()
            .map(|library| {
                libpq_runtime_dependency(
                    library,
                    database_library_format.expect("database library format was resolved"),
                )
            })
            .transpose()?;
        let staged_libpq_library = libpq_library
            .as_ref()
            .map(|library| library.stage_for_link("libpq"))
            .transpose()?;
        let staged_libmysql_library = libmysql_library
            .as_ref()
            .map(|library| library.library.stage_for_link("MySQL client"))
            .transpose()?;
        let mysql_requirement = libmysql_library
            .as_ref()
            .map(|library| {
                mysql_runtime_dependency(
                    &library.library,
                    database_library_format.expect("database library format was resolved"),
                )
            })
            .transpose()?;
        // A failed compiler may leave a partial file. Each automatic fallback
        // starts from a fresh staging path and can never touch the old output.
        let output_staging = LinkOutputStaging::create(output)?;
        let temporary_output = output_staging.path();
        let mut command = Command::new(&candidate.program);
        for arg in &candidate.args {
            command.arg(arg);
        }
        for argument in target_arguments {
            command.arg(argument);
        }
        if let Some(probed_target) = &probed_clang_target {
            command.arg(format!("--target={probed_target}"));
        }
        let compiler_output = temporary_output;
        command
            .arg(source)
            .arg("-std=c11")
            .arg("-o")
            .arg(compiler_output);
        if let Some(include) = &libmysql_include {
            command.arg(format!("-I{}", include.display()));
        }
        if let Some(opt_level) = profile.rustc_opt_level() {
            command.arg(format!("-O{opt_level}"));
        }
        if static_link {
            command.arg("-static");
        }
        if needs_winsock {
            command.arg("-lws2_32");
        }
        if needs_pthreads {
            command.arg("-pthread");
        }
        if needs_libpq {
            let library = staged_libpq_library
                .as_ref()
                .expect("a PG compiler candidate has a private libpq link copy");
            command.arg(strip_verbatim(library.path()));
        }
        if needs_libmysql {
            if let Some(library) = &staged_libmysql_library {
                command.arg(strip_verbatim(library.path()));
            }
        }
        if verbose {
            println!("c compiler command: {command:?}");
        }
        let compiler_timeout = remaining_build_phase_timeout(
            link_deadline,
            C_COMPILER_PROCESS_TIMEOUT,
            "C compiler execution",
        )
        .map_err(|error| KuError::message(error.to_string()))?;
        match run_build_process_bounded(&mut command, compiler_timeout) {
            Ok(status) if status.success() => {
                let verified = match validate_link_output_candidate(
                    temporary_output,
                    source,
                    target,
                    &candidate.label,
                    features,
                    libpq_requirement.as_ref(),
                    mysql_requirement.as_ref(),
                ) {
                    Ok(verified) => verified,
                    Err(error) => {
                        if candidate.explicitly_configured {
                            return Err(error);
                        }
                        compiler_failures.push(format!("{}: {error}", candidate.label));
                        continue;
                    }
                };
                install_verified_link_output(verified, &output_staging)?;
                return Ok(());
            }
            Ok(status) => {
                if candidate.explicitly_configured {
                    return Err(KuError::message(format!(
                        "native C build failed: configured compiler '{}' exited with {status}\nhelp: inspect generated source at {} and repair KU_CC or its target sysroot",
                        candidate.label,
                        source.display()
                    )));
                }
                compiler_failures.push(format!("{} exited with {status}", candidate.label));
                continue;
            }
            Err(err) if build_cleanup_is_unconfirmed(&err) => {
                return Err(KuError::message(format!(
                    "native compiler cleanup failed closed: {err}"
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if candidate.explicitly_configured {
                    return Err(KuError::message(format!(
                        "failed to run configured C compiler '{}': {err}\nhelp: set KU_CC to an installed compiler command",
                        candidate.label
                    )));
                }
                continue;
            }
            Err(err) => {
                if candidate.explicitly_configured {
                    return Err(KuError::message(format!(
                        "failed to run configured C compiler '{}': {err}\nhelp: repair KU_CC or use a different installed compiler command",
                        candidate.label
                    )));
                }
                compiler_failures.push(format!("{} could not run: {err}", candidate.label));
                continue;
            }
        }
    }
    // No GCC-style compiler found. On Windows, fall back to a native MSVC (cl.exe)
    // toolchain located via vswhere and driven through vcvars64.bat. Only for the
    // native target. An explicit target that exactly matches this host may use
    // the same fallback; a real cross build still requires zig/clang or KU_CC.
    if cfg!(windows) && (target.is_none() || target.is_some_and(BuildTarget::matches_host)) {
        let vcvars = match detect_msvc_vcvars(link_deadline) {
            Ok(vcvars) => vcvars,
            Err(error) => {
                compiler_failures.push(format!("MSVC discovery failed: {error}"));
                None
            }
        };
        if let Some(vcvars) = vcvars {
            tried.push("cl (MSVC)".to_string());
            let output_staging = LinkOutputStaging::create(output)?;
            let compiler_output = output_staging.path();
            let result = (|| -> Result<
                (
                    Option<LibpqRuntimeDependency>,
                    Option<MysqlRuntimeDependency>,
                ),
                KuError,
            > {
                let msvc_libpq_library = if needs_libpq {
                    Some(libpq_library_from_explicit_directory(
                        explicit_libpq_dir
                            .as_ref()
                            .expect("std.pg requires an explicit KU_PG_LIB snapshot"),
                        LibpqLibraryFormat::WindowsMsvc,
                    )?)
                } else {
                    None
                };
                let msvc_libmysql_library = if needs_libmysql {
                    Some(libmysql_library_from_directory(
                        libmysql_directory
                            .as_ref()
                            .expect("std.mysql requires an exact client library snapshot"),
                        LibpqLibraryFormat::WindowsMsvc,
                    )?)
                } else {
                    None
                };
                let staged_msvc_libpq_library = msvc_libpq_library
                    .as_ref()
                    .map(|library| library.stage_for_link("libpq"))
                    .transpose()?;
                let staged_msvc_libmysql_library = msvc_libmysql_library
                    .as_ref()
                    .map(|library| library.library.stage_for_link("MySQL client"))
                    .transpose()?;
                let libpq_requirement = msvc_libpq_library
                    .as_ref()
                    .map(|library| {
                        libpq_runtime_dependency(library, LibpqLibraryFormat::WindowsMsvc)
                    })
                    .transpose()?;
                let mysql_requirement = msvc_libmysql_library
                    .as_ref()
                    .map(|library| {
                        mysql_runtime_dependency(&library.library, LibpqLibraryFormat::WindowsMsvc)
                    })
                    .transpose()?;
                compile_with_msvc(
                    &vcvars,
                    source,
                    compiler_output,
                    MsvcDatabaseLinks {
                        libpq: staged_msvc_libpq_library
                            .as_ref()
                            .map(StagedLinkLibrary::path),
                        libmysql: staged_msvc_libmysql_library
                            .as_ref()
                            .map(StagedLinkLibrary::path),
                        libmysql_include: libmysql_include.as_deref(),
                    },
                    MsvcCompileOptions {
                        profile,
                        static_link,
                        verbose,
                        deadline: link_deadline,
                    },
                )?;
                Ok((libpq_requirement, mysql_requirement))
            })();
            match result {
                Err(error) => {
                    if error
                        .to_string()
                        .contains(BUILD_PROCESS_CLEANUP_UNCONFIRMED)
                    {
                        return Err(error);
                    }
                    compiler_failures.push(format!("cl (MSVC): {error}"));
                }
                Ok((libpq_requirement, mysql_requirement)) => {
                    finalize_link_output(
                        &output_staging,
                        source,
                        target,
                        "cl (MSVC)",
                        features,
                        libpq_requirement.as_ref(),
                        mysql_requirement.as_ref(),
                    )?;
                    return Ok(());
                }
            }
        }
    }
    if !compiler_failures.is_empty() {
        return Err(KuError::message(format!(
            "native C build failed with every available automatic compiler: {}\nhelp: inspect generated source at {} and set KU_CC to one known-good compiler command",
            compiler_failures.join("; "),
            source.display()
        )));
    }
    if let Some(error) = deferred_libpq_error {
        return Err(KuError::message(format!(
            "native C build found no runnable compiler paired with a compatible libpq import/shared library: {error}\nhelp: keep one target ABI in KU_PG_LIB and set KU_CC to the matching compiler when automatic discovery is unavailable"
        )));
    }
    if let Some(error) = deferred_libmysql_error {
        return Err(KuError::message(format!(
            "native C build found no runnable compiler paired with a compatible MySQL import/shared library: {error}\nhelp: keep one target ABI in KU_MYSQL_LIB and set KU_CC to the matching compiler when automatic discovery is unavailable"
        )));
    }
    let target_help = target.map_or_else(
        || "install clang/gcc/zig or Visual Studio (MSVC)".to_string(),
        |target| {
            format!(
                "install zig or clang with a '{}' sysroot, or set KU_CC to a compiler already configured for that target",
                target.rust_triple
            )
        },
    );
    Err(KuError::message(format!(
        "C compiler not found for native build\nhelp: {target_help}; tried {}",
        tried.join(", ")
    )))
}

fn c_compiler_supports_explicit_target(
    candidate: &CCompilerCandidate,
    target: &BuildTarget,
) -> bool {
    target.matches_host()
        || candidate.kind != CCompilerKind::Preconfigured
        || candidate.explicitly_configured
}

fn compiler_declared_target(candidate: &CCompilerCandidate) -> Result<Option<&str>, KuError> {
    let mut declared = None;
    let mut index = 0usize;
    while index < candidate.args.len() {
        let argument = candidate.args[index].as_str();
        let value = if argument == "--target" || argument == "-target" {
            index += 1;
            Some(
                candidate
                    .args
                    .get(index)
                    .ok_or_else(|| {
                        KuError::message(format!(
                            "configured compiler '{}' has a target flag without a triple",
                            candidate.label
                        ))
                    })?
                    .as_str(),
            )
        } else {
            argument
                .strip_prefix("--target=")
                .or_else(|| argument.strip_prefix("-target="))
        };
        if let Some(value) = value {
            if value.is_empty() {
                return Err(KuError::message(format!(
                    "configured compiler '{}' has an empty target triple",
                    candidate.label
                )));
            }
            if declared.is_some_and(|previous: &str| !previous.eq_ignore_ascii_case(value)) {
                return Err(KuError::message(format!(
                    "configured compiler '{}' declares conflicting target triples",
                    candidate.label
                )));
            }
            declared = Some(value);
        }
        index += 1;
    }
    Ok(declared)
}

fn compiler_target_matches_build(value: &str, target: &BuildTarget) -> bool {
    let value = value.to_ascii_lowercase();
    let architecture_matches = match target.binary_format {
        NativeBinaryFormat::ElfX86_64 | NativeBinaryFormat::PeX86_64 => {
            value.contains("x86_64") || value.contains("amd64")
        }
        NativeBinaryFormat::MachOArm64 => value.contains("aarch64") || value.contains("arm64"),
    };
    let operating_system_matches = match target.binary_format {
        NativeBinaryFormat::ElfX86_64 => value.contains("linux"),
        NativeBinaryFormat::PeX86_64 => value.contains("windows") || value.contains("mingw"),
        NativeBinaryFormat::MachOArm64 => {
            value.contains("darwin") || value.contains("apple") || value.contains("macos")
        }
    };
    architecture_matches && operating_system_matches
}

fn probe_clang_default_target(
    candidate: &CCompilerCandidate,
    deadline: Instant,
) -> io::Result<String> {
    let mut command = Command::new(&candidate.program);
    command.args(&candidate.args).arg("-dumpmachine");
    let timeout = remaining_build_phase_timeout(
        deadline,
        COMPILER_TARGET_PROBE_TIMEOUT,
        "clang target probe",
    )?;
    let (status, stdout) =
        run_build_process_capture_stdout(&mut command, timeout, MAX_COMPILER_TARGET_BYTES)?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "compiler target probe exited with {status}"
        )));
    }
    let target = std::str::from_utf8(&stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "compiler target is not UTF-8"))?
        .trim();
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "compiler target probe returned an empty or malformed triple",
        ));
    }
    Ok(target.to_string())
}

fn c_compiler_target_arguments(
    candidate: &CCompilerCandidate,
    target: &BuildTarget,
) -> Result<Vec<String>, KuError> {
    if let Some(declared) = compiler_declared_target(candidate)? {
        if !compiler_target_matches_build(declared, target) {
            return Err(KuError::message(format!(
                "configured compiler target '{declared}' conflicts with Ku build target '{}'",
                target.slug
            )));
        }
        return Ok(Vec::new());
    }
    Ok(match candidate.kind {
        CCompilerKind::ZigCc => vec!["-target".to_string(), target.c_triple.to_string()],
        // Clang accepts the GNU-style target option only in its joined form.
        // Passing `--target` and the triple as separate argv entries is rejected
        // by both Apple Clang and the LLVM Clang shipped on GitHub runners.
        CCompilerKind::Clang => vec![format!("--target={}", target.rust_triple)],
        CCompilerKind::Preconfigured => Vec::new(),
    })
}

/// Locate a native MSVC toolchain by asking vswhere for the latest install that
/// carries the VC C/C++ tools, then returning its `vcvars64.bat`. Returns `None`
/// when vswhere or Visual Studio is absent (e.g. non-Windows hosts).
fn detect_msvc_vcvars(deadline: Instant) -> Result<Option<PathBuf>, KuError> {
    let program_files_x86 = env::var("ProgramFiles(x86)")
        .or_else(|_| env::var("ProgramFiles"))
        .ok();
    let Some(program_files_x86) = program_files_x86 else {
        return Ok(None);
    };
    if !Path::new(&program_files_x86).is_absolute() {
        return Err(KuError::message(
            "Visual Studio discovery root is not absolute; refusing to execute vswhere",
        ));
    }
    let vswhere = Path::new(&program_files_x86)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !path_is_plain_regular_file(&vswhere) {
        return Ok(None);
    }
    let mut command = Command::new(&vswhere);
    command.args([
        "-utf8",
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
    ]);
    let timeout = remaining_build_phase_timeout(
        deadline,
        BUILD_TOOL_PROBE_TIMEOUT,
        "Visual Studio discovery",
    )
    .map_err(|error| KuError::message(error.to_string()))?;
    let (status, stdout) =
        run_build_process_capture_stdout(&mut command, timeout, MAX_BUILD_TOOL_CAPTURE_BYTES)
            .map_err(|error| {
                KuError::message(format!(
                    "failed to run bounded Visual Studio discovery '{}': {error}",
                    vswhere.display()
                ))
            })?;
    if !status.success() {
        return Err(KuError::message(format!(
            "Visual Studio discovery '{}' exited with {status}",
            vswhere.display()
        )));
    }
    let install = std::str::from_utf8(&stdout)
        .map_err(|_| KuError::message("vswhere returned a non-UTF-8 installation path"))?
        .trim()
        .to_string();
    if install.is_empty() {
        return Ok(None);
    }
    let vcvars = Path::new(&install)
        .join("VC")
        .join("Auxiliary")
        .join("Build")
        .join("vcvars64.bat");
    if !path_is_plain_regular_file(&vcvars) {
        return Err(KuError::message(format!(
            "Visual Studio discovery returned '{}', but vcvars64.bat is missing",
            install
        )));
    }
    Ok(Some(vcvars))
}

/// Compile a generated C source with MSVC `cl.exe`. cl needs INCLUDE/LIB/PATH
/// from `vcvars64.bat`, so we snapshot that environment once and inject it into
/// the cl process directly (no `cmd`, which cannot handle the `\\?\` verbatim
/// paths that `fs::canonicalize` produces). `/utf-8` keeps diagnostics readable
/// regardless of the console code page.
#[derive(Clone, Copy)]
struct MsvcDatabaseLinks<'a> {
    libpq: Option<&'a Path>,
    libmysql: Option<&'a Path>,
    libmysql_include: Option<&'a Path>,
}

#[derive(Clone, Copy)]
struct MsvcCompileOptions {
    profile: BuildProfile,
    static_link: bool,
    verbose: bool,
    deadline: Instant,
}

fn compile_with_msvc(
    vcvars: &Path,
    source: &Path,
    output: &Path,
    database: MsvcDatabaseLinks<'_>,
    options: MsvcCompileOptions,
) -> Result<(), KuError> {
    let env = load_vcvars_env(vcvars, options.deadline)?;
    let cl = find_cl_in_env(&env).ok_or_else(|| {
        KuError::message(
            "located Visual Studio but cl.exe was not on the vcvars PATH\nhelp: repair the \"Desktop development with C++\" workload, or install clang/gcc/zig and set KU_CC",
        )
    })?;

    // cl and cmd both dislike \\?\ verbatim paths; hand cl plain absolute paths.
    let source = strip_verbatim(source);
    let output = strip_verbatim(output);
    let obj = output.with_extension("obj");

    let mut command = Command::new(&cl);
    command.env_clear();
    for (key, value) in &env {
        command.env(key, value);
    }
    command.arg("/nologo").arg("/std:c11").arg("/utf-8");
    if let Some(opt) = options.profile.msvc_opt_flag() {
        command.arg(opt);
    }
    if options.static_link {
        command.arg("/MT");
    }
    if let Some(include) = database.libmysql_include {
        command.arg(format!("/I{}", strip_verbatim(include).display()));
    }
    if let Some(library) = database.libpq {
        if !path_is_plain_regular_file(library) {
            return Err(KuError::message(format!(
                "selected libpq library '{}' changed before linking; refusing to fall back",
                library.display()
            )));
        }
    }
    command.arg(&source);
    command.arg(format!("/Fe:{}", output.display()));
    command.arg(format!("/Fo:{}", obj.display()));
    // Keep a single `/link` boundary: every following argument is consumed by
    // link.exe, so emitting another `/link` for a second database is invalid.
    if database.libpq.is_some() || database.libmysql.is_some() {
        command.arg("/link");
        if let Some(library) = database.libpq {
            command.arg(strip_verbatim(library));
        }
        if let Some(library) = database.libmysql {
            if !path_is_plain_regular_file(library) {
                return Err(KuError::message(format!(
                    "selected MySQL client library '{}' changed before linking; refusing to fall back",
                    library.display()
                )));
            }
            command.arg(strip_verbatim(library));
        }
    }
    if options.verbose {
        println!("msvc: {} (env from {})", cl.display(), vcvars.display());
    }

    let timeout = remaining_build_phase_timeout(
        options.deadline,
        C_COMPILER_PROCESS_TIMEOUT,
        "MSVC compiler execution",
    )
    .map_err(|error| KuError::message(error.to_string()))?;
    let result = run_build_process_bounded(&mut command, timeout);
    let _ = fs::remove_file(&obj);
    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(KuError::message(format!(
            "native C build failed: MSVC cl exited with {status}\nhelp: inspect generated source at {}",
            source.display()
        ))),
        Err(err) => Err(KuError::message(format!(
            "failed to run MSVC cl.exe: {err}\nhelp: repair Visual Studio C++ tools, or install clang/gcc/zig and set KU_CC"
        ))),
    }
}

/// Run `vcvars64.bat` and capture the resulting environment as key/value pairs.
/// We write a throwaway bat into a plain temp dir (not the `\\?\` build dir) and
/// pass the vcvars path as the script's first argument. Keeping that path out of
/// the ASCII script body preserves non-ASCII Visual Studio install paths. A
/// marker line separates any residual vcvars banner from the `set` dump.
fn windows_system_cmd_path() -> Result<PathBuf, KuError> {
    let root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| KuError::message("SystemRoot is missing; cannot locate system cmd.exe"))?;
    if !root.is_absolute() {
        return Err(KuError::message(
            "SystemRoot is not absolute; refusing to search for cmd.exe",
        ));
    }
    let cmd = root.join("System32").join("cmd.exe");
    if !path_is_plain_regular_file(&cmd) {
        return Err(KuError::message(format!(
            "system cmd.exe is missing at '{}'",
            cmd.display()
        )));
    }
    Ok(cmd)
}

fn decode_utf16le(bytes: &[u8], what: &str) -> Result<String, KuError> {
    if bytes.len() % 2 != 0 {
        return Err(KuError::message(format!(
            "{what} returned truncated UTF-16LE output"
        )));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
        .map_err(|_| KuError::message(format!("{what} returned invalid UTF-16LE output")))
}

fn load_vcvars_env(vcvars: &Path, deadline: Instant) -> Result<Vec<(String, String)>, KuError> {
    const MARKER: &str = "___KU_VCVARS_ENV___";
    let mut bat = BuildTemporaryPath::new("vcvars-probe", "bat")
        .map_err(|err| KuError::message(format!("failed to reserve a vcvars probe path: {err}")))?;
    let script = format!(
        "@echo off\r\ncall \"%~1\" >nul 2>&1\r\nif errorlevel 1 exit /b 1\r\nif not errorlevel 0 exit /b 1\r\necho {MARKER}\r\nset\r\n"
    );
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(bat.as_path())
        .map_err(|err| KuError::message(format!("failed to create vcvars probe script: {err}")))?;
    bat.arm();
    if let Err(err) = file.write_all(script.as_bytes()) {
        drop(file);
        return Err(KuError::message(format!(
            "failed to write vcvars probe script: {err}"
        )));
    }
    drop(file);
    let mut command = Command::new(windows_system_cmd_path()?);
    command
        .args(["/D", "/U", "/C"])
        .arg(bat.as_path())
        .arg(vcvars);
    let timeout = remaining_build_phase_timeout(
        deadline,
        BUILD_TOOL_PROBE_TIMEOUT,
        "vcvars64 environment probe",
    )
    .map_err(|error| KuError::message(error.to_string()))?;
    let result =
        run_build_process_capture_stdout(&mut command, timeout, MAX_BUILD_TOOL_CAPTURE_BYTES);
    let (status, stdout) =
        result.map_err(|err| KuError::message(format!("failed to run vcvars64.bat: {err}")))?;
    if !status.success() {
        return Err(KuError::message(
            "vcvars64.bat failed to initialize the MSVC build environment",
        ));
    }
    let text = decode_utf16le(&stdout, "vcvars64.bat")?;
    let mut vars = Vec::new();
    let mut seen_marker = false;
    for line in text.lines() {
        if !seen_marker {
            seen_marker = line.trim() == MARKER;
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if !key.is_empty() {
                vars.push((key.to_string(), value.to_string()));
            }
        }
    }
    if vars.is_empty() {
        return Err(KuError::message(
            "vcvars64.bat produced no environment; the MSVC C++ tools may be missing",
        ));
    }
    Ok(vars)
}

/// Find cl.exe by scanning the PATH captured from vcvars.
fn find_cl_in_env(env: &[(String, String)]) -> Option<PathBuf> {
    let path = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())?;
    for dir in path.split(';') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join("cl.exe");
        if path_is_plain_regular_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Strip a Windows `\\?\` verbatim prefix so downstream tools see a normal path.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn c_compiler_candidates(env_cc: Option<&str>) -> Vec<CCompilerCandidate> {
    if let Some(env_cc) = env_cc {
        return parse_c_compiler_candidate(env_cc, true)
            .into_iter()
            .collect();
    }
    let mut candidates: Vec<CCompilerCandidate> = Vec::new();
    for fallback in ["zig cc", "clang", "cc", "gcc"] {
        if let Some(candidate) = parse_c_compiler_candidate(fallback, false) {
            if !candidates
                .iter()
                .any(|existing| existing.label == candidate.label)
            {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn configured_c_compiler(value: Option<OsString>) -> Result<Option<String>, KuError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.into_string().map_err(|_| {
        KuError::message("KU_CC must be valid Unicode and name exactly one compiler command")
    })?;
    let words = split_command_words(&value).map_err(KuError::message)?;
    if words.is_empty() {
        return Err(KuError::message(
            "KU_CC is set but empty; unset it for automatic compiler discovery or set one compiler command",
        ));
    }
    let program_name = Path::new(&words[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&words[0])
        .to_ascii_lowercase();
    if matches!(
        program_name.as_str(),
        "cl" | "cl.exe" | "clang-cl" | "clang-cl.exe"
    ) {
        return Err(KuError::message(
            "KU_CC requires a GCC-compatible driver command; use clang/zig cc/gcc, or unset KU_CC for automatic MSVC discovery",
        ));
    }
    Ok(Some(value))
}

fn parse_c_compiler_candidate(
    value: &str,
    explicitly_configured: bool,
) -> Option<CCompilerCandidate> {
    let parts = split_command_words(value).ok()?;
    let (program, args) = parts.split_first()?;
    let label = parts.join(" ");
    let program_name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    let kind = if matches!(program_name.as_str(), "zig" | "zig.exe")
        && args.first().is_some_and(|arg| arg == "cc")
    {
        CCompilerKind::ZigCc
    } else if program_name.contains("clang") {
        CCompilerKind::Clang
    } else {
        CCompilerKind::Preconfigured
    };
    Some(CCompilerCandidate {
        label,
        program: program.clone(),
        args: args.to_vec(),
        kind,
        explicitly_configured,
    })
}

fn split_command_words(value: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in value.chars() {
        match character {
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if quoted {
        return Err("KU_CC contains an unmatched double quote");
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn reject_native_async(program: &Program) -> Result<(), KuError> {
    reject_compiled_async(
        program,
        "native C prototype does not support async/await yet; use the interpreter runtime",
    )
}

fn reject_compiled_async(program: &Program, message: &str) -> Result<(), KuError> {
    if program.items.iter().any(item_contains_async) {
        return Err(KuError::message(message));
    }
    Ok(())
}

fn item_contains_async(item: &Item) -> bool {
    match item {
        Item::Function(function) => function_contains_async(function),
        Item::Import(_) | Item::Struct(_) | Item::Enum(_) | Item::Module(_) => false,
    }
}

fn function_contains_async(function: &FnDecl) -> bool {
    function.is_async || function.body.iter().any(stmt_contains_async)
}

fn stmt_contains_async(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::VarDecl { value, .. }
        | Stmt::Assign { value, .. }
        | Stmt::Fail { value, .. }
        | Stmt::Panic { value, .. }
        | Stmt::Print { value, .. } => expr_contains_await(value),
        Stmt::AssignTarget { target, value, .. } => {
            assign_target_contains_await(target) || expr_contains_await(value)
        }
        Stmt::CompoundAssign { target, value, .. } => {
            assign_target_contains_await(target) || expr_contains_await(value)
        }
        Stmt::DestructureAssign { values, .. } => values.iter().any(expr_contains_await),
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            expr_contains_await(value)
                || bindings
                    .iter()
                    .any(|binding| binding.default.as_ref().is_some_and(expr_contains_await))
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_await(condition)
                || then_branch.iter().any(stmt_contains_async)
                || else_branch.iter().any(stmt_contains_async)
        }
        Stmt::While {
            condition, body, ..
        } => expr_contains_await(condition) || body.iter().any(stmt_contains_async),
        Stmt::For { iterable, body, .. } => {
            expr_contains_await(iterable) || body.iter().any(stmt_contains_async)
        }
        Stmt::Function(function) => function_contains_async(function),
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            body.iter().any(stmt_contains_async)
                || catch_body.iter().any(stmt_contains_async)
                || finally_body.iter().any(stmt_contains_async)
        }
        Stmt::Return { value, .. } => value.as_ref().is_some_and(expr_contains_await),
        Stmt::Expr { expr, .. } => expr_contains_await(expr),
        Stmt::Break { .. } | Stmt::Continue { .. } => false,
    }
}

fn assign_target_contains_await(target: &AssignTarget) -> bool {
    match target {
        AssignTarget::Variable(_) => false,
        AssignTarget::Index { target, index } => {
            expr_contains_await(target) || expr_contains_await(index)
        }
        AssignTarget::Field { target, .. } => expr_contains_await(target),
    }
}

fn expr_contains_await(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Await(_) => true,
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => expr_contains_await(expr),
        ExprKind::Binary { left, right, .. } => {
            expr_contains_await(left) || expr_contains_await(right)
        }
        ExprKind::Call { callee, args } => {
            expr_contains_await(callee) || args.iter().any(expr_contains_await)
        }
        ExprKind::Array(values) => values.iter().any(expr_contains_await),
        ExprKind::Index { target, index } => {
            expr_contains_await(target) || expr_contains_await(index)
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            expr_contains_await(target)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            fields.iter().any(|(_, value)| expr_contains_await(value))
        }
        ExprKind::Match { value, arms } => {
            expr_contains_await(value)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(expr_contains_await)
                        || expr_contains_await(&arm.value)
                })
        }
        ExprKind::Function { body, .. } => body.iter().any(stmt_contains_async),
        ExprKind::Literal(_) | ExprKind::Variable(_) => false,
    }
}

struct TempBuildDir {
    path: PathBuf,
}

impl TempBuildDir {
    #[cfg(test)]
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn create_private(label: &str) -> io::Result<Self> {
        const MAX_ATTEMPTS: usize = 8;
        for _ in 0..MAX_ATTEMPTS {
            let path = unique_build_tool_path(label, "dir")?;
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = fs::DirBuilder::new();
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique private build directory",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Drop for TempBuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn build_runner_source(path: &str, source: &str, dependency_mode: DependencyResolveMode) -> String {
    let literal = raw_string_literal(source);
    let dependency_mode = match dependency_mode {
        DependencyResolveMode::Refresh => "Refresh",
        DependencyResolveMode::Update => "Update",
        DependencyResolveMode::Locked => "Locked",
        DependencyResolveMode::Offline => "Offline",
    };
    format!(
        "const SOURCE: &str = {literal};\nfn main() {{\n    if let Err(err) = ku::cli::run_source_with_dependency_mode(\n        {path:?},\n        SOURCE,\n        ku::package::DependencyResolveMode::{dependency_mode},\n    ) {{\n        eprintln!(\"{{err}}\");\n        std::process::exit(1);\n    }}\n}}\n"
    )
}

fn raw_string_literal(source: &str) -> String {
    for hashes in 0..16 {
        let fence = "#".repeat(hashes);
        let close = format!("\"{fence}");
        if !source.contains(&close) {
            return format!("r{fence}\"{source}\"{fence}");
        }
    }
    format!("{source:?}")
}

fn find_ku_rlib(exe_dir: &Path) -> Result<PathBuf, KuError> {
    let direct = exe_dir.join("libku.rlib");
    let deps = exe_dir.join("deps");
    let mut candidates = Vec::new();
    if direct.is_file() {
        candidates.push(direct.clone());
    }
    if let Ok(entries) = fs::read_dir(&deps) {
        for (index, entry) in entries.enumerate() {
            if index >= MAX_RLIB_DIRECTORY_ENTRIES {
                return Err(KuError::message(format!(
                    "ku build stopped after scanning {MAX_RLIB_DIRECTORY_ENTRIES} entries in '{}'; remove stale target artifacts and retry",
                    deps.display()
                )));
            }
            let entry = entry.map_err(|err| {
                KuError::message(format!(
                    "failed to inspect Rust dependency directory '{}': {err}",
                    deps.display()
                ))
            })?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("libku-") && name.ends_with(".rlib") && path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop().ok_or_else(|| {
        KuError::message(format!(
            "ku build needs libku.rlib next to the ku executable; looked in {} and {}",
            direct.display(),
            deps.display()
        ))
    })
}

fn find_dependency_dirs(exe_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    push_existing_dir(&mut dirs, exe_dir.join("deps"));
    if exe_dir.file_name().is_some_and(|name| name == "release") {
        if let Some(repo) = exe_dir.parent().and_then(Path::parent) {
            push_existing_dir(&mut dirs, repo.join("target").join("release").join("deps"));
        }
    }
    if exe_dir.file_name().is_some_and(|name| name == "debug") {
        if let Some(repo) = exe_dir.parent().and_then(Path::parent) {
            push_existing_dir(&mut dirs, repo.join("target").join("debug").join("deps"));
        }
    }
    if exe_dir.file_name().is_some_and(|name| name == "release") {
        if let Some(repo) = exe_dir.parent() {
            push_existing_dir(&mut dirs, repo.join("target").join("release").join("deps"));
        }
    }
    if exe_dir.file_name().is_some_and(|name| name == "debug") {
        if let Some(repo) = exe_dir.parent() {
            push_existing_dir(&mut dirs, repo.join("target").join("debug").join("deps"));
        }
    }
    dirs
}

fn push_existing_dir(dirs: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() && !dirs.iter().any(|existing| existing == &path) {
        dirs.push(path);
    }
}

fn expected_ku_file(path: &str) -> KuError {
    KuError::message(expected_ku_file_message(path))
}

fn expected_ku_file_message(path: &str) -> String {
    format!("expected a .ku source file, got '{path}'")
}

fn command_error(message: impl Into<String>) -> KuError {
    KuError::message(format!("{}\n\n{}", message.into(), HELP))
}

fn diagnostic_json_line(error: &KuError, file: &str, source: &str) -> String {
    let diagnostic = error.diagnostic_data(file, source);
    format!(
        "{{\"level\":{},\"code\":{},\"message\":{},\"file\":{},\"line\":{},\"column\":{},\"endLine\":{},\"endColumn\":{},\"notes\":{},\"helps\":{}}}",
        json_string(diagnostic.level),
        json_string(diagnostic.code),
        json_string(&diagnostic.message),
        json_string(&diagnostic.file),
        diagnostic.line,
        diagnostic.column,
        diagnostic.end_line,
        diagnostic.end_column,
        json_string_array(&diagnostic.notes),
        json_string_array(&diagnostic.helps),
    )
}

fn json_string_array(values: &[&str]) -> String {
    let values = values
        .iter()
        .map(|value| json_string(value))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{values}]")
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch <= '\u{1F}' => {
                output.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn is_ku_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
}

fn looks_like_file_path(path: &str) -> bool {
    Path::new(path).extension().is_some() || path.contains('/') || path.contains('\\')
}

pub fn check_source(file: &str, source: &str) -> Result<(), KuError> {
    check_source_with_dependency_mode(file, source, DependencyResolveMode::Update)
}

pub fn check_source_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<(), KuError> {
    check_source_with_options(
        file,
        source,
        CheckOptions {
            dependency_mode,
            ..CheckOptions::default()
        },
    )
}

fn check_source_with_options(
    file: &str,
    source: &str,
    options: CheckOptions,
) -> Result<(), KuError> {
    parse_and_check_with_options(file, source, options)
        .map(|_| ())
        .map_err(|err| KuError::message(err.diagnostic(file, source)))
}

fn checked_program_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<Program, KuError> {
    let program = parse_and_check_with_dependency_mode(file, source, dependency_mode)
        .map_err(|err| KuError::message(err.diagnostic(file, source)))?;
    Ok(program)
}

pub fn run_source(file: &str, source: &str) -> Result<(), KuError> {
    run_source_with_dependency_mode(file, source, DependencyResolveMode::Update)
}

pub fn run_source_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<(), KuError> {
    let program = checked_program_with_dependency_mode(file, source, dependency_mode)?;
    run_program_with_stack(program, source_base_dir(file))
        .map_err(|err| KuError::message(err.diagnostic(file, source)))
}

fn run_program_with_stack(program: Program, base_dir: PathBuf) -> Result<(), KuError> {
    thread::Builder::new()
        .name("ku-interpreter".to_string())
        .stack_size(INTERPRETER_STACK_SIZE)
        .spawn(move || {
            let mut interpreter = Interpreter::with_base_dir(base_dir);
            interpreter.run(program)
        })
        .map_err(|err| KuError::message(format!("failed to start interpreter: {err}")))?
        .join()
        .map_err(|_| KuError::message("interpreter thread panicked"))?
}

fn source_base_dir(file: &str) -> PathBuf {
    Path::new(file)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn parse_source(source: &str) -> Result<Program, KuError> {
    let tokens = Lexer::new(source).tokenize()?;
    Parser::new(tokens).parse()
}

fn parse_and_check(file: &str, source: &str) -> Result<Program, KuError> {
    parse_and_check_with_dependency_mode(file, source, DependencyResolveMode::Update)
}

fn parse_and_check_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<Program, KuError> {
    parse_and_check_with_options(
        file,
        source,
        CheckOptions {
            dependency_mode,
            ..CheckOptions::default()
        },
    )
}

fn parse_and_check_with_options(
    file: &str,
    source: &str,
    options: CheckOptions,
) -> Result<Program, KuError> {
    let original = parse_source(source)?;
    deny_unused_imports(&original)?;
    let program = parse_and_expand_with_dependency_mode(file, source, options.dependency_mode)?;
    Checker::new().check(&program)?;
    if options.deny_unused {
        deny_unused_local_bindings(&program)?;
    }
    Ok(program)
}

#[derive(Debug, Clone)]
struct UnusedBinding {
    name: String,
    span: Span,
    used: bool,
}

#[derive(Debug, Default)]
struct UnusedScope {
    bindings: Vec<UnusedBinding>,
}

#[derive(Debug, Default)]
struct UnusedAnalyzer {
    scopes: Vec<UnusedScope>,
}

impl UnusedAnalyzer {
    fn new() -> Self {
        Self {
            scopes: vec![UnusedScope::default()],
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(UnusedScope::default());
    }

    fn pop_scope(&mut self) -> KuResult<()> {
        let Some(scope) = self.scopes.pop() else {
            return Ok(());
        };
        for binding in scope.bindings {
            if !binding.used {
                return Err(unused_binding_error(&binding.name, binding.span));
            }
        }
        Ok(())
    }

    fn define(&mut self, name: &str, span: Span, track: bool) {
        if name == "_" || name.starts_with('_') {
            return;
        }
        let used = !track;
        if let Some(scope) = self.scopes.last_mut() {
            if let Some(existing) = scope
                .bindings
                .iter_mut()
                .find(|binding| binding.name == name)
            {
                existing.span = span;
                existing.used = used;
            } else {
                scope.bindings.push(UnusedBinding {
                    name: name.to_string(),
                    span,
                    used,
                });
            }
        }
    }

    fn binding_exists(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.bindings.iter().any(|binding| binding.name == name))
    }

    fn use_name(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope
                .bindings
                .iter_mut()
                .rev()
                .find(|binding| binding.name == name)
            {
                binding.used = true;
                return;
            }
        }
    }

    fn write_name(&mut self, name: &str, span: Span) {
        if name == "_" || name.starts_with('_') {
            return;
        }
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope
                .bindings
                .iter_mut()
                .rev()
                .find(|binding| binding.name == name)
            {
                binding.span = span;
                binding.used = false;
                return;
            }
        }
        self.define(name, span, true);
    }

    fn finish(mut self) -> KuResult<()> {
        while !self.scopes.is_empty() {
            self.pop_scope()?;
        }
        Ok(())
    }

    fn visit_items(&mut self, items: &[Item]) -> KuResult<()> {
        for item in items {
            if let Item::Function(function) = item {
                self.visit_function(function, false)?;
            }
        }
        Ok(())
    }

    fn visit_function(&mut self, function: &FnDecl, track_name: bool) -> KuResult<()> {
        if track_name {
            self.define(&function.name, function.span, true);
        }
        self.push_scope();
        if track_name {
            self.define(&function.name, function.span, false);
        }
        for param in &function.params {
            self.define(&param.name, param.span, false);
        }
        self.visit_block(&function.body)?;
        self.pop_scope()
    }

    fn visit_block(&mut self, body: &[Stmt]) -> KuResult<()> {
        for stmt in body {
            self.visit_stmt(stmt)?;
        }
        Ok(())
    }

    fn visit_scoped_block(&mut self, body: &[Stmt]) -> KuResult<()> {
        self.push_scope();
        self.visit_block(body)?;
        self.pop_scope()
    }

    fn visit_stmt(&mut self, stmt: &Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name, value, span, ..
            } => {
                self.visit_expr(value)?;
                self.define(name, *span, true);
            }
            Stmt::Assign { name, value, span } => {
                self.visit_expr(value)?;
                if self.binding_exists(name) {
                    self.write_name(name, *span);
                } else {
                    self.define(name, *span, true);
                }
            }
            Stmt::AssignTarget { target, value, .. } => {
                self.visit_assign_target(target)?;
                self.visit_expr(value)?;
            }
            Stmt::CompoundAssign { target, value, .. } => {
                self.visit_assign_target(target)?;
                self.visit_expr(value)?;
            }
            Stmt::DestructureAssign {
                names,
                values,
                span,
            } => {
                for value in values {
                    self.visit_expr(value)?;
                }
                for name in names.iter().flatten() {
                    self.define(name, *span, true);
                }
            }
            Stmt::ObjectDestructureAssign {
                bindings,
                rest,
                value,
                ..
            } => {
                self.visit_expr(value)?;
                for binding in bindings {
                    if let Some(default) = &binding.default {
                        self.visit_expr(default)?;
                    }
                    let local = binding.local.as_deref().unwrap_or(&binding.field);
                    self.define(local, binding.span, true);
                }
                if let Some(rest) = rest {
                    if let Some(local) = &rest.local {
                        self.define(local, rest.span, true);
                    }
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.visit_expr(condition)?;
                self.visit_scoped_block(then_branch)?;
                if !else_branch.is_empty() {
                    self.visit_scoped_block(else_branch)?;
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.visit_expr(condition)?;
                self.visit_scoped_block(body)?;
            }
            Stmt::For {
                name,
                iterable,
                body,
                span,
            } => {
                self.visit_expr(iterable)?;
                self.push_scope();
                self.define(name, *span, true);
                self.visit_block(body)?;
                self.pop_scope()?;
            }
            Stmt::Function(function) => {
                self.visit_function(function, true)?;
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                span,
            } => {
                self.visit_scoped_block(body)?;
                if !catch_body.is_empty() {
                    self.push_scope();
                    if let Some(catch_name) = catch_name {
                        self.define(catch_name, *span, true);
                    }
                    self.visit_block(catch_body)?;
                    self.pop_scope()?;
                }
                if !finally_body.is_empty() {
                    self.visit_scoped_block(finally_body)?;
                }
            }
            Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
                self.visit_expr(value)?;
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.visit_expr(value)?;
                }
            }
            Stmt::Expr { expr, .. } => {
                self.visit_expr(expr)?;
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
        Ok(())
    }

    fn visit_assign_target(&mut self, target: &AssignTarget) -> KuResult<()> {
        match target {
            AssignTarget::Variable(name) => self.use_name(name),
            AssignTarget::Index { target, index } => {
                self.visit_expr(target)?;
                self.visit_expr(index)?;
            }
            AssignTarget::Field { target, .. } => {
                self.visit_expr(target)?;
            }
        }
        Ok(())
    }

    fn visit_expr(&mut self, expr: &Expr) -> KuResult<()> {
        match &expr.kind {
            ExprKind::Literal(Literal::TemplateString(text)) => {
                self.visit_template_string(text, expr.span)?;
            }
            ExprKind::Literal(_) => {}
            ExprKind::Variable(name) => self.use_name(name),
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
                self.visit_expr(expr)?;
            }
            ExprKind::Binary { left, right, .. } => {
                self.visit_expr(left)?;
                self.visit_expr(right)?;
            }
            ExprKind::Call { callee, args } => {
                self.visit_expr(callee)?;
                for arg in args {
                    self.visit_expr(arg)?;
                }
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.visit_expr(value)?;
                }
            }
            ExprKind::Index { target, index } => {
                self.visit_expr(target)?;
                self.visit_expr(index)?;
            }
            ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
                self.visit_expr(target)?;
            }
            ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                for (_, value) in fields {
                    self.visit_expr(value)?;
                }
            }
            ExprKind::Match { value, arms } => {
                self.visit_expr(value)?;
                for arm in arms {
                    self.push_scope();
                    self.define_match_pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard)?;
                    }
                    self.visit_expr(&arm.value)?;
                    self.pop_scope()?;
                }
            }
            ExprKind::Function { params, body, .. } => {
                self.push_scope();
                for param in params {
                    self.define(&param.name, param.span, false);
                }
                self.visit_block(body)?;
                self.pop_scope()?;
            }
        }
        Ok(())
    }

    fn visit_template_string(&mut self, text: &str, base_span: Span) -> KuResult<()> {
        let mut chars = text.char_indices().peekable();
        while let Some((_, ch)) = chars.next() {
            if ch == '\\' {
                let _ = chars.next();
                continue;
            }
            if ch != '{' {
                continue;
            }
            let mut expr = String::new();
            let mut depth = 1usize;
            let iter = chars.by_ref();
            while let Some((_, inner)) = iter.next() {
                match inner {
                    '\\' => {
                        expr.push(inner);
                        if let Some((_, escaped)) = iter.next() {
                            expr.push(escaped);
                        }
                    }
                    '{' => {
                        depth += 1;
                        expr.push(inner);
                    }
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                        expr.push(inner);
                    }
                    _ => expr.push(inner),
                }
            }
            if depth == 0 && !expr.trim().is_empty() {
                let parsed = Lexer::new(&expr)
                    .tokenize()
                    .and_then(|tokens| Parser::new(tokens).parse_expression_only())
                    .map_err(|err| KuError::runtime(err.message, base_span))?;
                self.visit_expr(&parsed)?;
            }
        }
        Ok(())
    }

    fn define_match_pattern(&mut self, pattern: &MatchPattern) {
        match pattern {
            MatchPattern::Binding(name) => self.define(name, Span::default(), true),
            MatchPattern::EnumVariant { fields, .. } => {
                for field in fields {
                    self.define_match_pattern(field);
                }
            }
            MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
        }
    }
}

fn deny_unused_local_bindings(program: &Program) -> KuResult<()> {
    let mut analyzer = UnusedAnalyzer::new();
    analyzer.visit_items(&program.items)?;
    analyzer.finish()
}

fn deny_unused_imports(program: &Program) -> KuResult<()> {
    let used = collect_import_name_references(program);
    for item in &program.items {
        let Item::Import(import) = item else {
            continue;
        };
        if is_std_import_path(&import.path) && std_import_modules(import).is_err() {
            continue;
        }
        match &import.kind {
            ImportKind::Named(names) => {
                for name in names {
                    let local = name.local_name();
                    if local == "_" || local.starts_with('_') {
                        continue;
                    }
                    if !used.contains(local) {
                        return Err(unused_import_error(local, name.span));
                    }
                }
            }
            ImportKind::Namespace(namespace) => {
                if namespace == "_" || namespace.starts_with('_') {
                    continue;
                }
                if !used.contains(namespace) {
                    return Err(unused_import_error(namespace, import.span));
                }
            }
            ImportKind::Glob => {}
        }
    }
    Ok(())
}

fn unused_binding_error(name: &str, span: Span) -> KuError {
    KuError::runtime(
        format!(
            "unused local binding '{name}'; remove it, use it, or rename it to '_{name}' when it is intentionally unused"
        ),
        span,
    )
}

fn unused_import_error(name: &str, span: Span) -> KuError {
    KuError::runtime(
        format!(
            "unused import '{name}'; remove it, use it, or rename it to '_{name}' when it is intentionally unused"
        ),
        span,
    )
}

fn collect_import_name_references(program: &Program) -> HashSet<String> {
    let mut used = HashSet::new();
    for item in &program.items {
        match item {
            Item::Import(_) | Item::Module(_) => {}
            Item::Function(function) => collect_function_references(function, &mut used),
            Item::Struct(decl) => {
                for field in &decl.fields {
                    if let Some(ty) = &field.ty {
                        collect_type_references(ty, &mut used);
                    }
                }
            }
            Item::Enum(decl) => {
                for variant in &decl.variants {
                    for field in &variant.fields {
                        if let Some(ty) = &field.ty {
                            collect_type_references(ty, &mut used);
                        }
                    }
                }
            }
        }
    }
    used
}

fn collect_function_references(function: &FnDecl, used: &mut HashSet<String>) {
    for param in &function.params {
        if let Some(ty) = &param.ty {
            collect_type_references(ty, used);
        }
    }
    if let Some(return_type) = &function.return_type {
        collect_type_references(return_type, used);
    }
    collect_stmt_references(&function.body, used);
}

fn collect_stmt_references(body: &[Stmt], used: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::VarDecl { ty, value, .. } => {
                if let Some(ty) = ty {
                    collect_type_references(ty, used);
                }
                collect_expr_references(value, used);
            }
            Stmt::Assign { value, .. } | Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => {
                collect_expr_references(value, used)
            }
            Stmt::Print { value, .. } => collect_expr_references(value, used),
            Stmt::AssignTarget { target, value, .. }
            | Stmt::CompoundAssign { target, value, .. } => {
                collect_assign_target_references(target, used);
                collect_expr_references(value, used);
            }
            Stmt::DestructureAssign { values, .. } => {
                for value in values {
                    collect_expr_references(value, used);
                }
            }
            Stmt::ObjectDestructureAssign {
                bindings, value, ..
            } => {
                collect_expr_references(value, used);
                for binding in bindings {
                    if let Some(default) = &binding.default {
                        collect_expr_references(default, used);
                    }
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                collect_expr_references(condition, used);
                collect_stmt_references(then_branch, used);
                collect_stmt_references(else_branch, used);
            }
            Stmt::While {
                condition, body, ..
            } => {
                collect_expr_references(condition, used);
                collect_stmt_references(body, used);
            }
            Stmt::For { iterable, body, .. } => {
                collect_expr_references(iterable, used);
                collect_stmt_references(body, used);
            }
            Stmt::Function(function) => collect_function_references(function, used),
            Stmt::Try {
                body,
                catch_body,
                finally_body,
                ..
            } => {
                collect_stmt_references(body, used);
                collect_stmt_references(catch_body, used);
                collect_stmt_references(finally_body, used);
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    collect_expr_references(value, used);
                }
            }
            Stmt::Expr { expr, .. } => collect_expr_references(expr, used),
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }
}

fn collect_assign_target_references(target: &AssignTarget, used: &mut HashSet<String>) {
    match target {
        AssignTarget::Variable(name) => collect_name_reference(name, used),
        AssignTarget::Index { target, index } => {
            collect_expr_references(target, used);
            collect_expr_references(index, used);
        }
        AssignTarget::Field { target, .. } => collect_expr_references(target, used),
    }
}

fn collect_expr_references(expr: &Expr, used: &mut HashSet<String>) {
    match &expr.kind {
        ExprKind::Variable(name) => collect_name_reference(name, used),
        ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
            collect_expr_references(expr, used)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr_references(left, used);
            collect_expr_references(right, used);
        }
        ExprKind::Call { callee, args } => {
            collect_expr_references(callee, used);
            for arg in args {
                collect_expr_references(arg, used);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_expr_references(value, used);
            }
        }
        ExprKind::Index { target, index } => {
            collect_expr_references(target, used);
            collect_expr_references(index, used);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_expr_references(target, used);
        }
        ExprKind::StructLiteral { name, fields } => {
            collect_name_reference(name, used);
            for (_, value) in fields {
                collect_expr_references(value, used);
            }
        }
        ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_expr_references(value, used);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_expr_references(value, used);
            for arm in arms {
                collect_match_pattern_references(&arm.pattern, used);
                if let Some(guard) = &arm.guard {
                    collect_expr_references(guard, used);
                }
                collect_expr_references(&arm.value, used);
            }
        }
        ExprKind::Function {
            params,
            return_type,
            body,
        } => {
            for param in params {
                if let Some(ty) = &param.ty {
                    collect_type_references(ty, used);
                }
            }
            if let Some(return_type) = return_type {
                collect_type_references(return_type, used);
            }
            collect_stmt_references(body, used);
        }
        ExprKind::Literal(_) => {}
    }
}

fn collect_match_pattern_references(pattern: &MatchPattern, used: &mut HashSet<String>) {
    if let MatchPattern::EnumVariant {
        enum_name, fields, ..
    } = pattern
    {
        collect_name_reference(enum_name, used);
        for field in fields {
            collect_match_pattern_references(field, used);
        }
    }
}

fn collect_type_references(ty: &TypeName, used: &mut HashSet<String>) {
    match ty {
        TypeName::Array(inner) | TypeName::Result(inner) => collect_type_references(inner, used),
        TypeName::Function {
            params,
            return_type,
            ..
        } => {
            for param in params {
                collect_type_references(param, used);
            }
            collect_type_references(return_type, used);
        }
        TypeName::Union(types) => {
            for ty in types {
                collect_type_references(ty, used);
            }
        }
        TypeName::Custom(name) => collect_name_reference(name, used),
        TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::String | TypeName::Null => {}
    }
}

fn collect_name_reference(name: &str, used: &mut HashSet<String>) {
    used.insert(name.to_string());
    if let Some((namespace, _)) = name.split_once('.') {
        used.insert(namespace.to_string());
    }
}

fn parse_and_expand_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<Program, KuError> {
    let program = parse_source(source)?;
    let program = if program_has_imports(&program) {
        let path = Path::new(file);
        if !path.exists() {
            if program_has_only_std_imports(&program) {
                let mut loader = ModuleLoader::new(None)?;
                loader.load_virtual_entry(path, program, source.len())?
            } else {
                return Err(KuError::runtime(
                    "imports require a real .ku file path",
                    Span::default(),
                ));
            }
        } else {
            let mut package = package::discover_for_file(path)?;
            let deadline = package
                .as_ref()
                .map(|_| package::package_operation_deadline());
            let _usage_lease = package
                .as_ref()
                .map(|package| {
                    package::acquire_package_usage_lease_until(
                        package,
                        deadline.expect("package operation deadline is present"),
                    )
                })
                .transpose()?;
            if let Some(package) = &mut package {
                package::ensure_cache_dir(package)?;
                package::resolve_remote_dependencies_with_mode_until(
                    package,
                    dependency_mode,
                    deadline.expect("package operation deadline is present"),
                )?;
            }
            let mut loader = ModuleLoader::new(package)?;
            let program = loader.load_entry(path, program, source.len())?;
            if matches!(
                dependency_mode,
                DependencyResolveMode::Update | DependencyResolveMode::Refresh
            ) {
                if let Some(package) = &loader.package {
                    package::write_lock_with_frozen_dependencies(
                        package,
                        &loader.dependency_snapshots,
                    )?;
                }
            }
            program
        }
    } else {
        program
    };
    Ok(program)
}

fn program_has_imports(program: &Program) -> bool {
    program
        .items
        .iter()
        .any(|item| matches!(item, Item::Import(_)))
}

fn program_has_only_std_imports(program: &Program) -> bool {
    program.items.iter().all(|item| match item {
        Item::Import(import) => is_std_import_path(&import.path),
        _ => true,
    })
}

fn is_std_import_path(path: &str) -> bool {
    path == "std" || path.starts_with("std.") || path.starts_with("std:")
}

struct ModuleExports {
    path: PathBuf,
    /// Source-facing export name -> the single compiler symbol interned for the
    /// defining module. Import aliases never create another declaration/name;
    /// they only rewrite references to this symbol.
    exports: BTreeMap<String, String>,
    closure: ModuleClosure,
}

const IMPORT_CLOSURE_WORDS: usize = MAX_IMPORT_MODULES.div_ceil(64);

#[derive(Clone)]
struct ModuleClosure {
    words: [u64; IMPORT_CLOSURE_WORDS],
}

impl ModuleClosure {
    fn new() -> Self {
        Self {
            words: [0; IMPORT_CLOSURE_WORDS],
        }
    }

    fn insert(&mut self, module_id: usize) {
        debug_assert!((1..=MAX_IMPORT_MODULES).contains(&module_id));
        let index = module_id - 1;
        self.words[index / 64] |= 1_u64 << (index % 64);
    }

    fn union_with(&mut self, other: &Self) {
        for (word, other) in self.words.iter_mut().zip(other.words.iter().copied()) {
            *word |= other;
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words
            .iter()
            .enumerate()
            .flat_map(|(word_index, word)| {
                let word = *word;
                (0..64).filter_map(move |bit| {
                    (word & (1_u64 << bit) != 0).then_some(word_index * 64 + bit + 1)
                })
            })
    }
}

#[derive(Clone, Copy, Default)]
struct ExpandedModuleMaterial {
    items: usize,
    source_bytes: u64,
}

impl ExpandedModuleMaterial {
    fn new(source_bytes: usize) -> Self {
        Self {
            items: 0,
            source_bytes: source_bytes as u64,
        }
    }

    fn add_item(&mut self) {
        self.items += 1;
    }
}

#[derive(Default)]
struct ImportBudget {
    modules: usize,
    active_depth: usize,
    source_bytes: u64,
    expanded_items: usize,
    cloned_source_bytes: u64,
    import_edges: usize,
    import_bindings: usize,
}

impl ImportBudget {
    fn begin_module(&mut self, span: Span) -> KuResult<()> {
        if self.modules >= MAX_IMPORT_MODULES {
            return Err(import_limit_error(
                "module_limit",
                format!("import graph exceeds {MAX_IMPORT_MODULES} source modules"),
                span,
            ));
        }
        if self.active_depth >= MAX_IMPORT_DEPTH {
            return Err(import_limit_error(
                "depth_limit",
                format!("import graph exceeds recursive depth {MAX_IMPORT_DEPTH}"),
                span,
            ));
        }
        self.modules += 1;
        self.active_depth += 1;
        Ok(())
    }

    fn finish_module(&mut self) {
        debug_assert!(self.active_depth > 0);
        self.active_depth -= 1;
    }

    fn charge_source(&mut self, bytes: usize, span: Span) -> KuResult<()> {
        let next = self.source_bytes.checked_add(bytes as u64).ok_or_else(|| {
            import_limit_error(
                "source_limit",
                "import graph source byte accounting overflowed",
                span,
            )
        })?;
        if next > MAX_IMPORT_SOURCE_BYTES {
            return Err(import_limit_error(
                "source_limit",
                format!("import graph source exceeds {MAX_IMPORT_SOURCE_BYTES} cumulative bytes"),
                span,
            ));
        }
        self.source_bytes = next;
        Ok(())
    }

    fn charge_item(&mut self, span: Span) -> KuResult<()> {
        self.charge_items(1, span)
    }

    fn charge_import_edge(&mut self, span: Span) -> KuResult<()> {
        self.import_edges = self.import_edges.checked_add(1).ok_or_else(|| {
            import_limit_error("edge_limit", "import edge accounting overflowed", span)
        })?;
        if self.import_edges > MAX_IMPORT_EDGES {
            return Err(import_limit_error(
                "edge_limit",
                format!("import graph exceeds {MAX_IMPORT_EDGES} import edges"),
                span,
            ));
        }
        Ok(())
    }

    fn charge_import_bindings(&mut self, count: usize, span: Span) -> KuResult<()> {
        self.import_bindings = self.import_bindings.checked_add(count).ok_or_else(|| {
            import_limit_error(
                "binding_limit",
                "import binding accounting overflowed",
                span,
            )
        })?;
        if self.import_bindings > MAX_IMPORT_BINDINGS {
            return Err(import_limit_error(
                "binding_limit",
                format!("import graph exceeds {MAX_IMPORT_BINDINGS} imported bindings"),
                span,
            ));
        }
        self.charge_items(count, span)
    }

    fn charge_items(&mut self, count: usize, span: Span) -> KuResult<()> {
        let next = self.expanded_items.checked_add(count).ok_or_else(|| {
            import_limit_error(
                "expanded_item_limit",
                "import expansion item accounting overflowed",
                span,
            )
        })?;
        if next > MAX_IMPORT_EXPANDED_ITEMS {
            return Err(import_limit_error(
                "expanded_item_limit",
                format!("import expansion exceeds {MAX_IMPORT_EXPANDED_ITEMS} materialized items"),
                span,
            ));
        }
        self.expanded_items = next;
        Ok(())
    }

    fn charge_clone(&mut self, material: ExpandedModuleMaterial, span: Span) -> KuResult<()> {
        let next_items = self
            .expanded_items
            .checked_add(material.items)
            .ok_or_else(|| {
                import_limit_error(
                    "expanded_item_limit",
                    "import expansion item accounting overflowed",
                    span,
                )
            })?;
        if next_items > MAX_IMPORT_EXPANDED_ITEMS {
            return Err(import_limit_error(
                "expanded_item_limit",
                format!("import expansion exceeds {MAX_IMPORT_EXPANDED_ITEMS} materialized items"),
                span,
            ));
        }
        let next_bytes = self
            .cloned_source_bytes
            .checked_add(material.source_bytes)
            .ok_or_else(|| {
                import_limit_error(
                    "expanded_clone_limit",
                    "import expansion clone accounting overflowed",
                    span,
                )
            })?;
        if next_bytes > MAX_IMPORT_CLONED_SOURCE_BYTES {
            return Err(import_limit_error(
                "expanded_clone_limit",
                format!(
                    "import expansion exceeds {MAX_IMPORT_CLONED_SOURCE_BYTES} source-equivalent cloned bytes"
                ),
                span,
            ));
        }
        self.expanded_items = next_items;
        self.cloned_source_bytes = next_bytes;
        Ok(())
    }
}

fn import_limit_error(code: &'static str, message: impl Into<String>, span: Span) -> KuError {
    KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "import",
        code,
        message,
        span,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Visiting,
    Done,
}

struct ModuleLoader {
    states: HashMap<PathBuf, LoadState>,
    modules: HashMap<PathBuf, Arc<ModuleExports>>,
    /// A module receives one identity for the whole compilation. The generated
    /// name is based on this id rather than on an individual import edge, so a
    /// diamond graph cannot split one nominal struct/enum into multiple types.
    module_ids: HashMap<PathBuf, usize>,
    next_module_id: usize,
    /// Canonical declarations in dependency-before-dependent order. Each
    /// source declaration is stored here once, independent of import diamonds.
    materialized_modules: Vec<Vec<Item>>,
    /// Source-equivalent size of each canonical module's stored declarations.
    /// This is parallel to `materialized_modules` and lets every real checker /
    /// final-program AST clone participate in the hard expansion budget.
    materialized_materials: Vec<ExpandedModuleMaterial>,
    materialized_order: Vec<usize>,
    materialized_names: HashSet<String>,
    package: Option<PackageContext>,
    package_import_scopes: Vec<package::PackageImportScope>,
    dependency_snapshots: Vec<package::LockDependency>,
    budget: ImportBudget,
}

impl ModuleLoader {
    fn new(package: Option<PackageContext>) -> KuResult<Self> {
        let package_import_scopes = package
            .as_ref()
            .map(|package| package::package_import_scopes(package, Span::default()))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            states: HashMap::new(),
            modules: HashMap::new(),
            module_ids: HashMap::new(),
            next_module_id: 0,
            materialized_modules: Vec::new(),
            materialized_materials: Vec::new(),
            materialized_order: Vec::new(),
            materialized_names: HashSet::new(),
            package,
            package_import_scopes,
            dependency_snapshots: Vec::new(),
            budget: ImportBudget::default(),
        })
    }

    fn load_virtual_entry(
        &mut self,
        path: &Path,
        program: Program,
        source_bytes: usize,
    ) -> KuResult<Program> {
        self.budget.begin_module(Span::default())?;
        let result = (|| {
            self.budget.charge_source(source_bytes, Span::default())?;
            self.expand_program(path, program, true, source_bytes)
                .map(|(program, _, _, _)| program)
        })();
        self.budget.finish_module();
        result
    }

    fn load_entry(
        &mut self,
        path: &Path,
        program: Program,
        source_bytes: usize,
    ) -> KuResult<Program> {
        let canonical = canonical_file(path, Span::default())?;
        self.budget.begin_module(Span::default())?;
        let result = (|| {
            self.budget.charge_source(source_bytes, Span::default())?;
            self.states.insert(canonical.clone(), LoadState::Visiting);
            let (expanded, _, _, _) =
                self.expand_program(&canonical, program, true, source_bytes)?;
            self.states.insert(canonical.clone(), LoadState::Done);
            let mut items = Vec::new();
            let mut materialized_names = HashSet::new();
            for module_id in &self.materialized_order {
                let material = self.materialized_materials[module_id - 1];
                self.budget.charge_clone(material, Span::default())?;
                for item in &self.materialized_modules[module_id - 1] {
                    push_materialized_item(&mut items, &mut materialized_names, item.clone());
                }
            }
            for item in expanded.items {
                if matches!(&item, Item::Module(module) if module.name.starts_with("std:")) {
                    push_materialized_item(&mut items, &mut materialized_names, item);
                } else {
                    // Preserve duplicate declarations written in the entry file;
                    // the checker owns that diagnostic.
                    items.push(item);
                }
            }
            Ok(Program { items })
        })();
        if result.is_err() {
            self.states.remove(&canonical);
        }
        self.budget.finish_module();
        result
    }

    fn load_module(&mut self, path: &Path, span: Span) -> KuResult<Arc<ModuleExports>> {
        let canonical = canonical_file(path, span)?;
        if self.states.get(&canonical) == Some(&LoadState::Visiting) {
            return Err(KuError::runtime(
                format!("circular import detected at {}", canonical.display()),
                span,
            ));
        }
        if let Some(module) = self.modules.get(&canonical) {
            return Ok(Arc::clone(module));
        }
        if !self.module_ids.contains_key(&canonical) {
            self.next_module_id = self.next_module_id.checked_add(1).ok_or_else(|| {
                import_limit_error("module_limit", "module identity counter overflowed", span)
            })?;
            self.module_ids
                .insert(canonical.clone(), self.next_module_id);
        }
        self.budget.begin_module(span)?;
        self.states.insert(canonical.clone(), LoadState::Visiting);
        let result = (|| {
            let source = read_import_source(&canonical, span)?;
            self.budget.charge_source(source.len(), span)?;
            let program = parse_source(&source).map_err(|err| {
                err.with_diagnostic_context(canonical.display().to_string(), source.clone())
            })?;
            let (expanded, material, exports, mut closure) = self
                .expand_program(&canonical, program, false, source.len())
                .map_err(|err| {
                    err.with_diagnostic_context(canonical.display().to_string(), source.clone())
                })?;
            let module_id = self.module_ids[&canonical];
            let mut check_items = Vec::new();
            let mut check_names = HashSet::new();
            for dependency_id in closure.iter() {
                let dependency_material = self.materialized_materials[dependency_id - 1];
                self.budget.charge_clone(dependency_material, span)?;
                for item in &self.materialized_modules[dependency_id - 1] {
                    push_materialized_item(&mut check_items, &mut check_names, item.clone());
                }
            }
            self.budget.charge_clone(material, span)?;
            for item in expanded.items.iter().cloned() {
                if matches!(&item, Item::Module(module) if module.name.starts_with("std:")) {
                    push_materialized_item(&mut check_items, &mut check_names, item);
                } else {
                    check_items.push(item);
                }
            }
            check_library_program(&Program { items: check_items }).map_err(|err| {
                err.with_diagnostic_context(canonical.display().to_string(), source.clone())
            })?;
            let dependency_snapshot =
                package::freeze_lock_dependency(&canonical, source.as_bytes())?;
            closure.insert(module_id);
            let module = Arc::new(ModuleExports {
                path: canonical.clone(),
                exports,
                closure,
            });
            let mut module_items = Vec::new();
            for item in expanded.items {
                if matches!(&item, Item::Module(module) if module.name.starts_with("std:")) {
                    module_items.push(item);
                } else {
                    let Some(name) = item_top_level_name(&item) else {
                        continue;
                    };
                    if matches!(&item, Item::Module(_)) {
                        continue;
                    }
                    if self.materialized_names.insert(name) {
                        module_items.push(item);
                    }
                }
            }
            if self.materialized_modules.len() < module_id {
                self.materialized_modules.resize_with(module_id, Vec::new);
                self.materialized_materials
                    .resize(module_id, ExpandedModuleMaterial::new(0));
            }
            self.materialized_modules[module_id - 1] = module_items;
            self.materialized_materials[module_id - 1] = material;
            self.materialized_order.push(module_id);
            self.states.insert(canonical.clone(), LoadState::Done);
            self.modules.insert(canonical.clone(), Arc::clone(&module));
            self.dependency_snapshots.push(dependency_snapshot);
            Ok(module)
        })();
        if result.is_err() {
            self.states.remove(&canonical);
        }
        self.budget.finish_module();
        result
    }

    fn expand_program(
        &mut self,
        path: &Path,
        program: Program,
        is_entry: bool,
        source_bytes: usize,
    ) -> KuResult<(
        Program,
        ExpandedModuleMaterial,
        BTreeMap<String, String>,
        ModuleClosure,
    )> {
        let mut items = Vec::new();
        let mut material = ExpandedModuleMaterial::new(source_bytes);
        let mut namespace_maps = HashMap::new();
        let local_names = top_level_names(&program);
        let mut imported_names = HashSet::new();
        let own_renames = if is_entry {
            HashMap::new()
        } else {
            let module_id = self.module_ids.get(path).copied().ok_or_else(|| {
                import_limit_error(
                    "module_identity",
                    format!("module identity was not interned for {}", path.display()),
                    Span::default(),
                )
            })?;
            program
                .items
                .iter()
                .filter_map(item_export_name)
                .map(|name| {
                    let canonical = format!("__ku_import{module_id}_{name}");
                    (name, canonical)
                })
                .collect()
        };
        let mut reference_renames = own_renames.clone();
        let mut exports = BTreeMap::new();
        let mut closure = ModuleClosure::new();
        for item in &program.items {
            let Some(name) = item_export_name(item) else {
                continue;
            };
            if is_exported_name(&name) {
                exports.insert(
                    name.clone(),
                    own_renames.get(&name).cloned().unwrap_or(name),
                );
            }
        }

        for item in &program.items {
            let Item::Import(import) = item else {
                continue;
            };
            self.budget.charge_import_edge(import.span)?;
            if let Some(modules) = std_import_modules(import)? {
                self.budget
                    .charge_import_bindings(modules.len(), import.span)?;
                for module in modules {
                    if local_names.contains(&module) || !imported_names.insert(module.clone()) {
                        return Err(KuError::runtime(
                            format!(
                                "import namespace '{module}' conflicts with another top-level name"
                            ),
                            import.span,
                        ));
                    }
                    items.push(Item::Module(ModuleDecl {
                        name: format!("std:{module}"),
                        span: import.span,
                    }));
                    material.add_item();
                }
                continue;
            }
            let import_path = resolve_import_path(
                path,
                &import.path,
                import.span,
                self.package.as_ref(),
                &self.package_import_scopes,
            )?;
            let module = self.load_module(&import_path, import.span)?;
            closure.union_with(&module.closure);
            match &import.kind {
                ImportKind::Named(names) => {
                    self.budget
                        .charge_import_bindings(names.len(), import.span)?;
                    let mut seen_sources = HashSet::new();
                    let mut seen_locals = HashSet::new();
                    for name in names {
                        if !seen_sources.insert(name.source.clone()) {
                            return Err(KuError::runtime(
                                format!("duplicate import name '{}'", name.source),
                                name.span,
                            ));
                        }
                        let local = name.local_name().to_string();
                        if !seen_locals.insert(local.clone()) {
                            return Err(KuError::runtime(
                                format!("duplicate import alias '{local}'"),
                                name.span,
                            ));
                        }
                        if local_names.contains(&local) || !imported_names.insert(local.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{local}' conflicts with another top-level name"
                                ),
                                name.span,
                            ));
                        }
                        let canonical =
                            module.exports.get(&name.source).cloned().ok_or_else(|| {
                                KuError::runtime(
                                    format!(
                                        "'{}' is not exported by {}",
                                        name.source,
                                        module.path.display()
                                    ),
                                    name.span,
                                )
                            })?;
                        reference_renames.insert(local.clone(), canonical.clone());
                    }
                }
                ImportKind::Glob => {
                    self.budget
                        .charge_import_bindings(module.exports.len(), import.span)?;
                    for (name, canonical) in &module.exports {
                        if local_names.contains(name) || !imported_names.insert(name.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{name}' conflicts with another top-level name"
                                ),
                                import.span,
                            ));
                        }
                        reference_renames.insert(name.clone(), canonical.clone());
                    }
                }
                ImportKind::Namespace(namespace) => {
                    if local_names.contains(namespace) || !imported_names.insert(namespace.clone())
                    {
                        return Err(KuError::runtime(
                            format!("import namespace '{namespace}' conflicts with another top-level name"),
                            import.span,
                        ));
                    }
                    self.budget
                        .charge_import_bindings(module.exports.len(), import.span)?;
                    let mut map = BTreeMap::new();
                    for (name, canonical) in &module.exports {
                        map.insert(name.clone(), canonical.clone());
                    }
                    namespace_maps.insert(namespace.clone(), map);
                }
            }
        }

        for item in program.items {
            if matches!(item, Item::Import(_)) {
                continue;
            }
            let span = match &item {
                Item::Import(decl) => decl.span,
                Item::Function(decl) => decl.span,
                Item::Struct(decl) => decl.span,
                Item::Enum(decl) => decl.span,
                Item::Module(decl) => decl.span,
            };
            self.budget.charge_item(span)?;
            let item = rewrite_top_level_names_in_item(item, &reference_renames, &namespace_maps)?;
            // Do not deduplicate source declarations: duplicate declarations in
            // one file must still reach the checker and produce its diagnostic.
            items.push(item);
            material.add_item();
        }
        debug_assert_eq!(items.len(), material.items);
        Ok((Program { items }, material, exports, closure))
    }
}

fn read_import_source(path: &Path, span: Span) -> KuResult<String> {
    let file = fs::File::open(path).map_err(|err| {
        KuError::runtime(
            format!("failed to read import '{}': {err}", path.display()),
            span,
        )
    })?;
    let mut source = String::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_string(&mut source)
        .map_err(|err| {
            KuError::runtime(
                format!("failed to read import '{}': {err}", path.display()),
                span,
            )
        })?;
    if source.len() as u64 > MAX_SOURCE_BYTES {
        return Err(KuError::runtime(
            format!(
                "source file too large: {} bytes exceeds {} bytes",
                source.len(),
                MAX_SOURCE_BYTES
            ),
            span,
        ));
    }
    Ok(source)
}

fn check_library_program(program: &Program) -> KuResult<()> {
    let mut program = program.clone();
    if !program
        .items
        .iter()
        .any(|item| matches!(item, Item::Function(function) if function.name == "main"))
    {
        program.items.push(Item::Function(FnDecl {
            name: "main".to_string(),
            is_async: false,
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: None,
            body: Vec::new(),
            span: Span::default(),
        }));
    }
    Checker::new().check(&program)
}

fn std_import_modules(import: &ImportDecl) -> KuResult<Option<Vec<String>>> {
    if import.path == "std" {
        let ImportKind::Named(names) = &import.kind else {
            return Err(KuError::runtime(
                "std root imports must use named form, for example import { fs, http } from \"std\"",
                import.span,
            ));
        };
        let mut modules = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if name.alias.is_some() {
                return Err(KuError::runtime(
                    "std root imports do not support aliases yet",
                    name.span,
                ));
            }
            if !stdlib::metadata::is_std_module(&name.source) {
                return Err(KuError::runtime(
                    format!("unknown std module '{}'", name.source),
                    name.span,
                ));
            }
            if !seen.insert(name.source.clone()) {
                return Err(KuError::runtime(
                    format!("duplicate std module import '{}'", name.source),
                    name.span,
                ));
            }
            modules.push(name.source.clone());
        }
        return Ok(Some(modules));
    }
    let module = if let Some(module) = import.path.strip_prefix("std.") {
        module
    } else {
        return Ok(None);
    };
    if !stdlib::metadata::is_std_module(module) {
        return Err(KuError::runtime(
            format!("unknown std module '{}'", import.path),
            import.span,
        ));
    }
    match &import.kind {
        ImportKind::Namespace(namespace) if namespace == module => Ok(Some(vec![module.to_string()])),
        ImportKind::Namespace(_) => Err(KuError::runtime(
            format!(
                "std module '{}' must be imported as '{}'",
                import.path, module
            ),
            import.span,
        )),
        ImportKind::Glob => Ok(Some(vec![module.to_string()])),
        ImportKind::Named(_) => Err(KuError::runtime(
            "std module imports must use namespace form, for example import http from \"std.http\", or shorthand import \"std.http\"",
            import.span,
        )),
    }
}

fn reject_large_file(path: &Path, span: Span) -> KuResult<()> {
    let metadata = fs::metadata(path).map_err(|err| {
        KuError::runtime(format!("failed to read '{}': {err}", path.display()), span)
    })?;
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(KuError::runtime(
            format!(
                "source file too large: {} bytes exceeds {} bytes",
                metadata.len(),
                MAX_SOURCE_BYTES
            ),
            span,
        ));
    }
    Ok(())
}

fn canonical_file(path: &Path, span: Span) -> KuResult<PathBuf> {
    fs::canonicalize(path).map_err(|err| {
        KuError::runtime(
            format!("failed to resolve '{}': {err}", path.display()),
            span,
        )
    })
}

fn resolve_import_path(
    current_file: &Path,
    import_path: &str,
    span: Span,
    package: Option<&PackageContext>,
    package_import_scopes: &[package::PackageImportScope],
) -> KuResult<PathBuf> {
    if package.is_some() {
        package::validate_package_import_text(import_path, span)?;
    }
    let raw = Path::new(import_path);
    let current_scope = package
        .map(|_| package::package_import_scope_for_file(package_import_scopes, current_file, span))
        .transpose()?;
    if let Some(current_scope) = current_scope {
        if let Some(path) = package::resolve_dependency_import(
            package_import_scopes,
            current_scope,
            import_path,
            span,
        )? {
            return Ok(path);
        }
    }
    let base = if raw.is_absolute() {
        PathBuf::new()
    } else if let Some(current_scope) = current_scope {
        if import_path.starts_with("./") || import_path.starts_with("../") {
            current_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        } else {
            current_scope.import_root.clone()
        }
    } else {
        current_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    let mut path = base.join(raw);
    if path.extension().is_none() {
        path.set_extension("ku");
    }
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
    {
        return Err(KuError::runtime(
            "import path must point to a .ku file",
            span,
        ));
    }
    if let Some(scope) = current_scope {
        path = package::canonical_import_in_scope(&path, scope, span)?;
    }
    Ok(path)
}

fn top_level_names(program: &Program) -> HashSet<String> {
    program
        .items
        .iter()
        .filter_map(item_top_level_name)
        .collect()
}

fn item_top_level_name(item: &Item) -> Option<String> {
    match item {
        Item::Function(function) => Some(function.name.clone()),
        Item::Struct(decl) => Some(decl.name.clone()),
        Item::Enum(decl) => Some(decl.name.clone()),
        Item::Module(decl) => Some(decl.name.clone()),
        Item::Import(_) => None,
    }
}

fn item_export_name(item: &Item) -> Option<String> {
    match item {
        Item::Function(function) => Some(function.name.clone()),
        Item::Struct(decl) => Some(decl.name.clone()),
        Item::Enum(decl) => Some(decl.name.clone()),
        Item::Module(_) | Item::Import(_) => None,
    }
}

fn is_exported_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn push_materialized_item(
    items: &mut Vec<Item>,
    materialized_names: &mut HashSet<String>,
    item: Item,
) {
    // A source-level `module foo` is metadata for its own file and historically
    // was not copied into importers. Synthetic std modules are capabilities used
    // by imported function bodies, so they do belong to the dependency closure.
    if matches!(&item, Item::Module(module) if !module.name.starts_with("std:")) {
        return;
    }
    let Some(name) = item_top_level_name(&item) else {
        debug_assert!(matches!(item, Item::Import(_)));
        return;
    };
    if materialized_names.insert(name) {
        items.push(item);
    }
}

type NamespaceMaps = HashMap<String, BTreeMap<String, String>>;

fn rewrite_top_level_names_in_item(
    item: Item,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) -> KuResult<Item> {
    match item {
        Item::Function(mut function) => {
            if let Some(renamed) = rename_map.get(&function.name) {
                function.name = renamed.clone();
            }
            rewrite_top_level_references_in_function(&mut function, rename_map, namespaces)?;
            Ok(Item::Function(function))
        }
        Item::Struct(mut decl) => {
            if let Some(renamed) = rename_map.get(&decl.name) {
                decl.name = renamed.clone();
            }
            for field in &mut decl.fields {
                rewrite_required_type_name(&mut field.ty, rename_map, namespaces);
            }
            Ok(Item::Struct(decl))
        }
        Item::Enum(mut decl) => {
            if let Some(renamed) = rename_map.get(&decl.name) {
                decl.name = renamed.clone();
            }
            for variant in &mut decl.variants {
                for field in &mut variant.fields {
                    rewrite_required_type_name(&mut field.ty, rename_map, namespaces);
                }
            }
            Ok(Item::Enum(decl))
        }
        Item::Module(_) | Item::Import(_) => Ok(item),
    }
}

fn rewrite_type_names_in_function(
    function: &mut FnDecl,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    for param in &mut function.params {
        rewrite_optional_type_name(&mut param.ty, rename_map, namespaces);
    }
    if let Some(return_type) = &mut function.return_type {
        rewrite_type_name(return_type, rename_map, namespaces);
    }
}

fn rewrite_optional_type_name(
    ty: &mut Option<TypeName>,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    if let Some(ty) = ty {
        rewrite_type_name(ty, rename_map, namespaces);
    }
}

fn rewrite_required_type_name(
    ty: &mut Option<TypeName>,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    rewrite_optional_type_name(ty, rename_map, namespaces);
}

fn rewrite_type_name(
    ty: &mut TypeName,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    match ty {
        TypeName::Array(inner) | TypeName::Result(inner) => {
            rewrite_type_name(inner, rename_map, namespaces)
        }
        TypeName::Function {
            params,
            return_type,
            ..
        } => {
            for param in params {
                rewrite_type_name(param, rename_map, namespaces);
            }
            rewrite_type_name(return_type, rename_map, namespaces);
        }
        TypeName::Union(types) => {
            for ty in types {
                rewrite_type_name(ty, rename_map, namespaces);
            }
        }
        TypeName::Custom(name) => {
            if let Some(renamed) = rename_map.get(name) {
                *name = renamed.clone();
            } else if let Some(renamed) = namespace_lookup(name, namespaces) {
                *name = renamed;
            }
        }
        TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::String | TypeName::Null => {}
    }
}

fn rewrite_top_level_references_in_function(
    function: &mut FnDecl,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) -> KuResult<()> {
    rewrite_type_names_in_function(function, rename_map, namespaces);
    let mut rewriter = TopLevelReferenceRewriter::new(rename_map, namespaces);
    rewriter.push_scope();
    for param in &function.params {
        rewriter.define(&param.name);
    }
    let result = rewriter.rewrite_block(&mut function.body);
    rewriter.pop_scope();
    result
}

struct TopLevelReferenceRewriter<'a> {
    rename_map: &'a HashMap<String, String>,
    namespaces: &'a NamespaceMaps,
    scopes: Vec<HashSet<String>>,
}

impl<'a> TopLevelReferenceRewriter<'a> {
    fn new(rename_map: &'a HashMap<String, String>, namespaces: &'a NamespaceMaps) -> Self {
        Self {
            rename_map,
            namespaces,
            scopes: Vec::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashSet::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string());
        }
    }

    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|scope| scope.contains(name))
    }

    fn namespace_symbol(
        &self,
        namespace: &str,
        name: &str,
        kind: &str,
        span: Span,
    ) -> KuResult<Option<String>> {
        if self.is_local(namespace) {
            return Ok(None);
        }
        let Some(exports) = self.namespaces.get(namespace) else {
            return Ok(None);
        };
        exports.get(name).cloned().map(Some).ok_or_else(|| {
            KuError::runtime(
                format!("module '{namespace}' has no exported {kind} '{name}'"),
                span,
            )
        })
    }

    fn rewrite_block(&mut self, body: &mut [Stmt]) -> KuResult<()> {
        for stmt in body {
            self.rewrite_stmt(stmt)?;
        }
        Ok(())
    }

    fn rewrite_scoped_block(&mut self, body: &mut [Stmt]) -> KuResult<()> {
        self.push_scope();
        let result = self.rewrite_block(body);
        self.pop_scope();
        result
    }

    fn rewrite_stmt(&mut self, stmt: &mut Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name, ty, value, ..
            } => {
                if let Some(ty) = ty {
                    rewrite_type_name(ty, self.rename_map, self.namespaces);
                }
                self.rewrite_expr(value)?;
                self.define(name);
            }
            Stmt::Assign { name, value, .. } => {
                self.rewrite_expr(value)?;
                if !self.is_local(name) {
                    self.define(name);
                }
            }
            Stmt::AssignTarget { target, value, .. }
            | Stmt::CompoundAssign { target, value, .. } => {
                self.rewrite_assign_target(target)?;
                self.rewrite_expr(value)?;
            }
            Stmt::DestructureAssign { names, values, .. } => {
                for value in values {
                    self.rewrite_expr(value)?;
                }
                for name in names.iter().flatten() {
                    self.define(name);
                }
            }
            Stmt::ObjectDestructureAssign {
                bindings,
                rest,
                value,
                ..
            } => {
                self.rewrite_expr(value)?;
                for binding in bindings {
                    if let Some(default) = &mut binding.default {
                        self.rewrite_expr(default)?;
                    }
                    let local = binding.local.as_deref().unwrap_or(&binding.field);
                    self.define(local);
                }
                if let Some(local) = rest.as_ref().and_then(|rest| rest.local.as_deref()) {
                    self.define(local);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.rewrite_expr(condition)?;
                self.rewrite_scoped_block(then_branch)?;
                if !else_branch.is_empty() {
                    self.rewrite_scoped_block(else_branch)?;
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.rewrite_expr(condition)?;
                self.rewrite_scoped_block(body)?;
            }
            Stmt::For {
                name,
                iterable,
                body,
                ..
            } => {
                self.rewrite_expr(iterable)?;
                self.push_scope();
                self.define(name);
                let result = self.rewrite_block(body);
                self.pop_scope();
                result?;
            }
            Stmt::Function(function) => {
                let local_name = function.name.clone();
                self.define(&local_name);
                rewrite_type_names_in_function(function, self.rename_map, self.namespaces);
                self.push_scope();
                self.define(&local_name);
                for param in &function.params {
                    self.define(&param.name);
                }
                let result = self.rewrite_block(&mut function.body);
                self.pop_scope();
                result?;
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                ..
            } => {
                self.rewrite_scoped_block(body)?;
                if !catch_body.is_empty() {
                    self.push_scope();
                    if let Some(catch_name) = catch_name.as_deref() {
                        self.define(catch_name);
                    }
                    let result = self.rewrite_block(catch_body);
                    self.pop_scope();
                    result?;
                }
                if !finally_body.is_empty() {
                    self.rewrite_scoped_block(finally_body)?;
                }
            }
            Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
                self.rewrite_expr(value)?
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.rewrite_expr(value)?;
                }
            }
            Stmt::Expr { expr, .. } => self.rewrite_expr(expr)?,
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
        Ok(())
    }

    fn rewrite_assign_target(&mut self, target: &mut AssignTarget) -> KuResult<()> {
        match target {
            AssignTarget::Variable(_) => Ok(()),
            AssignTarget::Index { target, index } => {
                self.rewrite_expr(target)?;
                self.rewrite_expr(index)
            }
            AssignTarget::Field { target, .. } => self.rewrite_expr(target),
        }
    }

    fn rewrite_expr(&mut self, expr: &mut Expr) -> KuResult<()> {
        match &mut expr.kind {
            ExprKind::Variable(name) => {
                if !self.is_local(name) {
                    if let Some(renamed) = self.rename_map.get(name).cloned() {
                        *name = renamed;
                    }
                }
            }
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
                self.rewrite_expr(expr)?
            }
            ExprKind::Binary { left, right, .. } => {
                self.rewrite_expr(left)?;
                self.rewrite_expr(right)?;
            }
            ExprKind::Call { callee, args } => {
                self.rewrite_expr(callee)?;
                for arg in args {
                    self.rewrite_expr(arg)?;
                }
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.rewrite_expr(value)?;
                }
            }
            ExprKind::Index { target, index } => {
                self.rewrite_expr(target)?;
                self.rewrite_expr(index)?;
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Field {
                    target: enum_target,
                    name: enum_name,
                } = &mut target.kind
                {
                    if let ExprKind::Variable(namespace) = &enum_target.kind {
                        if let Some(renamed) =
                            self.namespace_symbol(namespace, enum_name, "type", target.span)?
                        {
                            target.kind = ExprKind::Variable(renamed);
                        }
                    }
                }
                let replacement = if let ExprKind::Variable(namespace) = &target.kind {
                    self.namespace_symbol(namespace, name, "symbol", expr.span)?
                } else {
                    None
                };
                if let Some(renamed) = replacement {
                    expr.kind = ExprKind::Variable(renamed);
                } else {
                    self.rewrite_expr(target)?;
                }
            }
            ExprKind::OptionalField { target, .. } => {
                self.rewrite_expr(target)?;
            }
            ExprKind::StructLiteral { name, fields } => {
                if let Some(renamed) = self.rename_map.get(name).cloned() {
                    *name = renamed;
                } else if let Some(renamed) = namespace_lookup(name, self.namespaces) {
                    *name = renamed;
                }
                for (_, value) in fields {
                    self.rewrite_expr(value)?;
                }
            }
            ExprKind::ObjectLiteral { fields } => {
                for (_, value) in fields {
                    self.rewrite_expr(value)?;
                }
            }
            ExprKind::Match { value, arms } => {
                self.rewrite_expr(value)?;
                for arm in arms {
                    self.push_scope();
                    self.rewrite_match_pattern(&mut arm.pattern);
                    let result = (|| {
                        if let Some(guard) = &mut arm.guard {
                            self.rewrite_expr(guard)?;
                        }
                        self.rewrite_expr(&mut arm.value)
                    })();
                    self.pop_scope();
                    result?;
                }
            }
            ExprKind::Function {
                params,
                return_type,
                body,
            } => {
                for param in params.iter_mut() {
                    if let Some(ty) = &mut param.ty {
                        rewrite_type_name(ty, self.rename_map, self.namespaces);
                    }
                }
                if let Some(return_type) = return_type {
                    rewrite_type_name(return_type, self.rename_map, self.namespaces);
                }
                self.push_scope();
                for param in params.iter() {
                    self.define(&param.name);
                }
                let result = self.rewrite_block(body);
                self.pop_scope();
                result?;
            }
            ExprKind::Literal(_) => {}
        }
        Ok(())
    }

    fn rewrite_match_pattern(&mut self, pattern: &mut MatchPattern) {
        match pattern {
            MatchPattern::Binding(name) => self.define(name),
            MatchPattern::EnumVariant {
                enum_name, fields, ..
            } => {
                if let Some(renamed) = self.rename_map.get(enum_name).cloned() {
                    *enum_name = renamed;
                } else if let Some(renamed) = namespace_lookup(enum_name, self.namespaces) {
                    *enum_name = renamed;
                }
                for field in fields {
                    self.rewrite_match_pattern(field);
                }
            }
            MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
        }
    }
}

fn namespace_lookup(path: &str, namespaces: &NamespaceMaps) -> Option<String> {
    let (namespace, name) = path.split_once('.')?;
    namespaces.get(namespace)?.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_build_process_child_fixture() {
        #[cfg(windows)]
        if let Some(started) = env::var_os("KU_TEST_BUILD_GRANDCHILD_STARTED") {
            let escaped = env::var_os("KU_TEST_BUILD_GRANDCHILD_ESCAPED")
                .expect("grandchild escaped sentinel path");
            fs::write(&started, b"started").expect("write grandchild started sentinel");
            thread::sleep(Duration::from_millis(750));
            fs::write(&escaped, b"escaped").expect("write grandchild escaped sentinel");
            return;
        }
        #[cfg(windows)]
        if env::var_os("KU_TEST_SPAWN_BUILD_GRANDCHILD").is_some() {
            let started = env::var_os("KU_TEST_BUILD_GRANDCHILD_STARTED_PATH")
                .expect("started sentinel path");
            let escaped = env::var_os("KU_TEST_BUILD_GRANDCHILD_ESCAPED_PATH")
                .expect("escaped sentinel path");
            let mut grandchild = Command::new(env::current_exe().expect("resolve test executable"));
            grandchild
                .args([
                    "--exact",
                    "cli::tests::bounded_build_process_child_fixture",
                    "--nocapture",
                ])
                .env_remove("KU_TEST_SPAWN_BUILD_GRANDCHILD")
                .env("KU_TEST_BUILD_GRANDCHILD_STARTED", &started)
                .env("KU_TEST_BUILD_GRANDCHILD_ESCAPED", &escaped)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let grandchild_process = grandchild
                .spawn()
                .expect("spawn immediate build grandchild");
            let deadline = Instant::now() + Duration::from_secs(2);
            while !Path::new(&started).exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                Path::new(&started).is_file(),
                "grandchild did not start within its bounded fixture window"
            );
            // Closing the direct handle without waiting is intentional: the
            // outer Windows Job must own and terminate this still-live child.
            drop(grandchild_process);
            return;
        }
        if env::var_os("KU_TEST_HANG_BUILD_CHILD").is_some() {
            thread::sleep(Duration::from_secs(30));
        }
        if env::var_os("KU_TEST_FLOOD_BUILD_CHILD").is_some() {
            let chunk = [b'x'; 8 * 1024];
            let mut stdout = io::stdout().lock();
            for _ in 0..1024 {
                if stdout.write_all(&chunk).is_err() {
                    break;
                }
            }
        }
    }

    #[test]
    fn build_process_deadline_terminates_a_hung_child() {
        let mut command = Command::new(env::current_exe().expect("resolve unit test executable"));
        command
            .args([
                "--exact",
                "cli::tests::bounded_build_process_child_fixture",
                "--nocapture",
            ])
            .env("KU_TEST_HANG_BUILD_CHILD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        let error = run_build_process_bounded(&mut command, Duration::from_millis(150))
            .expect_err("hung build child must hit its absolute deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout cleanup exceeded its bounded grace"
        );
    }

    #[test]
    fn build_process_capture_enforces_its_memory_limit() {
        let mut command = Command::new(env::current_exe().expect("resolve unit test executable"));
        command
            .args([
                "--exact",
                "cli::tests::bounded_build_process_child_fixture",
                "--nocapture",
            ])
            .env("KU_TEST_FLOOD_BUILD_CHILD", "1");
        let started = Instant::now();
        let error = run_build_process_capture_stdout(&mut command, Duration::from_secs(5), 1024)
            .expect_err("flooding build child must hit the output cap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "output-limit cleanup exceeded its deadline"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_build_child_cannot_spawn_before_job_assignment_or_escape_after_exit() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-build-job-race-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).expect("create build Job fixture");
        let started = dir.join("grandchild-started");
        let escaped = dir.join("grandchild-escaped");
        let mut command = Command::new(env::current_exe().expect("resolve unit test executable"));
        command
            .args([
                "--exact",
                "cli::tests::bounded_build_process_child_fixture",
                "--nocapture",
            ])
            .env("KU_TEST_DELAY_BUILD_JOB_ATTACH", "1")
            .env("KU_TEST_SPAWN_BUILD_GRANDCHILD", "1")
            .env("KU_TEST_BUILD_GRANDCHILD_STARTED_PATH", &started)
            .env("KU_TEST_BUILD_GRANDCHILD_ESCAPED_PATH", &escaped)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = run_build_process_bounded(&mut command, Duration::from_secs(3))
            .expect("Job-contained child fixture must complete");
        assert!(status.success());
        assert!(started.is_file(), "contained grandchild must have started");
        thread::sleep(Duration::from_millis(900));
        assert!(
            !escaped.exists(),
            "grandchild escaped the build Job during the spawn-to-assign window"
        );
        fs::remove_dir_all(dir).expect("remove build Job fixture");
    }

    #[cfg(windows)]
    #[test]
    fn vcvars_probe_handles_unicode_paths_and_every_nonzero_exit() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-vcvars-工具链-{}-{nonce}", std::process::id()));
        let guard = TempBuildDir::new(dir.clone());
        fs::create_dir(&dir).expect("create Unicode vcvars fixture directory");

        let success = dir.join("vcvars-成功.bat");
        fs::write(
            &success,
            b"@echo off\r\nset KU_TEST_VCVARS_UNICODE_PATH=ok\r\nexit /b 0\r\n",
        )
        .expect("write successful vcvars fixture");
        let deadline = Instant::now() + BUILD_TOOL_PROBE_TIMEOUT;
        let vars = load_vcvars_env(&success, deadline).expect("load vcvars from a Unicode path");
        assert!(vars.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("KU_TEST_VCVARS_UNICODE_PATH") && value == "ok"
        }));

        let failure = dir.join("vcvars-失败.bat");
        fs::write(&failure, b"@echo off\r\nexit /b -1\r\n").expect("write failing vcvars fixture");
        let error = load_vcvars_env(&failure, Instant::now() + BUILD_TOOL_PROBE_TIMEOUT)
            .expect_err("a negative vcvars exit code must fail closed")
            .to_string();
        assert!(error.contains("failed to initialize"));
        guard.cleanup();
    }

    #[test]
    fn package_yank_has_one_cli_shape_and_bounded_arguments() {
        assert!(HELP.contains("ku package yank [path]"));
        assert!(!HELP.contains("ku yank "));
        let receipt = package::PackageYankReceipt {
            name: "math".to_string(),
            version: "1.2.3".to_string(),
            registry: "https://registry.example/v1/".to_string(),
        };
        assert_eq!(
            package_yank_success_message(&receipt),
            "package yank ok: math@1.2.3 https://registry.example/v1/"
        );

        let error = run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "yank".to_string(),
            ".".to_string(),
            "unexpected".to_string(),
        ])
        .expect_err("yank accepts at most one package path");
        assert!(error
            .to_string()
            .contains("too many arguments for 'ku package yank'"));
    }

    #[test]
    fn dependency_mode_flags_are_single_and_propagate_to_built_runner() {
        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "src/main.ku".to_string(),
            "--offline".to_string(),
        ];
        let options = parse_build_options(&args).expect("parse offline build");
        assert_eq!(options.dependency_mode, DependencyResolveMode::Offline);

        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "--native".to_string(),
            "--locked".to_string(),
            "src/main.ku".to_string(),
        ];
        let (path, mode) = parse_native_compat_args(&args).expect("parse locked native build");
        assert_eq!(path, PathBuf::from("src/main.ku"));
        assert_eq!(mode, DependencyResolveMode::Locked);

        let runner = build_runner_source("src/main.ku", "fn main() {}", mode);
        assert!(runner.contains("run_source_with_dependency_mode"));
        assert!(runner.contains("DependencyResolveMode::Locked"));

        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "--locked".to_string(),
            "--offline".to_string(),
            "src/main.ku".to_string(),
        ];
        let error = parse_build_options(&args).expect_err("conflicting modes must fail");
        assert!(error
            .to_string()
            .contains("only one of --locked or --offline"));
    }

    #[test]
    fn imported_source_snapshot_prevents_ast_lock_hash_mismatch() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-import-lock-snapshot-{}-{nonce}",
            std::process::id()
        ));
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("create import lock fixture");
        fs::write(
            dir.join(package::MANIFEST_FILE),
            b"name = \"app\"\nversion = \"0.1.0\"\nroot = \"src\"\n",
        )
        .expect("write import lock manifest");
        let dependency = src.join("value.ku");
        let original_dependency = b"fn Value(): int { return 1 }\n";
        fs::write(&dependency, original_dependency).expect("write original dependency");
        let main = src.join("main.ku");
        let main_source = "import { Value } from \"./value\"\nfn main() { println(Value()) }\n";
        fs::write(&main, main_source).expect("write import lock entry");

        let package = package::discover_from_dir(&dir)
            .expect("discover import lock package")
            .expect("import lock package exists");
        let program = parse_source(main_source).expect("parse import lock entry");
        let mut loader = ModuleLoader::new(Some(package)).expect("create module loader");
        loader
            .load_entry(&main, program, main_source.len())
            .expect("load dependency from original source bytes");
        assert_eq!(loader.dependency_snapshots.len(), 1);
        let frozen = loader.dependency_snapshots.clone();
        let expected = package::freeze_lock_dependency(&dependency, original_dependency)
            .expect("hash original parsed source");
        assert_eq!(frozen[0].cache_key, expected.cache_key);
        let package = loader.package.as_ref().expect("loader package context");
        package::write_lock_with_frozen_dependencies(package, &frozen)
            .expect("unchanged imported source writes the frozen hash");
        let original_lock = fs::read_to_string(&package.lock_path).expect("read frozen lock");
        assert!(original_lock.contains(&expected.cache_key));

        fs::write(&dependency, b"fn Value(): int { return 2 }\n")
            .expect("replace dependency after parsing");
        let err = package::write_lock_with_frozen_dependencies(package, &frozen)
            .expect_err("changed imported source must not update ku.lock");
        assert_eq!(err.code.as_deref(), Some("source_changed"));
        assert_eq!(
            fs::read_to_string(&package.lock_path).expect("read unchanged frozen lock"),
            original_lock
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_module_load_clears_transient_state_before_retry() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-import-retry-state-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create retry-state fixture");
        let dependency = dir.join("value.ku");
        fs::write(&dependency, "fn Value(): int { return \"bad\" }\n")
            .expect("write invalid dependency");
        let canonical = canonical_file(&dependency, Span::default()).expect("canonical dependency");
        let mut loader = ModuleLoader::new(None).expect("create module loader");

        let first_error = loader
            .load_module(&dependency, Span::default())
            .err()
            .expect("invalid dependency must fail checking");
        assert!(
            first_error.to_string().contains("type error"),
            "unexpected first load error: {first_error}"
        );
        assert!(!loader.states.contains_key(&canonical));
        assert!(!loader.modules.contains_key(&canonical));
        assert!(loader.materialized_order.is_empty());
        assert!(loader.dependency_snapshots.is_empty());

        fs::write(&dependency, "fn Value(): int { return 1 }\n").expect("repair dependency");
        let module = loader
            .load_module(&dependency, Span::default())
            .expect("a repaired dependency must not look circular");
        assert_eq!(
            module.exports.get("Value").map(String::as_str),
            Some("__ku_import1_Value")
        );
        assert_eq!(loader.materialized_order, vec![1]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_binding_budget_is_structured_and_hard_bounded() {
        let mut budget = ImportBudget::default();
        let error = budget
            .charge_import_bindings(MAX_IMPORT_BINDINGS + 1, Span::default())
            .expect_err("an oversized binding map must fail before allocation");
        assert_eq!(error.domain.as_deref(), Some("import"));
        assert_eq!(error.code.as_deref(), Some("binding_limit"));
        assert_eq!(budget.expanded_items, 0);
    }

    #[test]
    fn native_fs_locator_is_relative_to_executable_directory() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\app")
        } else {
            PathBuf::from("/workspace/app")
        };
        assert_eq!(
            executable_relative_locator(&root.join("bin"), &root.join("source"))
                .expect("sibling locator"),
            "../source"
        );
        assert_eq!(
            executable_relative_locator(&root.join("source"), &root.join("source"))
                .expect("same-directory locator"),
            "."
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_fs_locator_rejects_different_windows_drives() {
        let error = executable_relative_locator(Path::new(r"C:\bin"), Path::new(r"D:\source"))
            .expect_err("different drives cannot be relocatable");
        assert!(error.contains("filesystem root"));
    }

    #[test]
    fn build_target_resolver_accepts_host_and_supported_targets() {
        assert!(resolve_build_target(None)
            .expect("default target")
            .is_none());
        assert!(resolve_build_target(Some("host"))
            .expect("host target")
            .is_none());

        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("resolved linux target");
        assert_eq!(linux.slug, "x86_64-linux");
        assert_eq!(linux.rust_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(linux.c_triple, "x86_64-linux-gnu");
        assert!(!linux.is_windows);

        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("resolved windows target");
        assert_eq!(windows.rust_triple, "x86_64-pc-windows-msvc");
        assert!(windows.is_windows);
        assert_eq!(
            with_executable_extension(PathBuf::from("app"), Some(&windows)),
            PathBuf::from("app.exe")
        );

        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("resolved darwin target");
        assert_eq!(darwin.rust_triple, "aarch64-apple-darwin");
    }

    #[test]
    fn build_target_resolver_rejects_path_escape_and_unknown_targets() {
        let err = resolve_build_target(Some("../escape")).expect_err("path target must fail");
        assert!(
            err.to_string().contains("invalid build target"),
            "unexpected error: {err}"
        );

        let err = resolve_build_target(Some("wasm32-wasi")).expect_err("unknown target must fail");
        assert!(
            err.to_string().contains("unsupported build target"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn three_native_targets_emit_separate_source_free_import_graphs_without_a_linker() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-three-target-c-{}-{nonce}", std::process::id()));
        let source_dir = dir.join("source");
        let retained_dir = dir.join("retained-c");
        fs::create_dir_all(&source_dir).expect("create three-target source fixture");
        fs::create_dir_all(&retained_dir).expect("create retained C fixture");
        fs::write(
            source_dir.join("math.ku"),
            "fn Add(a:int, b:int): int { return a + b }\n",
        )
        .expect("write imported target fixture");
        let main = source_dir.join("main.ku");
        fs::write(
            &main,
            "import { Add } from \"./math.ku\"\nfn main(): null! { println(Add(20, 22)) return ok(null) }\n",
        )
        .expect("write target entry fixture");

        let mut retained = Vec::new();
        for target_name in ["x86_64-windows", "x86_64-linux", "aarch64-darwin"] {
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                "--target".to_string(),
                target_name.to_string(),
                main.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse target build");
            let plan = resolve_build_plan(&options).expect("resolve target build plan");
            fs::create_dir_all(&plan.build_dir).expect("create target build directory");
            assert_eq!(
                plan.build_dir,
                fs::canonicalize(&source_dir)
                    .expect("canonical target source fixture")
                    .join(package::DEFAULT_BUILD_DIR)
                    .join(target_name)
                    .join("debug")
            );
            assert_eq!(
                plan.output.extension().and_then(|value| value.to_str()),
                (target_name == "x86_64-windows").then_some("exe")
            );
            let c_path = write_native_c_artifact(&plan, DependencyResolveMode::Update)
                .expect("emit target-specific C without a linker");
            assert_eq!(c_path, plan.build_dir.join("c").join("main.c"));
            let c = fs::read_to_string(&c_path).expect("read target-specific C");
            assert!(
                c.lines().any(|line| {
                    line.starts_with("int64_t __ku_import")
                        && line.contains("_Add(int64_t a, int64_t b)")
                }),
                "import graph missing from {target_name} artifact"
            );
            assert!(!c.contains("run_source"));
            assert!(!c.contains("const SOURCE"));
            let retained_path = retained_dir.join(format!("{target_name}.c"));
            fs::copy(&c_path, &retained_path).expect("retain emitted C outside source tree");
            retained.push(retained_path);
        }

        fs::remove_dir_all(&source_dir).expect("remove complete Ku source tree");
        for artifact in retained {
            let c = fs::read_to_string(&artifact).expect("read retained source-free C");
            assert!(c.contains("KuResult_null ku_main()"));
            assert!(!c.contains("run_source"));
            assert!(!c.contains("const SOURCE"));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parallel_standalone_entries_keep_separate_native_c_artifacts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-parallel-native-entries-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create parallel native fixture");

        let make_plan = |name: &str, marker: &str| {
            let entry = dir.join(format!("{name}.ku"));
            fs::write(
                &entry,
                format!("fn main(): null! {{ println(\"{marker}\") return ok(null) }}\n"),
            )
            .expect("write standalone native entry");
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                entry.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse standalone native build");
            let plan = resolve_build_plan(&options).expect("resolve standalone native build");
            fs::create_dir_all(&plan.build_dir).expect("create standalone build directory");
            plan
        };

        let first = make_plan("first", "FIRST_NATIVE_ENTRY");
        let second = make_plan("second", "SECOND_NATIVE_ENTRY");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let first_handle = thread::spawn(move || {
            first_barrier.wait();
            write_native_c_artifact(&first, DependencyResolveMode::Update)
                .expect("emit first standalone native C")
        });
        let second_handle = thread::spawn(move || {
            second_barrier.wait();
            write_native_c_artifact(&second, DependencyResolveMode::Update)
                .expect("emit second standalone native C")
        });
        let first_c = first_handle.join().expect("first native build panicked");
        let second_c = second_handle.join().expect("second native build panicked");

        assert_ne!(first_c, second_c, "parallel entries must not share main.c");
        assert_eq!(
            first_c.file_name().and_then(|name| name.to_str()),
            Some("first.c")
        );
        assert_eq!(
            second_c.file_name().and_then(|name| name.to_str()),
            Some("second.c")
        );
        let first_source = fs::read_to_string(first_c).expect("read first native C");
        let second_source = fs::read_to_string(second_c).expect("read second native C");
        assert!(first_source.contains("FIRST_NATIVE_ENTRY"));
        assert!(!first_source.contains("SECOND_NATIVE_ENTRY"));
        assert!(second_source.contains("SECOND_NATIVE_ENTRY"));
        assert!(!second_source.contains("FIRST_NATIVE_ENTRY"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_file_locks_are_bounded_shared_and_released() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-build-lock-test-{}-{nonce}", std::process::id()));
        let path = dir.join("build.lock");

        let exclusive = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Exclusive,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("acquire initial exclusive build lock");
        let blocked_at = Instant::now();
        let blocked = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Shared,
            blocked_at + Duration::from_millis(40),
        );
        let error = match blocked {
            Ok(_) => panic!("an exclusive build lock must block another holder"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("build output remained busy"));
        assert!(
            blocked_at.elapsed() < Duration::from_secs(2),
            "lock contention must stop at its absolute deadline"
        );
        drop(exclusive);

        let first_shared = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Shared,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("acquire first shared build lock after release");
        let second_shared = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Shared,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("shared build locks may coexist");
        let blocked = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Exclusive,
            Instant::now() + Duration::from_millis(40),
        );
        assert!(
            blocked.is_err(),
            "a clean/exclusive lock must wait for ordinary builds"
        );
        drop(second_shared);
        drop(first_shared);

        let released = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Exclusive,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("exclusive build lock must become available after all holders drop");
        drop(released);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn separate_projects_targeting_one_output_share_the_output_lock() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-shared-native-output-lock-{}-{nonce}",
            std::process::id()
        ));
        let output = dir.join("dist").join("shared-program");

        let make_plan = |project: &str| {
            let project_dir = dir.join(project);
            fs::create_dir_all(&project_dir).expect("create standalone project");
            let entry = project_dir.join("main.ku");
            fs::write(&entry, "fn main() {}\n").expect("write standalone project entry");
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                "-o".to_string(),
                output.display().to_string(),
                entry.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse shared output build");
            resolve_build_plan(&options).expect("resolve shared output build")
        };

        let first = make_plan("first");
        let second = make_plan("second");
        assert_ne!(
            first.root_lock_path, second.root_lock_path,
            "independent build trees need independent clean leases"
        );
        assert_eq!(
            first.output_lock_path, second.output_lock_path,
            "the absolute final output identity must select one global lock"
        );
        assert_ne!(
            first.native_c_output, second.native_c_output,
            "each project retains its own generated C artifact"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn output_digest_uses_complete_compact_sha256_encoding() {
        assert_eq!(encode_base64url_no_pad(b""), "");
        assert_eq!(encode_base64url_no_pad(b"f"), "Zg");
        assert_eq!(encode_base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(encode_base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(encode_base64url_no_pad(&[0xff, 0xee, 0xdd]), "_-7d");

        let digest = native_output_path_digest(Path::new("dist/app"), Path::new("/workspace"));
        assert_eq!(digest.len(), 43, "all 256 digest bits must be retained");
        assert!(digest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
    }

    #[test]
    fn parallel_explicit_outputs_with_the_same_name_keep_separate_native_c_artifacts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-parallel-native-output-dirs-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create explicit-output native fixture");

        let make_plan = |name: &str, marker: &str| {
            let entry = dir.join(format!("{name}.ku"));
            fs::write(
                &entry,
                format!("fn main(): null! {{ println(\"{marker}\") return ok(null) }}\n"),
            )
            .expect("write explicit-output native entry");
            let output = dir.join(name).join("program");
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                "-o".to_string(),
                output.display().to_string(),
                entry.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse explicit-output build");
            let plan = resolve_build_plan(&options).expect("resolve explicit-output build");
            assert!(
                plan.native_c_output.starts_with(plan.build_dir.join("c")),
                "an explicit output C artifact must stay inside the isolated build tree"
            );
            plan
        };

        let first = make_plan("first", "FIRST_EXPLICIT_OUTPUT");
        let second = make_plan("second", "SECOND_EXPLICIT_OUTPUT");
        let first_expected = first.native_c_output.clone();
        let second_expected = second.native_c_output.clone();
        assert_ne!(first.ir_output, second.ir_output);
        assert_ne!(first.llvm_output, second.llvm_output);
        assert_eq!(
            first_expected
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(str::len),
            Some(43),
            "the explicit-output isolation directory keeps complete SHA-256 entropy"
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let first_handle = thread::spawn(move || {
            first_barrier.wait();
            write_native_c_artifact(&first, DependencyResolveMode::Update)
                .expect("emit first explicit-output native C")
        });
        let second_handle = thread::spawn(move || {
            second_barrier.wait();
            write_native_c_artifact(&second, DependencyResolveMode::Update)
                .expect("emit second explicit-output native C")
        });
        let first_c = first_handle.join().expect("first native build panicked");
        let second_c = second_handle.join().expect("second native build panicked");

        assert_eq!(first_c, first_expected);
        assert_eq!(second_c, second_expected);
        assert_ne!(first_c, second_c);
        assert_ne!(
            first_c.parent(),
            second_c.parent(),
            "the absolute output path must select an isolated artifact directory"
        );
        assert_eq!(
            first_c.file_name().and_then(|name| name.to_str()),
            Some("program.c")
        );
        assert_eq!(
            second_c.file_name().and_then(|name| name.to_str()),
            Some("program.c")
        );
        let first_source = fs::read_to_string(first_c).expect("read first explicit-output C");
        let second_source = fs::read_to_string(second_c).expect("read second explicit-output C");
        assert!(first_source.contains("FIRST_EXPLICIT_OUTPUT"));
        assert!(!first_source.contains("SECOND_EXPLICIT_OUTPUT"));
        assert!(second_source.contains("SECOND_EXPLICIT_OUTPUT"));
        assert!(!second_source.contains("FIRST_EXPLICIT_OUTPUT"));
        fs::remove_dir_all(&dir).ok();
    }

    fn dependency_elf(names: &[&str]) -> Vec<u8> {
        dependency_elf_with_soname(names, None)
    }

    fn dependency_elf_with_soname(names: &[&str], soname: Option<&str>) -> Vec<u8> {
        let mut elf = vec![0u8; 0x500];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[6] = 1;
        elf[7] = 3;
        elf[16..18].copy_from_slice(&3u16.to_le_bytes());
        elf[18..20].copy_from_slice(&62u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[24..32].copy_from_slice(&0x400080u64.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[52..54].copy_from_slice(&64u16.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&2u16.to_le_bytes());

        let file_len = elf.len() as u64;
        let load = 64usize;
        elf[load..load + 4].copy_from_slice(&1u32.to_le_bytes());
        elf[load + 4..load + 8].copy_from_slice(&5u32.to_le_bytes());
        elf[load + 16..load + 24].copy_from_slice(&0x400000u64.to_le_bytes());
        elf[load + 24..load + 32].copy_from_slice(&0x400000u64.to_le_bytes());
        elf[load + 32..load + 40].copy_from_slice(&file_len.to_le_bytes());
        elf[load + 40..load + 48].copy_from_slice(&file_len.to_le_bytes());
        elf[load + 48..load + 56].copy_from_slice(&0x1000u64.to_le_bytes());

        let mut strings = vec![0u8];
        let mut offsets = Vec::new();
        for name in names {
            offsets.push(strings.len() as u64);
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        let soname_offset = soname.map(|name| {
            let offset = strings.len() as u64;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            offset
        });
        let dynamic_size = ((names.len() + 3 + usize::from(soname.is_some())) * 16) as u64;
        let dynamic_header = load + 56;
        elf[dynamic_header..dynamic_header + 4].copy_from_slice(&2u32.to_le_bytes());
        elf[dynamic_header + 4..dynamic_header + 8].copy_from_slice(&4u32.to_le_bytes());
        elf[dynamic_header + 8..dynamic_header + 16].copy_from_slice(&0x200u64.to_le_bytes());
        elf[dynamic_header + 16..dynamic_header + 24].copy_from_slice(&0x400200u64.to_le_bytes());
        elf[dynamic_header + 24..dynamic_header + 32].copy_from_slice(&0x400200u64.to_le_bytes());
        elf[dynamic_header + 32..dynamic_header + 40].copy_from_slice(&dynamic_size.to_le_bytes());
        elf[dynamic_header + 40..dynamic_header + 48].copy_from_slice(&dynamic_size.to_le_bytes());
        elf[dynamic_header + 48..dynamic_header + 56].copy_from_slice(&8u64.to_le_bytes());

        let mut dynamic = 0x200usize;
        for offset in offsets {
            elf[dynamic..dynamic + 8].copy_from_slice(&1u64.to_le_bytes());
            elf[dynamic + 8..dynamic + 16].copy_from_slice(&offset.to_le_bytes());
            dynamic += 16;
        }
        if let Some(offset) = soname_offset {
            elf[dynamic..dynamic + 8].copy_from_slice(&14u64.to_le_bytes());
            elf[dynamic + 8..dynamic + 16].copy_from_slice(&offset.to_le_bytes());
            dynamic += 16;
        }
        elf[dynamic..dynamic + 8].copy_from_slice(&5u64.to_le_bytes());
        elf[dynamic + 8..dynamic + 16].copy_from_slice(&0x400300u64.to_le_bytes());
        dynamic += 16;
        elf[dynamic..dynamic + 8].copy_from_slice(&10u64.to_le_bytes());
        elf[dynamic + 8..dynamic + 16].copy_from_slice(&(strings.len() as u64).to_le_bytes());
        elf[0x300..0x300 + strings.len()].copy_from_slice(&strings);
        elf
    }

    fn dependency_pe(names: &[&str]) -> Vec<u8> {
        let mut pe = vec![0u8; 0x800];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        pe[0x96..0x98].copy_from_slice(&0x0022u16.to_le_bytes());
        pe[0x98..0x9a].copy_from_slice(&0x020bu16.to_le_bytes());
        pe[0xa8..0xac].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xb8..0xbc].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xbc..0xc0].copy_from_slice(&0x200u32.to_le_bytes());
        pe[0xd0..0xd4].copy_from_slice(&0x2000u32.to_le_bytes());
        pe[0xd4..0xd8].copy_from_slice(&0x200u32.to_le_bytes());
        pe[0x104..0x108].copy_from_slice(&16u32.to_le_bytes());
        pe[0x110..0x114].copy_from_slice(&0x1100u32.to_le_bytes());
        pe[0x114..0x118].copy_from_slice(&(((names.len() + 1) * 20) as u32).to_le_bytes());
        let section = 0x188usize;
        pe[section..section + 6].copy_from_slice(b".rdata");
        pe[section + 8..section + 12].copy_from_slice(&0x600u32.to_le_bytes());
        pe[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[section + 16..section + 20].copy_from_slice(&0x600u32.to_le_bytes());
        pe[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 36..section + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());

        let mut string_offset = 0x400usize;
        for (index, name) in names.iter().enumerate() {
            let descriptor = 0x300 + index * 20;
            let name_rva = 0x1000 + (string_offset - 0x200);
            pe[descriptor + 12..descriptor + 16].copy_from_slice(&(name_rva as u32).to_le_bytes());
            pe[string_offset..string_offset + name.len()].copy_from_slice(name.as_bytes());
            string_offset += name.len();
            pe[string_offset] = 0;
            string_offset += 1;
        }
        pe
    }

    fn dependency_macho(names: &[&str]) -> Vec<u8> {
        let mut commands = vec![0u8; 96];
        commands[..4].copy_from_slice(&0x19u32.to_le_bytes());
        commands[4..8].copy_from_slice(&72u32.to_le_bytes());
        commands[32..40].copy_from_slice(&0x1000u64.to_le_bytes());
        commands[60..64].copy_from_slice(&5u32.to_le_bytes());
        commands[72..76].copy_from_slice(&0x32u32.to_le_bytes());
        commands[76..80].copy_from_slice(&24u32.to_le_bytes());
        commands[80..84].copy_from_slice(&1u32.to_le_bytes());
        for name in names {
            let command_size = (24 + name.len() + 1).next_multiple_of(8);
            let start = commands.len();
            commands.resize(start + command_size, 0);
            commands[start..start + 4].copy_from_slice(&0x0cu32.to_le_bytes());
            commands[start + 4..start + 8].copy_from_slice(&(command_size as u32).to_le_bytes());
            commands[start + 8..start + 12].copy_from_slice(&24u32.to_le_bytes());
            commands[start + 24..start + 24 + name.len()].copy_from_slice(name.as_bytes());
        }
        let mut macho = vec![0u8; 32 + commands.len()];
        macho[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        macho[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        macho[12..16].copy_from_slice(&2u32.to_le_bytes());
        macho[16..20].copy_from_slice(&((2 + names.len()) as u32).to_le_bytes());
        macho[20..24].copy_from_slice(&(commands.len() as u32).to_le_bytes());
        let file_len = macho.len() as u64;
        commands[48..56].copy_from_slice(&file_len.to_le_bytes());
        macho[32..].copy_from_slice(&commands);
        macho
    }

    fn mysql_runtime(family: MysqlClientFamily, loader_name: &str) -> MysqlRuntimeDependency {
        MysqlRuntimeDependency {
            family,
            loader_name: loader_name.as_bytes().to_vec(),
        }
    }

    fn libpq_runtime(loader_name: &str) -> LibpqRuntimeDependency {
        LibpqRuntimeDependency {
            loader_name: loader_name.as_bytes().to_vec(),
        }
    }

    #[test]
    fn dynamic_dependency_names_require_exact_loader_families_and_numeric_versions() {
        for name in [b"libpq.dll".as_slice(), b"LIBPQ.SO.5", b"libpq.5.16.dylib"] {
            assert!(dynamic_library_matches(name, DynamicLibraryFamily::Libpq));
        }
        for name in [
            b"libmysql.dll".as_slice(),
            b"libmysqlclient.so.21",
            b"libmysqlclient.21.dylib",
        ] {
            assert!(dynamic_library_matches(name, DynamicLibraryFamily::Mysql));
        }
        assert!(dynamic_library_matches(
            b"libmariadb.so.3",
            DynamicLibraryFamily::Mariadb
        ));
        for name in [
            b"libpq-evil.so".as_slice(),
            b"libpq_evil.dll",
            b"libpq.so.evil",
            b"libpq.5evil.dylib",
            b"notlibpq.dll",
        ] {
            assert!(!dynamic_library_matches(name, DynamicLibraryFamily::Libpq));
        }
        for name in [
            b"libmysql_fake.dll".as_slice(),
            b"libmysqlclient.so.latest",
            b"libmysqlclient-evil.dylib",
            b"libmariadb-evil.dll",
        ] {
            assert!(!dynamic_library_matches(name, DynamicLibraryFamily::Mysql));
            assert!(!dynamic_library_matches(
                name,
                DynamicLibraryFamily::Mariadb
            ));
        }
        assert!(dynamic_dependency_references_private_staging(
            b"/tmp/.ku-link-library-1-deadbeef.dir/libpq.so.5"
        ));
        assert!(dynamic_dependency_references_private_staging(
            b"C:\\temp\\.KU-LINK-1-deadbeef\\libmysql.dll"
        ));
        assert!(!dynamic_dependency_references_private_staging(
            b"@rpath/libmysqlclient.21.dylib"
        ));
    }

    #[test]
    fn mysql_loader_identity_is_bounded_and_format_specific() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-mysql-loader-identity-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("create MySQL loader identity fixture");

        let elf_path = dir.join("libmysqlclient.so.21");
        fs::write(
            &elf_path,
            dependency_elf_with_soname(&[], Some("libmysqlclient.so.21")),
        )
        .expect("write ELF SONAME fixture");
        let elf = PinnedLinkLibrary::capture(&elf_path, "MySQL client")
            .expect("capture ELF SONAME fixture");
        assert_eq!(
            mysql_runtime_dependency(&elf, LibpqLibraryFormat::Linux)
                .expect("read ELF SONAME")
                .loader_name,
            b"libmysqlclient.so.21"
        );

        let macho_path = dir.join("libmysqlclient.21.dylib");
        let mut macho = dependency_macho(&["@rpath/libmysqlclient.21.dylib"]);
        let first_dylib = 32 + 96;
        macho[first_dylib..first_dylib + 4].copy_from_slice(&0x0du32.to_le_bytes());
        fs::write(&macho_path, macho).expect("write Mach-O install-name fixture");
        let macho = PinnedLinkLibrary::capture(&macho_path, "MySQL client")
            .expect("capture Mach-O install-name fixture");
        assert_eq!(
            mysql_runtime_dependency(&macho, LibpqLibraryFormat::Darwin)
                .expect("read Mach-O LC_ID_DYLIB")
                .loader_name,
            b"@rpath/libmysqlclient.21.dylib"
        );

        let import_path = dir.join("libmariadb.lib");
        let mut import = b"!<arch>\nfixture\0".to_vec();
        import.extend_from_slice(b"libmariadb.dll\0");
        fs::write(&import_path, import).expect("write bounded import archive fixture");
        let import = PinnedLinkLibrary::capture(&import_path, "MySQL client")
            .expect("capture import archive fixture");
        let requirement = mysql_runtime_dependency(&import, LibpqLibraryFormat::WindowsMsvc)
            .expect("read import DLL target");
        assert_eq!(requirement.family, MysqlClientFamily::Mariadb);
        assert_eq!(requirement.loader_name, b"libmariadb.dll");

        let conflict_path = dir.join("libmysql.lib");
        let mut conflict = b"!<arch>\nfixture\0".to_vec();
        conflict.extend_from_slice(b"libmariadb.dll\0");
        fs::write(&conflict_path, conflict).expect("write conflicting import archive fixture");
        let conflict = PinnedLinkLibrary::capture(&conflict_path, "MySQL client")
            .expect("capture conflicting import archive fixture");
        let error = mysql_runtime_dependency(&conflict, LibpqLibraryFormat::WindowsMsvc)
            .expect_err("canonical import alias cannot override the DLL target family")
            .to_string();
        assert!(error.contains("conflicting canonical and loader families"));
        fs::remove_dir_all(dir).expect("remove MySQL loader identity fixture");
    }

    #[test]
    fn libpq_loader_identity_is_bounded_and_format_specific() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-libpq-loader-identity-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("create libpq loader identity fixture");

        let elf_path = dir.join("libpq.so.5");
        fs::write(
            &elf_path,
            dependency_elf_with_soname(&[], Some("libpq.so.5")),
        )
        .expect("write libpq ELF SONAME fixture");
        let elf = PinnedLinkLibrary::capture(&elf_path, "libpq")
            .expect("capture libpq ELF SONAME fixture");
        assert_eq!(
            libpq_runtime_dependency(&elf, LibpqLibraryFormat::Linux)
                .expect("read libpq ELF SONAME")
                .loader_name,
            b"libpq.so.5"
        );

        let no_soname_path = dir.join("libpq.so.6");
        fs::write(&no_soname_path, dependency_elf_with_soname(&[], None))
            .expect("write libpq ELF fixture without SONAME");
        let no_soname = PinnedLinkLibrary::capture(&no_soname_path, "libpq")
            .expect("capture libpq ELF fixture without SONAME");
        let error = libpq_runtime_dependency(&no_soname, LibpqLibraryFormat::Linux)
            .expect_err("libpq ELF without SONAME must fail closed")
            .to_string();
        assert!(error.contains("no bounded DT_SONAME"));

        let macho_path = dir.join("libpq.5.dylib");
        let mut macho = dependency_macho(&["@rpath/libpq.5.dylib"]);
        let first_dylib = 32 + 96;
        macho[first_dylib..first_dylib + 4].copy_from_slice(&0x0du32.to_le_bytes());
        fs::write(&macho_path, macho).expect("write libpq Mach-O install-name fixture");
        let macho = PinnedLinkLibrary::capture(&macho_path, "libpq")
            .expect("capture libpq Mach-O install-name fixture");
        assert_eq!(
            libpq_runtime_dependency(&macho, LibpqLibraryFormat::Darwin)
                .expect("read libpq Mach-O LC_ID_DYLIB")
                .loader_name,
            b"@rpath/libpq.5.dylib"
        );

        let import_path = dir.join("libpq.lib");
        let mut import = b"!<arch>\nfixture\0".to_vec();
        import.extend_from_slice(b"libpq.dll\0");
        fs::write(&import_path, import).expect("write libpq import archive fixture");
        let import = PinnedLinkLibrary::capture(&import_path, "libpq")
            .expect("capture libpq import archive fixture");
        assert_eq!(
            libpq_runtime_dependency(&import, LibpqLibraryFormat::WindowsMsvc)
                .expect("read libpq import DLL target")
                .loader_name,
            b"libpq.dll"
        );

        let preferred_import_path = dir.join("libpqdll.lib");
        let mut preferred_import = b"!<arch>\nfixture\0".to_vec();
        preferred_import.extend_from_slice(b"libpq.dll\0");
        fs::write(&preferred_import_path, preferred_import)
            .expect("write preferred libpq import archive fixture");
        let preferred_import = PinnedLinkLibrary::capture(&preferred_import_path, "libpq")
            .expect("capture preferred libpq import archive fixture");
        assert_eq!(
            libpq_runtime_dependency(&preferred_import, LibpqLibraryFormat::WindowsMsvc)
                .expect("read preferred libpq import DLL target")
                .loader_name,
            b"libpq.dll"
        );

        fs::remove_dir_all(dir).expect("remove libpq loader identity fixture");
    }

    #[test]
    fn database_features_require_bounded_dynamic_dependencies_in_every_binary_format() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-dynamic-dependency-format-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create dependency-format fixture");
        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("explicit linux target");
        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("explicit darwin target");
        let features = CSourceFeatures {
            libpq: true,
            libmysql: true,
            ..CSourceFeatures::default()
        };
        let fixtures = [
            (
                dir.join("app-linux"),
                linux,
                dependency_elf(&["libpq.so.5", "libmysqlclient.so.21"]),
                "libpq.so.5",
                "libmysqlclient.so.21",
            ),
            (
                dir.join("app.exe"),
                windows,
                dependency_pe(&["LIBPQ.dll", "libmysql.dll"]),
                "libpq.dll",
                "libmysql.dll",
            ),
            (
                dir.join("app-darwin"),
                darwin,
                dependency_macho(&["@rpath/libpq.5.dylib", "/opt/lib/libmysqlclient.21.dylib"]),
                "@rpath/libpq.5.dylib",
                "/opt/lib/libmysqlclient.21.dylib",
            ),
        ];
        for (path, target, bytes, libpq_loader_name, mysql_loader_name) in fixtures {
            fs::write(&path, bytes).expect("write dynamic-dependency fixture");
            verify_native_binary_target(&path, &target).unwrap_or_else(|error| {
                panic!("target fixture {:?}: {error}", target.binary_format)
            });
            verify_native_binary_dynamic_dependencies(
                &path,
                &target,
                features,
                Some(&libpq_runtime(libpq_loader_name)),
                Some(&mysql_runtime(MysqlClientFamily::Mysql, mysql_loader_name)),
            )
            .unwrap_or_else(|error| {
                panic!("dependency fixture {:?}: {error}", target.binary_format)
            });
        }

        let missing = dir.join("missing-mysql");
        fs::write(&missing, dependency_elf(&["libpq.so.5"])).expect("write missing MySQL fixture");
        let error = verify_native_binary_dynamic_dependencies(
            &missing,
            &fixtures_target_linux(),
            features,
            Some(&libpq_runtime("libpq.so.5")),
            Some(&mysql_runtime(
                MysqlClientFamily::Mysql,
                "libmysqlclient.so.21",
            )),
        )
        .expect_err("a static/missing MySQL client dependency must fail closed");
        assert!(error.contains("no dynamic MySQL client"));

        let cross_family = dir.join("cross-family.exe");
        fs::write(
            &cross_family,
            dependency_pe(&["LIBPQ.dll", "libmariadb.dll"]),
        )
        .expect("write cross-family MySQL fixture");
        let windows_target = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        let error = verify_native_binary_dynamic_dependencies(
            &cross_family,
            &windows_target,
            features,
            Some(&libpq_runtime("libpq.dll")),
            Some(&mysql_runtime(MysqlClientFamily::Mysql, "libmysql.dll")),
        )
        .expect_err("a MariaDB import cannot satisfy a selected MySQL library");
        assert!(error.contains("mixes MySQL and MariaDB"));
        verify_native_binary_dynamic_dependencies(
            &cross_family,
            &windows_target,
            features,
            Some(&libpq_runtime("libpq.dll")),
            Some(&mysql_runtime(MysqlClientFamily::Mariadb, "libmariadb.dll")),
        )
        .expect("a selected MariaDB import must match its MariaDB runtime dependency");

        let wrong_loader = dir.join("wrong-loader.exe");
        fs::write(&wrong_loader, dependency_pe(&["LIBPQ.dll", "libmysql.dll"]))
            .expect("write wrong exact loader fixture");
        let error = verify_native_binary_dynamic_dependencies(
            &wrong_loader,
            &windows_target,
            features,
            Some(&libpq_runtime("libpq.dll")),
            Some(&mysql_runtime(
                MysqlClientFamily::Mysql,
                "libmysqlclient.dll",
            )),
        )
        .expect_err("same-family but different DLL target must fail closed");
        assert!(error.contains("differs from the selected client library"));

        let wrong_libpq_loader = dir.join("wrong-libpq-loader");
        fs::write(
            &wrong_libpq_loader,
            dependency_elf(&["libpq.so.5", "libmysqlclient.so.21"]),
        )
        .expect("write wrong exact libpq loader fixture");
        let error = verify_native_binary_dynamic_dependencies(
            &wrong_libpq_loader,
            &fixtures_target_linux(),
            features,
            Some(&libpq_runtime("libpq.so.6")),
            Some(&mysql_runtime(
                MysqlClientFamily::Mysql,
                "libmysqlclient.so.21",
            )),
        )
        .expect_err("same-family but different libpq loader target must fail closed");
        assert!(error.contains("differs from the selected library"));

        let mixed_family = dir.join("mixed-family.exe");
        fs::write(
            &mixed_family,
            dependency_pe(&["LIBPQ.dll", "libmysql.dll", "libmariadb.dll"]),
        )
        .expect("write mixed-family fixture");
        let error = verify_native_binary_dynamic_dependencies(
            &mixed_family,
            &windows_target,
            features,
            Some(&libpq_runtime("libpq.dll")),
            Some(&mysql_runtime(MysqlClientFamily::Mysql, "libmysql.dll")),
        )
        .expect_err("mixed MySQL and MariaDB imports must fail closed");
        assert!(error.contains("mixes MySQL and MariaDB"));

        let malformed_elf = dir.join("malformed-elf");
        let mut elf = dependency_elf(&["libpq.so.5", "libmysqlclient.so.21"]);
        elf[0x238..0x240].copy_from_slice(&(MAX_DYNAMIC_STRING_TABLE_BYTES + 1).to_le_bytes());
        fs::write(&malformed_elf, elf).expect("write malformed ELF dependency fixture");
        assert!(verify_native_binary_dynamic_dependencies(
            &malformed_elf,
            &fixtures_target_linux(),
            features,
            Some(&libpq_runtime("libpq.so.5")),
            Some(&mysql_runtime(
                MysqlClientFamily::Mysql,
                "libmysqlclient.so.21"
            ))
        )
        .is_err());

        let malformed_pe = dir.join("malformed-pe.exe");
        let mut pe = dependency_pe(&["LIBPQ.dll", "libmysql.dll"]);
        pe[0x30c..0x310].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&malformed_pe, pe).expect("write malformed PE dependency fixture");
        assert!(verify_native_binary_dynamic_dependencies(
            &malformed_pe,
            &resolve_build_target(Some("x86_64-windows"))
                .expect("windows target")
                .expect("explicit windows target"),
            features,
            Some(&libpq_runtime("libpq.dll")),
            Some(&mysql_runtime(MysqlClientFamily::Mysql, "libmysql.dll"))
        )
        .is_err());

        let oversized_optional_pe = dir.join("oversized-optional.exe");
        let mut pe = dependency_pe(&["LIBPQ.dll", "libmysql.dll"]);
        pe.resize(16 * 1024, 0);
        pe[0x94..0x96].copy_from_slice(&((MAX_PE_OPTIONAL_HEADER_BYTES + 1) as u16).to_le_bytes());
        fs::write(&oversized_optional_pe, pe).expect("write oversized PE optional header fixture");
        let mut file = fs::File::open(&oversized_optional_pe).expect("open oversized PE fixture");
        let length = file.metadata().expect("inspect oversized PE fixture").len();
        let error = read_pe_dynamic_dependencies(&mut file, length)
            .expect_err("PE dependency parser must independently cap the optional header");
        assert!(error.contains("bounded import directory"));

        let staged_dependency = dir.join("staged-dependency");
        fs::write(
            &staged_dependency,
            dependency_elf(&[
                "libpq.so.5",
                "/tmp/.ku-link-library-1-deadbeef.dir/libmysqlclient.so.21",
            ]),
        )
        .expect("write staged runtime dependency fixture");
        let error = verify_native_binary_dynamic_dependencies(
            &staged_dependency,
            &fixtures_target_linux(),
            features,
            Some(&libpq_runtime("libpq.so.5")),
            Some(&mysql_runtime(
                MysqlClientFamily::Mysql,
                "libmysqlclient.so.21",
            )),
        )
        .expect_err("private staging paths must never become runtime dependencies");
        assert!(error.contains("private Ku staging path"));

        let malformed_macho = dir.join("malformed-macho");
        let mut macho = dependency_macho(&["libpq.dylib", "libmariadb.dylib"]);
        let first_dylib = 32 + 96;
        let command_size = le_u32(&macho, first_dylib + 4);
        macho[first_dylib + 8..first_dylib + 12].copy_from_slice(&command_size.to_le_bytes());
        fs::write(&malformed_macho, macho).expect("write malformed Mach-O dependency fixture");
        assert!(verify_native_binary_dynamic_dependencies(
            &malformed_macho,
            &resolve_build_target(Some("aarch64-darwin"))
                .expect("darwin target")
                .expect("explicit darwin target"),
            features,
            Some(&libpq_runtime("libpq.dylib")),
            Some(&mysql_runtime(
                MysqlClientFamily::Mysql,
                "libmysqlclient.dylib"
            ))
        )
        .is_err());
        fs::remove_dir_all(&dir).ok();
    }

    fn fixtures_target_linux() -> BuildTarget {
        resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("explicit linux target")
    }

    #[test]
    fn non_main_package_entries_require_an_explicit_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-package-extra-entry-{}-{nonce}",
            std::process::id()
        ));
        let source_dir = dir.join("src");
        fs::create_dir_all(&source_dir).expect("create package entry fixture");
        fs::write(
            dir.join(package::MANIFEST_FILE),
            "name = \"entry_fixture\"\nversion = \"0.0.1\"\nroot = \"src\"\nmain = \"main.ku\"\n",
        )
        .expect("write package manifest");
        fs::write(source_dir.join("main.ku"), "fn main() {}\n").expect("write package main entry");
        let worker = source_dir.join("worker.ku");
        fs::write(&worker, "fn main() { println(\"worker\") }\n")
            .expect("write non-main package entry");

        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            worker.display().to_string(),
        ];
        let options = parse_build_options(&args).expect("parse non-main package build");
        let error = resolve_build_plan(&options)
            .expect_err("a non-main package entry without -o must not share package output")
            .to_string();
        assert!(error.contains("non-main package entry requires an explicit output path"));
        assert!(error.contains("ku build -o <output>"));

        let output = dir.join("bin").join("worker");
        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "-o".to_string(),
            output.display().to_string(),
            worker.display().to_string(),
        ];
        let options = parse_build_options(&args).expect("parse explicit non-main package build");
        let plan = resolve_build_plan(&options).expect("resolve explicit non-main package build");
        assert_eq!(
            plan.output.file_stem().and_then(|name| name.to_str()),
            Some("worker")
        );
        assert!(plan.native_c_output.starts_with(plan.build_dir.join("c")));
        assert_eq!(
            plan.native_c_output
                .file_name()
                .and_then(|name| name.to_str()),
            Some("worker.c")
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_target_binary_verification_accepts_only_matching_executables() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-target-format-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create target format fixture");

        let mut elf = vec![0u8; 0x200];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[6] = 1;
        elf[7] = 3;
        elf[16..18].copy_from_slice(&3u16.to_le_bytes());
        elf[18..20].copy_from_slice(&62u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[24..32].copy_from_slice(&0x400080u64.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[52..54].copy_from_slice(&64u16.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        elf[64..68].copy_from_slice(&1u32.to_le_bytes());
        elf[68..72].copy_from_slice(&5u32.to_le_bytes());
        elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
        elf[88..96].copy_from_slice(&0x400000u64.to_le_bytes());
        let elf_len = elf.len() as u64;
        elf[96..104].copy_from_slice(&elf_len.to_le_bytes());
        elf[104..112].copy_from_slice(&elf_len.to_le_bytes());
        elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes());
        let elf_path = dir.join("app-linux");
        fs::write(&elf_path, &elf).expect("write ELF fixture");

        let mut pe = vec![0u8; 0x400];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        pe[0x96..0x98].copy_from_slice(&0x0022u16.to_le_bytes());
        pe[0x98..0x9a].copy_from_slice(&0x020bu16.to_le_bytes());
        pe[0xa8..0xac].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xb8..0xbc].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xbc..0xc0].copy_from_slice(&0x200u32.to_le_bytes());
        pe[0xd0..0xd4].copy_from_slice(&0x2000u32.to_le_bytes());
        pe[0xd4..0xd8].copy_from_slice(&0x200u32.to_le_bytes());
        pe[0x104..0x108].copy_from_slice(&16u32.to_le_bytes());
        let section = 0x188usize;
        pe[section..section + 5].copy_from_slice(b".text");
        pe[section + 8..section + 12].copy_from_slice(&16u32.to_le_bytes());
        pe[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 36..section + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        let pe_path = dir.join("app.exe");
        fs::write(&pe_path, &pe).expect("write PE fixture");

        let mut macho = vec![0u8; 160];
        macho[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        macho[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        macho[12..16].copy_from_slice(&2u32.to_le_bytes());
        macho[16..20].copy_from_slice(&2u32.to_le_bytes());
        macho[20..24].copy_from_slice(&96u32.to_le_bytes());
        macho[32..36].copy_from_slice(&0x19u32.to_le_bytes());
        macho[36..40].copy_from_slice(&72u32.to_le_bytes());
        macho[64..72].copy_from_slice(&0x1000u64.to_le_bytes());
        let macho_len = macho.len() as u64;
        macho[80..88].copy_from_slice(&macho_len.to_le_bytes());
        macho[92..96].copy_from_slice(&5u32.to_le_bytes());
        macho[104..108].copy_from_slice(&0x32u32.to_le_bytes());
        macho[108..112].copy_from_slice(&24u32.to_le_bytes());
        macho[112..116].copy_from_slice(&1u32.to_le_bytes());
        let macho_path = dir.join("app-darwin");
        fs::write(&macho_path, &macho).expect("write Mach-O fixture");

        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("explicit linux target");
        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("explicit darwin target");
        verify_native_binary_target(&elf_path, &linux).expect("matching ELF target");
        verify_native_binary_target(&pe_path, &windows).expect("matching PE target");
        verify_native_binary_target(&macho_path, &darwin).expect("matching Mach-O target");
        assert!(verify_native_binary_target(&elf_path, &windows).is_err());
        assert!(verify_native_binary_target(&pe_path, &darwin).is_err());
        assert!(verify_native_binary_target(&macho_path, &linux).is_err());

        let truncated_elf = dir.join("truncated-elf");
        fs::write(&truncated_elf, &elf[..100]).expect("write truncated ELF");
        assert!(verify_native_binary_target(&truncated_elf, &linux).is_err());
        let wrong_os_elf = dir.join("wrong-os-elf");
        elf[7] = 9;
        fs::write(&wrong_os_elf, &elf).expect("write wrong-OS ELF");
        assert!(verify_native_binary_target(&wrong_os_elf, &linux).is_err());

        let truncated_pe = dir.join("truncated.exe");
        fs::write(&truncated_pe, &pe[..0x200]).expect("write truncated PE");
        assert!(verify_native_binary_target(&truncated_pe, &windows).is_err());

        let ios_macho = dir.join("app-ios");
        macho[112..116].copy_from_slice(&2u32.to_le_bytes());
        fs::write(&ios_macho, &macho).expect("write iOS Mach-O");
        assert!(verify_native_binary_target(&ios_macho, &darwin).is_err());
        let truncated_macho = dir.join("truncated-macho");
        fs::write(&truncated_macho, &macho[..100]).expect("write truncated Mach-O");
        assert!(verify_native_binary_target(&truncated_macho, &darwin).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_target_link_staging_never_reuses_old_or_non_file_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-target-staging-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create link staging fixture");
        let final_output = dir.join("app.exe");
        assert_eq!(link_output_directory(Path::new("app.exe")), Path::new("."));
        assert_eq!(link_output_directory(&final_output), dir.as_path());
        let reserved_output = dir.join(".ku-link-1-2-3.exe");
        let error = validate_native_output_name(&reserved_output)
            .expect_err("final output must not overlap the staging namespace")
            .to_string();
        assert!(error.contains("reserved .ku-link-"));
        let uppercase_reserved = dir.join(".KU-LINK-future-format.exe");
        assert!(
            validate_native_output_name(&uppercase_reserved).is_err(),
            "reserved staging prefix must be rejected case-insensitively"
        );

        let stale_file = dir.join(".ku-link-stale.exe");
        let stale_directory = dir.join(".ku-link-stale-dir");
        fs::write(&stale_file, b"unowned stale artifact").expect("write unowned stale file");
        fs::create_dir(&stale_directory).expect("create unowned stale directory");

        let first =
            LinkOutputStaging::create(&final_output).expect("reserve first private staging");
        let first_directory = first.directory.clone();
        let first_artifact = first.path().to_path_buf();
        assert_eq!(first_directory.parent(), Some(dir.as_path()));
        assert_eq!(first_artifact.parent(), Some(first_directory.as_path()));
        assert_eq!(
            first_artifact.extension().and_then(|value| value.to_str()),
            Some("exe")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&first_directory)
                .expect("inspect private staging permissions")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "private staging must be owner-only");
        }
        fs::write(&first_artifact, b"partial compiler artifact")
            .expect("write owned staging artifact");
        let second =
            LinkOutputStaging::create(&final_output).expect("reserve second private staging");
        assert_ne!(
            second.directory, first.directory,
            "each compiler fallback must receive a fresh random private directory"
        );
        drop(first);
        assert!(
            !first_directory.exists(),
            "RAII must remove only its private staging directory"
        );
        assert!(stale_file.is_file(), "unowned stale file must be preserved");
        assert!(
            stale_directory.is_dir(),
            "unowned stale directory must be preserved"
        );
        drop(second);

        let poisoned =
            LinkOutputStaging::create(&final_output).expect("reserve marker-replacement staging");
        let poisoned_directory = poisoned.directory.clone();
        fs::remove_file(&poisoned.marker).expect("remove owned staging marker");
        fs::write(&poisoned.marker, b"replacement marker").expect("replace owned staging marker");
        drop(poisoned);
        assert!(
            poisoned_directory.is_dir(),
            "RAII must not recursively delete a directory after its ownership marker is replaced"
        );
        fs::remove_dir_all(&poisoned_directory).expect("remove poisoned staging fixture");

        let oversized =
            LinkOutputStaging::create(&final_output).expect("reserve oversized staging");
        let file = fs::File::create(oversized.path()).expect("create sparse oversized artifact");
        file.set_len(MAX_NATIVE_LINK_OUTPUT_BYTES + 1)
            .expect("extend sparse oversized artifact");
        drop(file);
        let error = VerifiedLinkOutput::open(oversized.path())
            .expect_err("oversized native artifact must fail closed");
        assert!(error.contains("artifact limit"));

        let invalid_output = dir.join("output-directory.exe");
        fs::create_dir(&invalid_output).expect("create invalid output directory");
        assert!(LinkOutputStaging::create(&invalid_output).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn native_link_install_is_atomic_and_preserves_the_previous_output_on_failure() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-link-install-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create native install fixture");
        let output = dir.join("app.exe");
        fs::write(&output, b"old verified output").expect("write old native output");
        let staging = LinkOutputStaging::create(&output).expect("reserve native staging");
        fs::write(staging.path(), b"new verified output").expect("write new native staging");
        let verified = VerifiedLinkOutput::open(staging.path()).expect("open verified staging");
        install_verified_link_output(verified, &staging)
            .expect("atomically install new native output");
        assert_eq!(
            fs::read(&output).expect("read installed output"),
            b"new verified output"
        );
        assert!(
            !staging.path().exists(),
            "successful install must consume staging artifact"
        );

        let replaced = LinkOutputStaging::create(&output).expect("reserve replacement staging");
        fs::write(replaced.path(), b"verified artifact A").expect("write artifact A");
        let verified = VerifiedLinkOutput::open(replaced.path()).expect("verify artifact A");
        let displaced = replaced.directory.join("displaced.exe");
        fs::rename(replaced.path(), &displaced).expect("displace verified staging path");
        fs::write(replaced.path(), b"unverified artifact B").expect("replace staging path with B");
        let error = install_verified_link_output(verified, &replaced)
            .expect_err("verified staging path replacement must fail closed")
            .to_string();
        assert!(error.contains("replaced") || error.contains("changed"));
        assert_eq!(
            fs::read(&output).expect("read preserved output"),
            b"new verified output",
            "staging replacement must never overwrite the previous output"
        );

        let raced = LinkOutputStaging::create(&output).expect("reserve destination-race staging");
        fs::write(raced.path(), b"new candidate output").expect("write raced candidate");
        let verified = VerifiedLinkOutput::open(raced.path()).expect("verify raced candidate");
        let previous = dir.join("previous-output.exe");
        fs::rename(&output, &previous).expect("replace destination identity");
        fs::write(&output, b"concurrent user output").expect("write concurrent destination");
        let error = install_verified_link_output(verified, &raced)
            .expect_err("destination replacement must fail closed")
            .to_string();
        assert!(error.contains("changed while"));
        assert_eq!(
            fs::read(&output).expect("read concurrent output"),
            b"concurrent user output",
            "a concurrent destination must never be overwritten"
        );

        let missing_output = dir.join("initially-missing.exe");
        let missing =
            LinkOutputStaging::create(&missing_output).expect("reserve missing-output staging");
        fs::write(missing.path(), b"verified missing-output candidate")
            .expect("write missing-output candidate");
        let verified =
            VerifiedLinkOutput::open(missing.path()).expect("verify missing-output candidate");
        fs::write(&missing_output, b"concurrent creator won")
            .expect("create destination after staging reservation");
        let error = install_verified_link_output(verified, &missing)
            .expect_err("initially missing destination creation must fail closed")
            .to_string();
        assert!(error.contains("changed while"));
        assert_eq!(
            fs::read(&missing_output).expect("read concurrently created destination"),
            b"concurrent creator won"
        );

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;

            let locked = LinkOutputStaging::create(&output).expect("reserve locked staging");
            fs::write(locked.path(), b"replacement blocked by destination lock")
                .expect("write locked replacement staging");
            let verified =
                VerifiedLinkOutput::open(locked.path()).expect("verify locked replacement");
            let locked_output = fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2)
                .open(&output)
                .expect("lock previous native output against replacement");
            let error = install_verified_link_output(verified, &locked)
                .expect_err("locked destination must make atomic rename fail")
                .to_string();
            assert!(error.contains("atomically install"));
            assert!(
                locked.path().is_file(),
                "failed atomic rename must preserve verified staging"
            );
            drop(locked_output);
            assert_eq!(
                fs::read(&output).expect("read output preserved through locked failure"),
                b"concurrent user output"
            );
        }
        fs::remove_dir_all(dir).expect("remove native install fixture");
    }

    #[test]
    fn explicit_matching_host_target_links_when_a_native_toolchain_is_available() {
        let Some(target) = ["x86_64-linux", "x86_64-windows", "aarch64-darwin"]
            .into_iter()
            .map(|name| {
                resolve_build_target(Some(name))
                    .expect("supported target")
                    .expect("explicit target")
            })
            .find(BuildTarget::matches_host)
        else {
            eprintln!("skip: this host architecture has no first-stage explicit target");
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-explicit-host-link-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create explicit host link fixture");
        let source = dir.join("main.c");
        fs::write(&source, "int main(void) { return 0; }\n")
            .expect("write explicit host C fixture");
        let output = with_executable_extension(dir.join("app"), Some(&target));
        match compile_c_source(
            &source,
            &output,
            Some(&target),
            BuildProfile::Debug,
            false,
            false,
        ) {
            Ok(()) => {
                verify_native_binary_target(&output, &target)
                    .expect("explicit host output must match its target");
                assert!(
                    fs::read_dir(&dir)
                        .expect("scan explicit host fixture")
                        .flatten()
                        .all(|entry| !entry.file_name().to_string_lossy().starts_with(".ku-link-")),
                    "successful explicit host link left staging behind"
                );
            }
            Err(error) if error.to_string().contains("C compiler not found") => {
                eprintln!("skip: no host C compiler available: {error}");
            }
            Err(error) => panic!("explicit matching-host target failed: {error}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn target_incompatible_native_modules_fail_before_linking() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-target-features-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create target feature fixture");
        let source = dir.join("main.c");
        fs::write(
            &source,
            "#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1\n#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1\n#if defined(_WIN32)\n#include <winsock2.h>\n#else\n#include <pthread.h>\n#include <poll.h>\n#endif\n",
        )
        .expect("write portable socket runtime fixture");
        let features = CSourceFeatures::inspect(&source).expect("inspect feature fixture");
        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("explicit linux target");
        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        validate_c_target_features(features, Some(&linux))
            .expect("portable HTTP and Redis are valid for a Linux target");
        validate_c_target_features(features, Some(&windows))
            .expect("portable HTTP and Redis are valid for a Windows target");
        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("explicit darwin target");
        validate_c_target_features(features, Some(&darwin))
            .expect("portable HTTP and Redis are valid for a macOS target");

        fs::write(&source, "#define KU_FEATURE_LIBMYSQL 1\n")
            .expect("write libmysql feature fixture");
        let features = CSourceFeatures::inspect(&source).expect("inspect libmysql fixture");
        let non_host = [linux.clone(), windows.clone(), darwin]
            .into_iter()
            .find(|target| !target.matches_host())
            .expect("at least one supported target differs from this host");
        let error = validate_c_target_features(features, Some(&non_host))
            .expect_err("cross-target libmysql must fail closed")
            .to_string();
        assert!(error.contains("no portable target-library contract"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_mysql_toolchain_paths_are_authoritative_and_fail_closed() {
        let empty = explicit_libmysql_directory(Some(OsString::new()))
            .expect_err("empty KU_MYSQL_LIB must be rejected")
            .to_string();
        assert!(empty.contains("set but empty"));
        let relative = explicit_libmysql_directory(Some(OsString::from("relative/mysql/lib")))
            .expect_err("relative KU_MYSQL_LIB must be rejected")
            .to_string();
        assert!(relative.contains("absolute directory"));
        let relative =
            explicit_libmysql_include_dir(Some(OsString::from("relative/mysql/include")))
                .expect_err("relative KU_MYSQL_INCLUDE must be rejected")
                .to_string();
        assert!(relative.contains("absolute plain directory"));

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let root =
            env::temp_dir().join(format!("ku-mysql-toolchain-{}-{nonce}", std::process::id()));
        let guard = TempBuildDir::new(root.clone());
        let library_dir = root.join("lib");
        let include_dir = root.join("include");
        fs::create_dir_all(&library_dir).expect("create MySQL library fixture");
        fs::create_dir(&include_dir).expect("create MySQL include fixture");
        fs::write(library_dir.join("libmysql.lib"), b"fixture")
            .expect("write MySQL library fixture");
        fs::write(include_dir.join("mysql.h"), b"/* fixture */")
            .expect("write MySQL header fixture");

        assert_eq!(
            explicit_libmysql_directory(Some(library_dir.clone().into_os_string()))
                .expect("validate explicit MySQL library directory")
                .expect("explicit library directory")
                .dir,
            fs::canonicalize(&library_dir).expect("canonicalize MySQL library fixture")
        );
        assert_eq!(
            explicit_libmysql_include_dir(Some(include_dir.clone().into_os_string()))
                .expect("validate explicit MySQL include directory")
                .expect("explicit include directory"),
            fs::canonicalize(&include_dir).expect("canonicalize MySQL include fixture")
        );
        guard.cleanup();
    }

    #[test]
    fn mysql_library_selection_is_platform_specific_and_never_falls_back_to_static() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "ku-mysql-library-selection-{}-{nonce}",
            std::process::id()
        ));
        let guard = TempBuildDir::new(root.clone());
        fs::create_dir_all(&root).expect("create MySQL library selection fixture");
        let static_archive = root.join("libmysqlclient.a");
        fs::write(&static_archive, b"static fixture").expect("write static MySQL fixture");
        let static_snapshot = snapshot_libmysql_directory(&root)
            .expect("inspect static MySQL fixture")
            .expect("static MySQL directory exists");
        let error = libmysql_library_from_directory(&static_snapshot, LibpqLibraryFormat::Linux)
            .expect_err("static MySQL archive must not be selected")
            .to_string();
        assert!(error.contains("cannot link static MySQL client archive"));
        assert!(error.contains("transitive libraries"));

        let fixtures = [
            ("libmysql.lib", LibpqLibraryFormat::WindowsMsvc),
            ("libmysql.dll.a", LibpqLibraryFormat::WindowsMingw),
            ("libmysqlclient.so.21", LibpqLibraryFormat::Linux),
            ("libmysqlclient.21.dylib", LibpqLibraryFormat::Darwin),
        ];
        for (name, _) in fixtures {
            fs::write(root.join(name), b"shared or import fixture")
                .expect("write ABI-specific MySQL fixture");
        }
        let snapshot = snapshot_libmysql_directory(&root)
            .expect("inspect mixed MySQL fixture")
            .expect("mixed MySQL directory exists");
        for (name, format) in fixtures {
            let selected = libmysql_library_from_directory(&snapshot, format)
                .unwrap_or_else(|error| panic!("select {name}: {error}"));
            assert_eq!(
                selected.library.path,
                fs::canonicalize(root.join(name)).expect("canonicalize selected MySQL fixture"),
                "{format:?} must not select a library from another platform/ABI"
            );
            assert_eq!(
                mysql_client_family_from_canonical_path(&selected.library.path)
                    .expect("canonical MySQL family"),
                MysqlClientFamily::Mysql
            );
        }
        assert_eq!(
            libmysql_library_name_priority("libmysql.lib", LibpqLibraryFormat::WindowsMingw),
            None
        );
        assert_eq!(
            libmysql_library_name_priority("libmysql.dll.a", LibpqLibraryFormat::WindowsMsvc),
            None
        );
        assert_eq!(
            libmysql_library_name_priority("libmysqlclient.a", LibpqLibraryFormat::Linux),
            None
        );
        let mariadb_root = root.join("mariadb-only");
        fs::create_dir(&mariadb_root).expect("create MariaDB-only fixture");
        fs::write(
            mariadb_root.join("libmariadb.lib"),
            b"MariaDB import fixture",
        )
        .expect("write MariaDB import fixture");
        let mariadb_snapshot = snapshot_libmysql_directory(&mariadb_root)
            .expect("inspect MariaDB-only fixture")
            .expect("MariaDB-only directory exists");
        let mariadb =
            libmysql_library_from_directory(&mariadb_snapshot, LibpqLibraryFormat::WindowsMsvc)
                .expect("select MariaDB import library");
        assert_eq!(
            mysql_client_family_from_canonical_path(&mariadb.library.path)
                .expect("canonical MariaDB family"),
            MysqlClientFamily::Mariadb
        );
        #[cfg(unix)]
        {
            let alias_root = root.join("mysql-alias-to-mariadb");
            fs::create_dir(&alias_root).expect("create MySQL alias fixture");
            let target = alias_root.join("libmariadb.so.3");
            let alias = alias_root.join("libmysqlclient.so");
            fs::write(
                &target,
                dependency_elf_with_soname(&[], Some("libmariadb.so.3")),
            )
            .expect("write MariaDB compatibility target");
            std::os::unix::fs::symlink(&target, &alias)
                .expect("create libmysqlclient compatibility symlink");
            let alias_snapshot = snapshot_libmysql_directory(&alias_root)
                .expect("inspect MySQL-to-MariaDB alias")
                .expect("alias directory exists");
            let selected =
                libmysql_library_from_directory(&alias_snapshot, LibpqLibraryFormat::Linux)
                    .expect("select compatibility symlink");
            assert_eq!(
                selected.library.path,
                fs::canonicalize(&target).expect("canonicalize MariaDB target")
            );
            assert_eq!(
                mysql_client_family_from_canonical_path(&selected.library.path)
                    .expect("canonical alias target family"),
                MysqlClientFamily::Mariadb,
                "the canonical target, not the libmysqlclient alias, defines the family"
            );
            let requirement =
                mysql_runtime_dependency(&selected.library, LibpqLibraryFormat::Linux)
                    .expect("derive MariaDB family from the canonical target and DT_SONAME");
            assert_eq!(requirement.family, MysqlClientFamily::Mariadb);
            assert_eq!(requirement.loader_name, b"libmariadb.so.3");

            let conflict_path = alias_root.join("libmysqlclient.so.21");
            fs::write(
                &conflict_path,
                dependency_elf_with_soname(&[], Some("libmariadb.so.3")),
            )
            .expect("write conflicting loader identity fixture");
            let conflict = PinnedLinkLibrary::capture(&conflict_path, "MySQL client")
                .expect("capture conflicting loader fixture");
            let error = mysql_runtime_dependency(&conflict, LibpqLibraryFormat::Linux)
                .expect_err("canonical and DT_SONAME families must agree")
                .to_string();
            assert!(error.contains("conflicting canonical and loader families"));

            let missing_soname_path = alias_root.join("libmysqlclient.so.22");
            fs::write(&missing_soname_path, dependency_elf(&[]))
                .expect("write missing SONAME fixture");
            let missing_soname = PinnedLinkLibrary::capture(&missing_soname_path, "MySQL client")
                .expect("capture missing SONAME fixture");
            let error = mysql_runtime_dependency(&missing_soname, LibpqLibraryFormat::Linux)
                .expect_err("a private linked ELF without DT_SONAME must fail closed")
                .to_string();
            assert!(error.contains("no bounded DT_SONAME"));
        }
        guard.cleanup();
    }

    #[test]
    fn shared_library_handles_feed_private_copies_after_symlink_targets_are_replaced() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "ku-shared-library-symlink-{}-{nonce}",
            std::process::id()
        ));
        let guard = TempBuildDir::new(root.clone());
        fs::create_dir_all(&root).expect("create shared library symlink fixture");
        let target = root.join("libpq.so.5");
        let link = root.join("libpq.so");
        let original = b"original shared library identity";
        fs::write(&target, original).expect("write shared library target");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let linked = false;
        let selected = if linked {
            let snapshot = snapshot_libpq_directory(&root)
                .expect("inspect symlinked libpq fixture")
                .expect("symlinked libpq directory exists");
            libpq_library_from_explicit_directory(&snapshot, LibpqLibraryFormat::Linux)
                .expect("select symlinked libpq")
        } else {
            eprintln!("note: symlink creation is unavailable; exercising direct-path replacement");
            PinnedLinkLibrary::capture(&target, "libpq").expect("capture direct libpq fixture")
        };
        assert_eq!(
            selected.path,
            fs::canonicalize(&target).expect("canonicalize symlink target"),
            "the linker must receive the resolved target, not the mutable symlink"
        );
        let retired = root.join("retired-libpq.so.5");
        fs::rename(&target, &retired).expect("retire original symlink target");
        fs::write(&target, b"replacement with a different identity and length")
            .expect("replace symlink target");
        let staged = selected
            .stage_for_link("libpq")
            .expect("copy from the already-open original library handle");
        assert_eq!(
            fs::read(staged.path()).expect("read private libpq link copy"),
            original,
            "path replacement must not change the bytes supplied to the linker"
        );
        assert!(
            !staged.path().starts_with(&root),
            "the linker input must live in a separate private directory"
        );
        let staging_directory = staged
            .path()
            .parent()
            .expect("private link input has a parent")
            .to_path_buf();
        drop(staged);
        assert!(
            !staging_directory.exists(),
            "the private link-input directory must be removed by RAII"
        );
        guard.cleanup();
    }

    #[test]
    fn renamed_archives_and_oversized_link_inputs_fail_before_the_linker() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "ku-disguised-link-input-{}-{nonce}",
            std::process::id()
        ));
        let guard = TempBuildDir::new(root.clone());
        let pg = root.join("pg");
        let mysql = root.join("mysql");
        fs::create_dir_all(&pg).expect("create disguised libpq fixture");
        fs::create_dir(&mysql).expect("create disguised MySQL fixture");
        fs::write(pg.join("libpq.so"), b"!<arch>\nrenamed static archive")
            .expect("write disguised libpq archive");
        fs::write(
            mysql.join("libmysqlclient.so"),
            b"!<thin>\nrenamed thin archive",
        )
        .expect("write disguised MySQL archive");

        let pg_snapshot = snapshot_libpq_directory(&pg)
            .expect("snapshot disguised libpq")
            .expect("libpq directory exists");
        let pg_error =
            libpq_library_from_explicit_directory(&pg_snapshot, LibpqLibraryFormat::Linux)
                .expect_err("renamed libpq archive must be rejected")
                .to_string();
        assert!(pg_error.contains("cannot link static libpq archive"));

        let mysql_snapshot = snapshot_libmysql_directory(&mysql)
            .expect("snapshot disguised MySQL client")
            .expect("MySQL directory exists");
        let mysql_error =
            libmysql_library_from_directory(&mysql_snapshot, LibpqLibraryFormat::Linux)
                .expect_err("renamed MySQL archive must be rejected")
                .to_string();
        assert!(mysql_error.contains("cannot link static MySQL client archive"));

        let oversized = root.join("oversized.so");
        let oversized_file = fs::File::create(&oversized).expect("create sparse oversized input");
        oversized_file
            .set_len(MAX_PINNED_LINK_LIBRARY_BYTES + 1)
            .expect("size sparse oversized input");
        let oversized_error = PinnedLinkLibrary::capture(&oversized, "test")
            .expect_err("oversized link input must fail closed")
            .to_string();
        assert!(oversized_error.contains("link-input limit"));
        guard.cleanup();
    }

    #[test]
    fn c_compiler_candidates_treat_ku_cc_as_the_only_configured_driver() {
        let candidates = c_compiler_candidates(Some("zig cc"));
        let labels = candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["zig cc"]);
        assert_eq!(candidates[0].program, "zig");
        assert_eq!(candidates[0].args, vec!["cc"]);
        assert_eq!(candidates[0].kind, CCompilerKind::ZigCc);
        assert!(candidates[0].explicitly_configured);
        assert!(!labels.contains(&"cl"));

        let candidates = c_compiler_candidates(Some("clang"));
        let labels = candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["clang"]);
        assert_eq!(candidates[0].kind, CCompilerKind::Clang);
        assert!(candidates[0].explicitly_configured);

        let automatic = c_compiler_candidates(None);
        let automatic_labels = automatic
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(automatic_labels, vec!["zig cc", "clang", "cc", "gcc"]);
        assert!(automatic
            .iter()
            .all(|candidate| !candidate.explicitly_configured));
        assert_eq!(automatic[2].kind, CCompilerKind::Preconfigured);
        let cross_target = ["x86_64-linux", "x86_64-windows", "aarch64-darwin"]
            .into_iter()
            .map(|name| {
                resolve_build_target(Some(name))
                    .expect("supported target")
                    .expect("explicit target")
            })
            .find(|target| !target.matches_host())
            .expect("at least one target differs from this host");
        assert!(c_compiler_supports_explicit_target(
            &candidates[0],
            &cross_target
        ));
        assert!(c_compiler_supports_explicit_target(
            &automatic[0],
            &cross_target
        ));
        assert!(c_compiler_supports_explicit_target(
            &automatic[1],
            &cross_target
        ));
        assert!(!c_compiler_supports_explicit_target(
            &automatic[2],
            &cross_target
        ));
        assert!(!c_compiler_supports_explicit_target(
            &automatic[3],
            &cross_target
        ));

        let configured_gcc = parse_c_compiler_candidate("x86_64-w64-mingw32-gcc", true)
            .expect("parse configured cross gcc");
        assert_eq!(configured_gcc.kind, CCompilerKind::Preconfigured);
        assert!(c_compiler_supports_explicit_target(
            &configured_gcc,
            &cross_target
        ));
    }

    #[test]
    fn configured_c_compiler_rejects_ambiguous_environment_values() {
        assert_eq!(
            configured_c_compiler(None).expect("unset KU_CC uses discovery"),
            None
        );
        for value in ["", " ", "\t\r\n"] {
            let error = configured_c_compiler(Some(OsString::from(value)))
                .expect_err("empty KU_CC must fail closed")
                .to_string();
            assert!(error.contains("KU_CC is set but empty"));
        }
        let spaced = configured_c_compiler(Some(OsString::from(
            r#""C:\Program Files\LLVM\bin\clang.exe" --target=x86_64-pc-windows-msvc"#,
        )))
        .expect("quoted compiler path is valid")
        .expect("configured compiler");
        let candidate = c_compiler_candidates(Some(&spaced))
            .into_iter()
            .next()
            .expect("parse quoted compiler path");
        assert_eq!(candidate.program, r"C:\Program Files\LLVM\bin\clang.exe");
        assert_eq!(candidate.args, vec!["--target=x86_64-pc-windows-msvc"]);

        let error = configured_c_compiler(Some(OsString::from(r#""clang"#)))
            .expect_err("unmatched KU_CC quote must fail closed")
            .to_string();
        assert!(error.contains("unmatched double quote"));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let error = configured_c_compiler(Some(OsString::from_vec(vec![0xff])))
                .expect_err("non-Unicode KU_CC must fail closed")
                .to_string();
            assert!(error.contains("valid Unicode"));
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let error = configured_c_compiler(Some(OsString::from_wide(&[0xd800])))
                .expect_err("ill-formed UTF-16 KU_CC must fail closed")
                .to_string();
            assert!(error.contains("valid Unicode"));
        }
    }

    #[test]
    fn explicit_target_arguments_match_each_compiler_driver() {
        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("supported Linux target")
            .expect("explicit Linux target");
        let clang = parse_c_compiler_candidate("clang", false).expect("parse clang");
        assert_eq!(
            c_compiler_target_arguments(&clang, &linux).expect("clang target arguments"),
            vec!["--target=x86_64-unknown-linux-gnu"]
        );
        #[cfg(windows)]
        let zig_command = r#""C:\Program Files\zig\zig.exe" cc"#;
        #[cfg(unix)]
        let zig_command = r#""/opt/Program Files/zig/zig" cc"#;
        #[cfg(not(any(unix, windows)))]
        let zig_command = "zig cc";
        let zig =
            parse_c_compiler_candidate(zig_command, true).expect("parse absolute Zig command");
        assert_eq!(zig.kind, CCompilerKind::ZigCc);
        assert_eq!(
            c_compiler_target_arguments(&zig, &linux).expect("Zig target arguments"),
            vec!["-target", "x86_64-linux-gnu"]
        );
        let preconfigured =
            parse_c_compiler_candidate("x86_64-linux-gnu-gcc", true).expect("parse configured GCC");
        assert!(c_compiler_target_arguments(&preconfigured, &linux)
            .expect("preconfigured target arguments")
            .is_empty());

        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("supported Windows target")
            .expect("explicit Windows target");
        let gnu_clang = parse_c_compiler_candidate("clang --target=x86_64-w64-windows-gnu", true)
            .expect("parse configured GNU clang");
        assert!(
            c_compiler_target_arguments(&gnu_clang, &windows)
                .expect("declared compatible target is preserved")
                .is_empty(),
            "Ku must not append a conflicting MSVC target"
        );
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &gnu_clang),
            LibpqLibraryFormat::WindowsMingw
        );
        let conflict = c_compiler_target_arguments(&gnu_clang, &linux)
            .expect_err("declared Windows target must conflict with Linux build")
            .to_string();
        assert!(conflict.contains("conflicts"));
    }

    #[test]
    fn libpq_library_names_are_platform_specific() {
        for (name, priority) in [("libpqdll.lib", 0), ("libpq.lib", 1), ("LIBPQ.LIB", 1)] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::WindowsMsvc),
                Some(priority),
                "explicit MSVC configuration should accept {name}"
            );
        }
        assert_eq!(
            libpq_library_name_priority("libpq.dll.a", LibpqLibraryFormat::WindowsMsvc),
            None,
            "MSVC must not accept a MinGW import archive"
        );
        assert_eq!(
            libpq_library_name_priority("libpq.dll.a", LibpqLibraryFormat::WindowsMingw),
            Some(0),
            "MinGW should accept its import archive"
        );
        assert_eq!(
            libpq_library_name_priority("libpqdll.lib", LibpqLibraryFormat::WindowsMingw),
            None,
            "MinGW must not guess that an MSVC-style .lib has a compatible ABI"
        );
        assert_eq!(
            libpq_library_name_priority("libpq.lib", LibpqLibraryFormat::WindowsMingw),
            None,
            "MinGW must require an explicit .dll.a import archive"
        );
        for (name, priority) in [("libpq.so", 0), ("libpq.so.5", 1), ("libpq.so.5.17", 1)] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Linux),
                Some(priority),
                "Linux should accept {name}"
            );
        }
        for (name, priority) in [
            ("libpq.dylib", 0),
            ("libpq.5.dylib", 1),
            ("libpq.5.17.dylib", 1),
        ] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Darwin),
                Some(priority),
                "Darwin should accept {name}"
            );
        }
        for name in ["libpq.dll", "pq.lib", "libpq.so", "libpq.a"] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::WindowsMsvc),
                None,
                "MSVC should reject {name}"
            );
        }
        for name in [
            "libpq.lib",
            "libpq.so.",
            "libpq.so.backup",
            "libpq.dylib",
            "libpq.a",
            "README",
        ] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Linux),
                None,
                "Linux should reject {name}"
            );
        }
        for name in [
            "libpq.lib",
            "libpq.so",
            "libpq.foo.dylib",
            "libpq.dylib.backup",
            "libpq.a",
            "README",
        ] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Darwin),
                None,
                "Darwin should reject {name}"
            );
        }
    }

    #[test]
    fn explicit_libpq_selection_is_deterministic_across_supported_names() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-libpq-selection-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).expect("create libpq selection fixture");

        let ambiguous = dir.join("libpq.lib");
        fs::write(&ambiguous, b"target linker validates this file")
            .expect("write ambiguous Windows library fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::WindowsMsvc)
                .expect("inspect explicit Windows selection fixture"),
            Some(fs::canonicalize(&ambiguous).expect("canonicalize Windows library fixture")),
            "an authoritative KU_PG_LIB may select the conventional filename"
        );

        let import = dir.join("libpqdll.lib");
        fs::write(&import, b"target linker validates this file")
            .expect("write import-specific Windows library fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::WindowsMsvc)
                .expect("inspect explicit Windows import fixture"),
            Some(fs::canonicalize(&import).expect("canonicalize Windows import fixture"))
        );

        let old = dir.join("libpq.9.dylib");
        let new = dir.join("libpq.10.dylib");
        fs::write(&old, b"target linker validates this file")
            .expect("write old Darwin library fixture");
        fs::write(&new, b"target linker validates this file")
            .expect("write new Darwin library fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Darwin)
                .expect("inspect Darwin library fixtures"),
            Some(fs::canonicalize(&new).expect("canonicalize Darwin library fixture")),
            "the newest numeric dylib name must win"
        );

        fs::remove_dir_all(dir).expect("remove libpq selection fixture");
    }

    #[test]
    fn libpq_link_platform_uses_the_requested_target_os() {
        let host_platform = LibpqLibraryPlatform::host();
        for (name, platform) in [
            ("x86_64-linux", LibpqLibraryPlatform::Linux),
            ("x86_64-windows", LibpqLibraryPlatform::Windows),
            ("aarch64-darwin", LibpqLibraryPlatform::Darwin),
        ] {
            let target = resolve_build_target(Some(name))
                .expect("supported target")
                .expect("explicit target");
            assert_eq!(libpq_link_platform(Some(&target)), platform);
        }
        assert_eq!(libpq_link_platform(None), host_platform);
    }

    #[test]
    fn windows_libpq_format_tracks_msvc_and_mingw_compilers() {
        let msvc = CCompilerCandidate {
            label: "clang".to_string(),
            program: "clang".to_string(),
            args: Vec::new(),
            kind: CCompilerKind::Clang,
            explicitly_configured: false,
        };
        let mingw = CCompilerCandidate {
            label: "x86_64-w64-mingw32-gcc".to_string(),
            program: "x86_64-w64-mingw32-gcc".to_string(),
            args: Vec::new(),
            kind: CCompilerKind::Preconfigured,
            explicitly_configured: true,
        };
        let zig = CCompilerCandidate {
            label: "zig cc".to_string(),
            program: "zig".to_string(),
            args: vec!["cc".to_string()],
            kind: CCompilerKind::ZigCc,
            explicitly_configured: false,
        };
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &msvc),
            LibpqLibraryFormat::WindowsMsvc
        );
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &mingw),
            LibpqLibraryFormat::WindowsMingw
        );
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &zig),
            LibpqLibraryFormat::WindowsMingw
        );
    }

    #[test]
    fn libpq_feature_marker_must_be_a_standalone_generated_line() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-libpq-feature-marker-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("create libpq marker fixture");
        let source = dir.join("main.c");
        fs::write(
            &source,
            "static const char *user_text = \"#define KU_FEATURE_LIBPQ 1\";\n",
        )
        .expect("write user-text marker fixture");
        assert!(
            !CSourceFeatures::inspect(&source)
                .expect("inspect user-text marker fixture")
                .libpq,
            "marker text inside a C string must not enable PostgreSQL linking"
        );
        fs::write(&source, "#define KU_FEATURE_LIBPQ 1\n").expect("write generated marker fixture");
        assert!(
            CSourceFeatures::inspect(&source)
                .expect("inspect generated marker fixture")
                .libpq
        );
        fs::remove_dir_all(dir).expect("remove libpq marker fixture");
    }

    #[test]
    fn static_std_pg_linking_fails_closed_with_actionable_help() {
        validate_libpq_link_mode(false, true).expect("non-PG static builds remain supported");
        validate_libpq_link_mode(true, false).expect("dynamic PG builds remain supported");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-static-libpq-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).expect("create static libpq fixture");
        let source = dir.join("main.c");
        fs::write(&source, "#define KU_FEATURE_LIBPQ 1\n").expect("write static libpq fixture");
        let err = compile_c_source(
            &source,
            &dir.join("app"),
            None,
            BuildProfile::Debug,
            true,
            false,
        )
        .expect_err("static libpq must fail before invoking a linker");
        let message = err.to_string();
        assert!(message.contains("cannot safely link std.pg with --static"));
        assert!(message.contains("transitive libraries"));
        assert!(message.contains("omit --static"));
        assert!(message.contains("link the emitted C yourself"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn static_std_mysql_linking_fails_before_library_discovery() {
        validate_libmysql_link_mode(false, true).expect("non-MySQL static builds remain supported");
        validate_libmysql_link_mode(true, false).expect("dynamic MySQL builds remain supported");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-static-libmysql-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).expect("create static MySQL fixture");
        let source = dir.join("main.c");
        fs::write(&source, "#define KU_FEATURE_LIBMYSQL 1\n").expect("write static MySQL fixture");
        let error = compile_c_source(
            &source,
            &dir.join("app"),
            None,
            BuildProfile::Debug,
            true,
            false,
        )
        .expect_err("static MySQL must fail before library discovery")
        .to_string();
        assert!(error.contains("cannot safely link std.mysql with --static"));
        assert!(error.contains("transitive libraries"));
        assert!(error.contains("omit --static"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn libpq_directory_selection_requires_an_existing_library_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-libpq-discovery-{}-{nonce}", std::process::id()));
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux)
                .expect("inspect missing libpq directory"),
            None,
            "a missing directory must not be trusted"
        );

        fs::create_dir(&dir).expect("create libpq discovery fixture");
        fs::write(dir.join("README"), b"not a library").expect("write unrelated discovery fixture");
        fs::create_dir(dir.join("libpq.so")).expect("create misleading library directory");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux)
                .expect("inspect unrelated libpq directory"),
            None,
            "a directory or unrelated file must not count as libpq"
        );

        let versioned_old = dir.join("libpq.so.5");
        let versioned_new = dir.join("libpq.so.10");
        fs::write(&versioned_old, b"linker-validated fixture")
            .expect("write old versioned libpq fixture");
        fs::write(&versioned_new, b"linker-validated fixture")
            .expect("write new versioned libpq fixture");
        let archive = dir.join("libpq.a");
        fs::write(&archive, b"static fixture").expect("write static libpq fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux)
                .expect("inspect versioned libpq fixture"),
            Some(fs::canonicalize(&versioned_new).expect("canonicalize versioned libpq fixture")),
            "the newest numeric SONAME must win when no unversioned symlink exists"
        );

        fs::remove_file(versioned_old).expect("remove old versioned libpq fixture");
        fs::remove_file(versioned_new).expect("remove new versioned libpq fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux)
                .expect("inspect static-only libpq fixture"),
            None,
            "explicit selection must not select a static archive"
        );
        let err = libpq_library_in_dir(&dir, LibpqLibraryFormat::Linux)
            .expect_err("a static-only libpq directory must fail closed");
        let message = err.to_string();
        assert!(message.contains(&archive.display().to_string()));
        assert!(message.contains("transitive libraries"));
        assert!(message.contains("shared libpq"));
        assert!(message.contains("link the emitted C yourself"));

        fs::remove_file(archive).expect("remove static libpq fixture");
        fs::remove_dir(dir.join("libpq.so")).expect("remove misleading library directory");
        fs::remove_file(dir.join("README")).expect("remove unrelated discovery fixture");
        fs::remove_dir(dir).expect("remove libpq discovery fixture");
    }

    #[test]
    fn explicit_libpq_configuration_is_authoritative_and_exact() {
        assert_eq!(
            explicit_libpq_library(None, LibpqLibraryFormat::Linux)
                .expect("an unset KU_PG_LIB must remain distinguishable"),
            None
        );

        let empty = explicit_libpq_library(Some(OsString::new()), LibpqLibraryFormat::Linux)
            .expect_err("an empty KU_PG_LIB must fail closed");
        assert!(empty.to_string().contains("set but empty"));

        let relative = explicit_libpq_library(
            Some(OsString::from("relative-libpq")),
            LibpqLibraryFormat::Linux,
        )
        .expect_err("a relative KU_PG_LIB must fail closed");
        assert!(relative
            .to_string()
            .contains("must be an absolute directory"));

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-explicit-libpq-{}-{nonce}", std::process::id()));
        let missing = explicit_libpq_library(
            Some(dir.clone().into_os_string()),
            LibpqLibraryFormat::Linux,
        )
        .expect_err("a missing KU_PG_LIB must fail closed");
        assert!(missing.to_string().contains("failed to resolve KU_PG_LIB"));

        fs::create_dir(&dir).expect("create explicit libpq fixture");
        fs::write(dir.join("README"), b"not a library").expect("write unrelated explicit fixture");
        let unsupported = explicit_libpq_library(
            Some(dir.clone().into_os_string()),
            LibpqLibraryFormat::Linux,
        )
        .expect_err("an incompatible KU_PG_LIB must fail closed");
        assert!(unsupported
            .to_string()
            .contains("does not contain a target-compatible"));

        let library = dir.join("libpq.so.5");
        fs::write(&library, b"linker-validated fixture").expect("write explicit libpq fixture");
        let selected = explicit_libpq_library(
            Some(dir.clone().into_os_string()),
            LibpqLibraryFormat::Linux,
        )
        .expect("select explicit libpq")
        .expect("explicit libpq must return an exact file");
        assert_eq!(
            selected,
            fs::canonicalize(&library).expect("canonicalize explicit libpq fixture")
        );
        fs::remove_dir_all(dir).expect("remove explicit libpq fixture");
    }

    #[test]
    fn explicit_libpq_directory_can_match_a_later_compiler_abi() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-libpq-abi-fallback-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("create libpq ABI fixture");
        let library = dir.join("libpq.dll.a");
        fs::write(&library, b"linker-validated fixture").expect("write libpq ABI fixture");
        let canonical = fs::canonicalize(&dir).expect("canonicalize libpq ABI fixture");
        let snapshot = snapshot_libpq_directory(&canonical)
            .expect("inspect libpq ABI fixture")
            .expect("libpq ABI fixture must exist");

        libpq_library_from_explicit_directory(&snapshot, LibpqLibraryFormat::WindowsMsvc)
            .expect_err("an MSVC candidate must reject a MinGW-only import library");
        assert_eq!(
            libpq_library_from_explicit_directory(&snapshot, LibpqLibraryFormat::WindowsMingw)
                .expect("a later MinGW candidate must remain eligible")
                .path,
            fs::canonicalize(&library).expect("canonicalize MinGW import fixture")
        );

        let automatic_clang = parse_c_compiler_candidate("clang", false)
            .expect("parse automatic Windows clang candidate");
        assert_eq!(
            libpq_library_format_for_compiler(
                LibpqLibraryPlatform::Windows,
                &automatic_clang,
                None,
                Some("x86_64-w64-windows-gnu"),
            )
            .expect("probed GNU clang selects MinGW import ABI"),
            LibpqLibraryFormat::WindowsMingw
        );
        assert_eq!(
            libpq_library_format_for_compiler(
                LibpqLibraryPlatform::Windows,
                &automatic_clang,
                None,
                Some("x86_64-pc-windows-msvc"),
            )
            .expect("probed MSVC clang selects MSVC import ABI"),
            LibpqLibraryFormat::WindowsMsvc
        );
        assert!(
            libpq_library_format_for_compiler(
                LibpqLibraryPlatform::Windows,
                &automatic_clang,
                None,
                None,
            )
            .is_err(),
            "unqualified host clang must never guess its ABI"
        );

        #[cfg(windows)]
        let misleading_clang_command = r#""C:\mingw64\bin\clang.exe""#;
        #[cfg(unix)]
        let misleading_clang_command = "/opt/mingw64/bin/clang";
        #[cfg(not(any(unix, windows)))]
        let misleading_clang_command = "clang";
        let misleading_clang = parse_c_compiler_candidate(misleading_clang_command, false)
            .expect("parse clang below a misleading MinGW path");
        assert_eq!(misleading_clang.kind, CCompilerKind::Clang);
        assert_eq!(
            libpq_library_format_for_compiler(
                LibpqLibraryPlatform::Windows,
                &misleading_clang,
                None,
                Some("x86_64-pc-windows-msvc"),
            )
            .expect("a probed host clang target is the ABI authority"),
            LibpqLibraryFormat::WindowsMsvc
        );
        let windows_target = resolve_build_target(Some("x86_64-windows"))
            .expect("resolve Windows target")
            .expect("explicit Windows target");
        assert_eq!(
            libpq_library_format_for_compiler(
                LibpqLibraryPlatform::Windows,
                &misleading_clang,
                Some(&windows_target),
                None,
            )
            .expect("the target injected for clang is the ABI authority"),
            LibpqLibraryFormat::WindowsMsvc
        );

        let declared_msvc = parse_c_compiler_candidate("cc --target=x86_64-pc-windows-msvc", true)
            .expect("parse explicitly targeted cc");
        assert_eq!(
            libpq_library_format_for_compiler(
                LibpqLibraryPlatform::Windows,
                &declared_msvc,
                Some(&windows_target),
                None,
            )
            .expect("an explicit compiler target is the ABI authority"),
            LibpqLibraryFormat::WindowsMsvc
        );

        fs::write(dir.join("libpq.lib"), b"MSVC import fixture")
            .expect("write second ABI-specific import fixture");
        let both = snapshot_libpq_directory(&canonical)
            .expect("inspect dual-ABI libpq fixture")
            .expect("dual-ABI libpq fixture must exist");
        assert!(
            libpq_library_from_explicit_directory(&both, LibpqLibraryFormat::WindowsMsvc).is_ok()
        );
        assert!(
            libpq_library_from_explicit_directory(&both, LibpqLibraryFormat::WindowsMingw).is_ok()
        );
        fs::remove_dir_all(dir).expect("remove libpq ABI fixture");
    }

    #[test]
    fn explicit_libpq_selection_rejects_unbounded_directories() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let library_dir = env::temp_dir().join(format!(
            "ku-libpq-entry-limit-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&library_dir).expect("create bounded libpq fixture");
        for index in 0..=MAX_LIBPQ_LIBRARY_DIRECTORY_ENTRIES {
            fs::write(library_dir.join(format!("entry-{index}")), b"fixture")
                .expect("write bounded libpq entry");
        }
        let library_error = find_libpq_library(&library_dir, LibpqLibraryFormat::Linux)
            .expect_err("an oversized libpq directory must fail closed");
        assert!(library_error.to_string().contains("entry discovery limit"));
        fs::remove_dir_all(&library_dir).expect("remove bounded libpq fixture");
    }

    #[test]
    fn installed_library_versions_are_sorted_numerically() {
        assert!(numeric_version_key("17") > numeric_version_key("9.6"));
        assert!(
            numeric_version_key("MySQL Server 8.0.12") > numeric_version_key("MySQL Server 5.7")
        );

        let install_root = PathBuf::from("install").join("MySQL");
        let mut dirs = vec![
            install_root.join("9.6").join("lib"),
            install_root.join("17").join("lib"),
            install_root.join("10").join("lib"),
        ];
        sort_install_dirs_by_version(&mut dirs);
        assert_eq!(dirs.last(), Some(&install_root.join("17").join("lib")));
    }
}
