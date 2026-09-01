//! Bounded child-process execution for integration tests.
//!
//! `std::process::Command::output` has no deadline and buffers both output
//! streams without a limit. A compiler or generated program that hangs or
//! floods a pipe can therefore hang the whole test binary or exhaust its
//! memory. This helper drains both pipes concurrently, keeps only a bounded
//! prefix, and contains descendants where the host APIs permit it.

use std::fmt;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const CLEANUP_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub struct OutputLimits {
    pub per_stream: usize,
    pub total: usize,
}

impl OutputLimits {
    pub const fn new(per_stream: usize, total: usize) -> Self {
        Self { per_stream, total }
    }
}

#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Spawn,
    Wait,
    Timeout,
    OutputLimit,
    Reader,
}

#[derive(Debug)]
pub struct BoundedProcessError {
    kind: FailureKind,
    command: String,
    timeout: Duration,
    limits: OutputLimits,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    io_error_kind: Option<io::ErrorKind>,
    detail: Option<String>,
}

impl BoundedProcessError {
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    pub fn io_error_kind(&self) -> Option<io::ErrorKind> {
        self.io_error_kind
    }
}

impl fmt::Display for BoundedProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            FailureKind::Spawn => "could not be spawned",
            FailureKind::Wait => "could not be waited on or reaped",
            FailureKind::Timeout => "exceeded its absolute deadline",
            FailureKind::OutputLimit => "exceeded its bounded output limit",
            FailureKind::Reader => "failed while draining stdout or stderr",
        };
        writeln!(formatter, "bounded process {reason}: {}", self.command)?;
        writeln!(
            formatter,
            "timeout={:?}, per_stream_limit={}, total_limit={}",
            self.timeout, self.limits.per_stream, self.limits.total
        )?;
        if let Some(detail) = &self.detail {
            writeln!(formatter, "detail: {detail}")?;
        }
        write_captured(formatter, "stdout", &self.stdout, self.stdout_truncated)?;
        write_captured(formatter, "stderr", &self.stderr, self.stderr_truncated)
    }
}

impl std::error::Error for BoundedProcessError {}

fn write_captured(
    formatter: &mut fmt::Formatter<'_>,
    label: &str,
    bytes: &[u8],
    truncated: bool,
) -> fmt::Result {
    write!(
        formatter,
        "\n{label} ({} captured byte{}{}):\n{}",
        bytes.len(),
        if bytes.len() == 1 { "" } else { "s" },
        if truncated { ", truncated" } else { "" },
        String::from_utf8_lossy(bytes)
    )
}

#[derive(Default)]
struct CaptureState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_seen: usize,
    stderr_seen: usize,
    total_seen: usize,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl CaptureState {
    fn append(&mut self, stream: Stream, chunk: &[u8], limits: OutputLimits) -> bool {
        let total_captured = self.stdout.len().saturating_add(self.stderr.len());
        let (seen, captured, truncated) = match stream {
            Stream::Stdout => (
                &mut self.stdout_seen,
                &mut self.stdout,
                &mut self.stdout_truncated,
            ),
            Stream::Stderr => (
                &mut self.stderr_seen,
                &mut self.stderr,
                &mut self.stderr_truncated,
            ),
        };

        *seen = seen.saturating_add(chunk.len());
        self.total_seen = self.total_seen.saturating_add(chunk.len());

        let per_stream_room = limits.per_stream.saturating_sub(captured.len());
        let total_room = limits.total.saturating_sub(total_captured);
        let keep = chunk.len().min(per_stream_room).min(total_room);
        captured.extend_from_slice(&chunk[..keep]);
        if keep != chunk.len() {
            *truncated = true;
        }

        let exceeded = *seen > limits.per_stream || self.total_seen > limits.total;
        if exceeded {
            *truncated = true;
        }
        exceeded
    }

    fn snapshot(&self) -> CaptureSnapshot {
        CaptureSnapshot {
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
            stdout_truncated: self.stdout_truncated,
            stderr_truncated: self.stderr_truncated,
        }
    }
}

struct CaptureSnapshot {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

struct ReaderThread {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl ReaderThread {
    fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn join_if_done(&mut self) -> Option<Result<(), String>> {
        if !self.is_done() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(match handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("output reader thread panicked".to_string()),
        })
    }
}

struct DoneOnDrop(Arc<AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn spawn_reader<R>(
    mut reader: R,
    stream: Stream,
    capture: Arc<Mutex<CaptureState>>,
    output_exceeded: Arc<AtomicBool>,
    limits: OutputLimits,
) -> ReaderThread
where
    R: Read + Send + 'static,
{
    let done = Arc::new(AtomicBool::new(false));
    let thread_done = Arc::clone(&done);
    let handle = thread::spawn(move || {
        let _done = DoneOnDrop(thread_done);
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            let exceeded = lock_capture(&capture).append(stream, &buffer[..read], limits);
            if exceeded {
                output_exceeded.store(true, Ordering::Release);
            }
        }
    });
    ReaderThread {
        done,
        handle: Some(handle),
    }
}

fn lock_capture(capture: &Mutex<CaptureState>) -> MutexGuard<'_, CaptureState> {
    capture
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run a child process within one absolute deadline and fixed output budgets.
///
/// The command's stdin/stdout/stderr configuration is replaced. On Unix the
/// child becomes a process-group leader. On Windows a kill-on-close Job Object
/// is used when the host permits assignment; direct-child kill/reap remains the
/// fallback for hosts whose outer job disallows it.
pub fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    limits: OutputLimits,
) -> Result<BoundedOutput, Box<BoundedProcessError>> {
    assert!(
        timeout > Duration::ZERO,
        "bounded process timeout must be positive"
    );
    assert!(
        limits.per_stream > 0 && limits.total > 0,
        "bounded process output limits must be positive"
    );

    let command_text = format!("{command:?}");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    let mut child = command.spawn().map_err(|error| {
        Box::new(BoundedProcessError {
            kind: FailureKind::Spawn,
            command: command_text.clone(),
            timeout,
            limits,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            io_error_kind: Some(error.kind()),
            detail: Some(error.to_string()),
        })
    })?;
    let tree = ProcessTree::attach(&child);

    let stdout = child.stdout.take().expect("piped child stdout");
    let stderr = child.stderr.take().expect("piped child stderr");
    let capture = Arc::new(Mutex::new(CaptureState::default()));
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let mut stdout_reader = spawn_reader(
        stdout,
        Stream::Stdout,
        Arc::clone(&capture),
        Arc::clone(&output_exceeded),
        limits,
    );
    let mut stderr_reader = spawn_reader(
        stderr,
        Stream::Stderr,
        Arc::clone(&capture),
        Arc::clone(&output_exceeded),
        limits,
    );

    let mut failure = None;
    let mut failure_detail = None;
    let mut status = None;
    loop {
        if output_exceeded.load(Ordering::Acquire) {
            failure = Some(FailureKind::OutputLimit);
            break;
        }
        match child.try_wait() {
            Ok(Some(done)) => {
                status = Some(done);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                failure = Some(FailureKind::Wait);
                failure_detail = Some(error.to_string());
                break;
            }
        }
        if Instant::now() >= deadline {
            failure = Some(FailureKind::Timeout);
            break;
        }
        sleep_until_poll(deadline);
    }

    if status.is_some() {
        // A successful direct child may leave a descendant holding an inherited
        // stdout/stderr pipe. Waiting for the readers before containing that
        // descendant would consume the whole command timeout. The child was
        // deliberately placed in its own process group/Job, so terminate the
        // remaining tree as soon as the direct child has been reaped.
        tree.terminate_descendants();
    }

    if failure.is_none() {
        while !stdout_reader.is_done() || !stderr_reader.is_done() {
            if output_exceeded.load(Ordering::Acquire) {
                failure = Some(FailureKind::OutputLimit);
                break;
            }
            if Instant::now() >= deadline {
                failure = Some(FailureKind::Timeout);
                break;
            }
            sleep_until_poll(deadline);
        }
    }

    // The child and both readers can finish between the last flag check and
    // `try_wait`. Preserve the limit failure even in that narrow race.
    if failure.is_none() && output_exceeded.load(Ordering::Acquire) {
        failure = Some(FailureKind::OutputLimit);
    }

    if failure.is_some() && !terminate_and_reap(&mut child, &tree, status.is_some()) {
        let cleanup_detail = format!(
            "child did not terminate and reap within {:?}",
            CLEANUP_GRACE
        );
        failure_detail = Some(match failure_detail {
            Some(detail) => format!("{detail}; {cleanup_detail}"),
            None => cleanup_detail,
        });
    }

    let cleanup_deadline = Instant::now()
        .checked_add(CLEANUP_GRACE)
        .unwrap_or_else(Instant::now);
    while (!stdout_reader.is_done() || !stderr_reader.is_done())
        && Instant::now() < cleanup_deadline
    {
        thread::sleep(POLL_INTERVAL);
    }

    let mut reader_errors = Vec::new();
    for reader in [&mut stdout_reader, &mut stderr_reader] {
        match reader.join_if_done() {
            Some(Ok(())) => {}
            Some(Err(error)) => reader_errors.push(error),
            None => reader_errors.push("output reader did not finish after process cleanup".into()),
        }
    }

    let snapshot = lock_capture(&capture).snapshot();
    if let Some(kind) = failure {
        if !reader_errors.is_empty() {
            let reader_detail = reader_errors.join("; ");
            failure_detail = Some(match failure_detail {
                Some(detail) => format!("{detail}; {reader_detail}"),
                None => reader_detail,
            });
        }
        return Err(Box::new(make_error(
            kind,
            command_text,
            timeout,
            limits,
            snapshot,
            failure_detail,
        )));
    }
    if !reader_errors.is_empty() {
        return Err(Box::new(make_error(
            FailureKind::Reader,
            command_text,
            timeout,
            limits,
            snapshot,
            Some(reader_errors.join("; ")),
        )));
    }

    Ok(BoundedOutput {
        status: status.expect("successful bounded child must have an exit status"),
        stdout: snapshot.stdout,
        stderr: snapshot.stderr,
    })
}

fn sleep_until_poll(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

fn terminate_and_reap(
    child: &mut std::process::Child,
    tree: &ProcessTree,
    already_reaped: bool,
) -> bool {
    tree.terminate(child);
    if already_reaped {
        return true;
    }
    let deadline = Instant::now()
        .checked_add(CLEANUP_GRACE)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => return false,
        }
    }
}

fn make_error(
    kind: FailureKind,
    command: String,
    timeout: Duration,
    limits: OutputLimits,
    snapshot: CaptureSnapshot,
    detail: Option<String>,
) -> BoundedProcessError {
    BoundedProcessError {
        kind,
        command,
        timeout,
        limits,
        stdout: snapshot.stdout,
        stderr: snapshot.stderr,
        stdout_truncated: snapshot.stdout_truncated,
        stderr_truncated: snapshot.stderr_truncated,
        io_error_kind: None,
        detail,
    }
}

#[cfg(unix)]
struct ProcessTree {
    process_group: Option<i32>,
}

#[cfg(unix)]
impl ProcessTree {
    fn attach(child: &std::process::Child) -> Self {
        Self {
            process_group: i32::try_from(child.id()).ok().filter(|pid| *pid > 0),
        }
    }

    fn terminate(&self, child: &mut std::process::Child) {
        self.terminate_descendants();
        let _ = child.kill();
    }

    fn terminate_descendants(&self) {
        if let Some(process_group) = self.process_group {
            // SAFETY: the child was spawned into a new process group whose id
            // is its validated positive pid. A negative pid targets that group.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: Option<WindowsJob>,
}

#[cfg(windows)]
impl ProcessTree {
    fn attach(child: &std::process::Child) -> Self {
        Self {
            job: WindowsJob::attach(child),
        }
    }

    fn terminate(&self, child: &mut std::process::Child) {
        if let Some(job) = &self.job {
            job.terminate();
        }
        // Keep the direct-child fallback even when Job Object assignment
        // succeeded; `kill` is harmless if the job already terminated it.
        let _ = child.kill();
    }

    fn terminate_descendants(&self) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

#[cfg(windows)]
struct WindowsJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl WindowsJob {
    fn attach(child: &std::process::Child) -> Option<Self> {
        use std::mem::{size_of, zeroed};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let information_size =
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).ok()?;

        // SAFETY: all pointers are either null or point at a correctly sized
        // Windows structure for the duration of each call. Every created
        // handle is closed on all failure paths or by `Drop`.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
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
                CloseHandle(handle);
                return None;
            }
            let process_handle = child.as_raw_handle() as HANDLE;
            if AssignProcessToJobObject(handle, process_handle) == 0 {
                CloseHandle(handle);
                return None;
            }
            Some(Self { handle })
        }
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `self.handle` remains owned and valid until `Drop`.
        unsafe {
            TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: the handle is owned by this value and is closed exactly once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn attach(_child: &std::process::Child) -> Self {
        Self
    }

    fn terminate(&self, child: &mut std::process::Child) {
        let _ = child.kill();
    }

    fn terminate_descendants(&self) {}
}
