//! Bounded whole-program execution with an operating-system memory provider.
//!
//! A submitted program, rather than an individual Ku Task, owns one fresh
//! process group. Runtime allocation ledgers remain separate from this limit.
//! The coordinator never parses source, connects to a database, or retries a
//! failed program. Output is retained within a fixed byte allowance and emitted
//! as streaming JSONL/base64 so receipts do not allocate copies of that output.

pub(crate) mod platform;

use std::{
    collections::VecDeque,
    env, fs,
    io::{self, Read, Write},
    mem::size_of,
    path::{Path, PathBuf},
    process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::error::KuError;
use platform::{Domain, GroupConfig, Metrics, Worker};

pub const SUPERVISE_USAGE: &str =
    "ku supervise [--worker-memory-bytes N] [--total-memory-bytes T] \
[--max-workers W] [--max-queued-jobs Q] [--max-queued-bytes B] \
[--max-output-bytes O] [--deadline-ms D] [--cgroup-root DIR] [--native] -- <entry>...\n\
Runs independent programs in fresh memory-limited workers; failures are never retried.\n\
The deadline includes queueing, launch and execution. Native entries must already be built.\n\
Receipts are JSONL; stdout_base64 and stderr_base64 preserve exact output bytes.\n";

const MIB: u64 = 1024 * 1024;
const MAX_WORKER_MEMORY: u64 = 1024 * 1024 * MIB;
const MAX_TOTAL_MEMORY: u64 = 16 * MAX_WORKER_MEMORY;
const MAX_WORKERS: usize = 64;
const MAX_QUEUED_JOBS: usize = 4096;
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_DEADLINE_MS: u64 = 24 * 60 * 60 * 1000;
const READER_CHUNK_BYTES: usize = 8192;
const READER_STACK_BYTES: usize = 256 * 1024;
const PER_PIPE_RESERVE_BYTES: u64 = MIB;
const FIXED_CONTROL_BYTES: u64 = 128 * 1024;
const DOMAIN_ALIGNMENT_BYTES: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const CLEANUP_GRACE: Duration = Duration::from_secs(5);
const READER_STOP_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Policy {
    worker_memory_bytes: u64,
    total_memory_bytes: u64,
    max_workers: usize,
    max_queued_jobs: usize,
    max_queued_bytes: usize,
    max_output_bytes: usize,
    deadline: Duration,
    cgroup_root: Option<PathBuf>,
    native: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            worker_memory_bytes: 128 * MIB,
            total_memory_bytes: 512 * MIB,
            max_workers: 2,
            max_queued_jobs: 32,
            max_queued_bytes: 256 * 1024,
            max_output_bytes: 1024 * 1024,
            deadline: Duration::from_secs(30),
            cgroup_root: None,
            native: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ControlReserve {
    queued_paths: u64,
    descriptors: u64,
    captures: u64,
    reader_chunks: u64,
    reader_stacks: u64,
    pipe_allowance: u64,
    fixed: u64,
    alignment_padding: u64,
    total: u64,
    worker_total: u64,
    max_running: usize,
}

impl ControlReserve {
    fn for_policy(policy: &Policy) -> Result<Self, KuError> {
        let workers = policy.max_workers as u64;
        let jobs = policy.max_queued_jobs as u64;
        // The original argv remains owned by run_cli. Allow its path storage,
        // the canonical queued paths, and transient Command/path inspection.
        let queued_paths = (policy.max_queued_bytes as u64)
            .checked_mul(4)
            .and_then(|value| value.checked_add(4 * MAX_PATH_BYTES as u64))
            .ok_or_else(|| command_error("queued path reserve overflow"))?;
        let descriptors = jobs
            .checked_mul((size_of::<QueuedJob>() + 2 * size_of::<String>()) as u64)
            .and_then(|value| {
                value.checked_add(
                    workers * (size_of::<ActiveWorker>() as u64 + 8 * MAX_PATH_BYTES as u64 + 4096),
                )
            })
            .ok_or_else(|| command_error("descriptor reserve overflow"))?;
        // Each Vec has exactly O requested capacity; their combined retained
        // length never exceeds O. Reserving 2*O also covers either stream being
        // the first writer. Allocator overhead is not a coordinator RSS claim.
        let captures = workers * 2 * policy.max_output_bytes as u64;
        let reader_chunks = workers * 2 * READER_CHUNK_BYTES as u64;
        let reader_stacks = workers * 2 * READER_STACK_BYTES as u64;
        let pipe_allowance = workers * 2 * PER_PIPE_RESERVE_BYTES;
        let fixed = FIXED_CONTROL_BYTES;
        let requested_total = [
            queued_paths,
            descriptors,
            captures,
            reader_chunks,
            reader_stacks,
            pipe_allowance,
            fixed,
        ]
        .into_iter()
        .try_fold(0_u64, |sum, value| sum.checked_add(value))
        .ok_or_else(|| command_error("control reserve overflow"))?;
        let unaligned_worker_total = policy
            .total_memory_bytes
            .checked_sub(requested_total)
            .ok_or_else(|| command_error("total memory is smaller than the control reserve"))?;
        // Round the OS-enforced aggregate down, never up. This accommodates
        // Linux hosts with either 4 KiB or 64 KiB pages and records the unused
        // remainder explicitly rather than silently enlarging the budget.
        let worker_total = unaligned_worker_total / DOMAIN_ALIGNMENT_BYTES * DOMAIN_ALIGNMENT_BYTES;
        let alignment_padding = unaligned_worker_total - worker_total;
        let total = requested_total + alignment_padding;
        let count = worker_total / policy.worker_memory_bytes;
        if count == 0 {
            return Err(command_error(
                "total memory must cover the control reserve and at least one complete worker",
            ));
        }
        let max_running = policy
            .max_workers
            .min(usize::try_from(count).unwrap_or(usize::MAX));
        Ok(Self {
            queued_paths,
            descriptors,
            captures,
            reader_chunks,
            reader_stacks,
            pipe_allowance,
            fixed,
            alignment_padding,
            total,
            worker_total,
            max_running,
        })
    }
}

#[derive(Debug)]
struct QueuedJob {
    id: u64,
    path: PathBuf,
    path_bytes: usize,
    enqueued: Instant,
    deadline: Instant,
}

#[derive(Default)]
struct Ledger {
    queued_jobs: usize,
    queued_bytes: usize,
    active_workers: usize,
    reserved_worker_bytes: u64,
    peak_active_workers: usize,
    peak_reserved_worker_bytes: u64,
}

impl Ledger {
    fn reserve_worker(&mut self, policy: &Policy, reserve: &ControlReserve) -> Result<(), KuError> {
        let next = self
            .reserved_worker_bytes
            .checked_add(policy.worker_memory_bytes)
            .ok_or_else(|| command_error("worker reservation overflow"))?;
        if self.active_workers >= reserve.max_running || next > reserve.worker_total {
            return Err(command_error("worker admission exceeded its reservation"));
        }
        self.active_workers += 1;
        self.reserved_worker_bytes = next;
        self.peak_active_workers = self.peak_active_workers.max(self.active_workers);
        self.peak_reserved_worker_bytes = self.peak_reserved_worker_bytes.max(next);
        Ok(())
    }

    fn release_worker(&mut self, policy: &Policy) {
        // Only call after process-group cleanup AND both output readers finish.
        self.active_workers -= 1;
        self.reserved_worker_bytes -= policy.worker_memory_bytes;
    }

    fn dequeue(&mut self, job: &QueuedJob) {
        self.queued_jobs -= 1;
        self.queued_bytes -= job.path_bytes;
    }
}

struct CaptureState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    total_seen: u64,
    limit: usize,
    exceeded: bool,
    reader_error: Option<String>,
}

impl CaptureState {
    fn new(limit: usize) -> io::Result<Self> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        stdout.try_reserve_exact(limit).map_err(io::Error::other)?;
        stderr.try_reserve_exact(limit).map_err(io::Error::other)?;
        Ok(Self {
            stdout,
            stderr,
            total_seen: 0,
            limit,
            exceeded: false,
            reader_error: None,
        })
    }

    fn append(&mut self, stream: Stream, bytes: &[u8]) {
        self.total_seen = self.total_seen.saturating_add(bytes.len() as u64);
        let retained = self.stdout.len() + self.stderr.len();
        let keep = bytes.len().min(self.limit.saturating_sub(retained));
        let target = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        target.extend_from_slice(&bytes[..keep]);
        self.exceeded |= keep < bytes.len();
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

fn capture_lock(capture: &Mutex<CaptureState>) -> MutexGuard<'_, CaptureState> {
    capture.lock().unwrap_or_else(|poison| poison.into_inner())
}

struct Reader {
    stop: Arc<AtomicBool>,
    producers_retired: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Reader {
    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn finish(&mut self, deadline: Instant, producers_retired: bool) -> Result<(), String> {
        if producers_retired {
            // The provider has confirmed the whole owned process group is
            // empty. Drain bytes already in the pipe, then stop at no data;
            // an inherited writer outside that group must not hold us open.
            self.producers_retired.store(true, Ordering::Release);
        }
        while !self.is_finished() && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        let forced_stop = !self.is_finished();
        if forced_stop {
            // Readers use nonblocking/peek reads, so no inherited pipe can
            // force an unbounded join after the process provider finishes.
            self.stop.store(true, Ordering::Release);
            let stop_deadline = deadline + READER_STOP_GRACE;
            while !self.is_finished() && Instant::now() < stop_deadline {
                thread::sleep(POLL_INTERVAL);
            }
        }
        if !self.is_finished() {
            return Err("output reader did not retire within its cleanup allowance".into());
        }
        self.handle.take().map_or(Ok(()), |handle| {
            handle
                .join()
                .map_err(|_| String::from("output reader panicked"))
        })?;
        if forced_stop {
            Err("output reader required a forced stop before its drain completed".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.is_finished() {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

trait CapturePipe: Read + Send + 'static {
    fn prepare(&self) -> io::Result<()>;
    fn readable_bytes(&self) -> io::Result<usize>;
}

macro_rules! capture_pipe {
    ($pipe:ty) => {
        impl CapturePipe for $pipe {
            fn prepare(&self) -> io::Result<()> {
                prepare_pipe(self)
            }
            fn readable_bytes(&self) -> io::Result<usize> {
                pipe_readable(self)
            }
        }
    };
}
capture_pipe!(ChildStdout);
capture_pipe!(ChildStderr);

#[cfg(unix)]
fn prepare_pipe(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fd is borrowed from a live owned pipe. Setting the read end to
    // nonblocking does not change the separate writer's open-file description.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn pipe_readable(_pipe: &impl std::os::fd::AsRawFd) -> io::Result<usize> {
    Ok(READER_CHUNK_BYTES)
}

#[cfg(windows)]
fn prepare_pipe(_pipe: &impl std::os::windows::io::AsRawHandle) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn pipe_readable(pipe: &impl std::os::windows::io::AsRawHandle) -> io::Result<usize> {
    #[link(name = "kernel32")]
    extern "system" {
        fn PeekNamedPipe(
            pipe: *mut std::ffi::c_void,
            buffer: *mut std::ffi::c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_available: *mut u32,
            bytes_left: *mut u32,
        ) -> i32;
    }
    let mut available = 0;
    // SAFETY: the borrowed handle is a live anonymous-pipe read handle. The
    // only output pointer refers to available for the entire call.
    let result = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((available as usize).min(READER_CHUNK_BYTES))
    }
}

#[cfg(not(any(unix, windows)))]
fn prepare_pipe<T>(_pipe: &T) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported pipe provider",
    ))
}

#[cfg(not(any(unix, windows)))]
fn pipe_readable<T>(_pipe: &T) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported pipe provider",
    ))
}

fn spawn_reader<P: CapturePipe>(
    mut pipe: P,
    stream: Stream,
    capture: Arc<Mutex<CaptureState>>,
) -> io::Result<Reader> {
    pipe.prepare()?;
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let producers_retired = Arc::new(AtomicBool::new(false));
    let reader_producers_retired = Arc::clone(&producers_retired);
    let handle = thread::Builder::new()
        .name("ku-worker-output".into())
        .stack_size(READER_STACK_BYTES)
        .spawn(move || {
            let mut chunk = [0_u8; READER_CHUNK_BYTES];
            while !reader_stop.load(Ordering::Acquire) {
                let available = match pipe.readable_bytes() {
                    Ok(0) => {
                        if reader_producers_retired.load(Ordering::Acquire) {
                            break;
                        }
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                    Ok(available) => available,
                    Err(error) if pipe_eof(&error) => break,
                    Err(error) => {
                        capture_lock(&capture).reader_error = Some(short_error(&error));
                        break;
                    }
                };
                match pipe.read(&mut chunk[..available]) {
                    Ok(0) => break,
                    Ok(count) => capture_lock(&capture).append(stream, &chunk[..count]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if reader_producers_retired.load(Ordering::Acquire) {
                            break;
                        }
                        thread::sleep(POLL_INTERVAL);
                    }
                    Err(error) if pipe_eof(&error) => break,
                    Err(error) => {
                        capture_lock(&capture).reader_error = Some(short_error(&error));
                        break;
                    }
                }
            }
        })?;
    Ok(Reader {
        stop,
        producers_retired,
        handle: Some(handle),
    })
}

fn pipe_eof(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::BrokenPipe
        || (cfg!(windows) && matches!(error.raw_os_error(), Some(109 | 232)))
}

fn short_error(error: &impl std::fmt::Display) -> String {
    let mut text = error.to_string();
    if text.len() > 1024 {
        let mut end = 1024;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

struct ActiveWorker {
    job: QueuedJob,
    started: Instant,
    worker: Worker,
    capture: Arc<Mutex<CaptureState>>,
    readers: Vec<Reader>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    WorkerExit,
    MemoryLimit,
    Timeout,
    OutputLimit,
    ReaderError,
    SpawnError,
    QueueTimeout,
    CleanupUnconfirmed,
}

impl Outcome {
    fn text(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::WorkerExit => "worker_exit",
            Self::MemoryLimit => "memory_limit",
            Self::Timeout => "timeout",
            Self::OutputLimit => "output_limit",
            Self::ReaderError => "reader_error",
            Self::SpawnError => "spawn_error",
            Self::QueueTimeout => "queue_timeout",
            Self::CleanupUnconfirmed => "cleanup_unconfirmed",
        }
    }
}

/// Run a finite batch. `args` is the full CLI argv, including `supervise`.
/// Platform refusal occurs before any user program is started.
pub fn run_supervise_command(args: &[String]) -> Result<(), KuError> {
    if args.len() == 3 && matches!(args[2].as_str(), "--help" | "-h") {
        print!("{SUPERVISE_USAGE}");
        return Ok(());
    }
    let (policy, entry_start) = parse_policy(args)?;
    let reserve = ControlReserve::for_policy(&policy)?;
    let mut domain = Domain::prepare(&GroupConfig {
        worker_memory_bytes: policy.worker_memory_bytes,
        total_worker_memory_bytes: reserve.worker_total,
        max_workers: reserve.max_running,
        cgroup_root: policy.cgroup_root.clone(),
    })
    .map_err(|error| command_error(format!("memory provider unavailable: {error}")))?;
    let mut queue = prepare_queue(&args[entry_start..], &policy)?;
    let mut ledger = Ledger {
        queued_jobs: queue.len(),
        queued_bytes: queue.iter().map(|job| job.path_bytes).sum(),
        ..Ledger::default()
    };
    let mut active: Vec<ActiveWorker> = Vec::new();
    active
        .try_reserve_exact(reserve.max_running)
        .map_err(|error| command_error(format!("cannot reserve worker descriptors: {error}")))?;
    let executable = if policy.native {
        None
    } else {
        Some(
            env::current_exe()
                .map_err(|error| command_error(format!("cannot locate Ku executable: {error}")))?,
        )
    };
    let mut failed = 0_usize;
    let mut output = io::stdout().lock();
    write_configuration(&mut output, &policy, &reserve, &ledger).map_err(|error| {
        command_error(format!("cannot publish coordinator configuration: {error}"))
    })?;

    while !queue.is_empty() || !active.is_empty() {
        for worker in &active {
            if Instant::now() >= worker.job.deadline {
                let _ = worker.worker.terminate_group();
            }
        }
        // Admit at most one worker between deadline checks. Filling a large
        // concurrency window in one pass would defer observation of workers
        // already running while later process launches are being prepared.
        let mut admitted = false;
        while active.len() < reserve.max_running {
            if admitted {
                break;
            }
            let Some(job) = queue.pop_front() else { break };
            ledger.dequeue(&job);
            if Instant::now() >= job.deadline {
                for worker in &active {
                    if Instant::now() >= worker.job.deadline {
                        let _ = worker.worker.terminate_group();
                    }
                }
                failed += 1;
                write_receipt(
                    &mut output,
                    Receipt {
                        job: &job,
                        started: None,
                        outcome: Outcome::QueueTimeout,
                        status: None,
                        metrics: None,
                        detail: None,
                        ledger: &ledger,
                    },
                )
                .map_err(|error| {
                    stop_on_output_failure(error, &mut active, &mut ledger, &policy)
                })?;
                continue;
            }
            if let Err(error) = ledger.reserve_worker(&policy, &reserve) {
                stop_active(&mut active, &mut ledger, &policy);
                return Err(error);
            }
            admitted = true;
            match start_worker(&mut domain, job, &policy, executable.as_deref()) {
                Ok(worker) => active.push(worker),
                Err(failure) => {
                    failed += 1;
                    if failure.cleaned {
                        ledger.release_worker(&policy);
                    }
                    let outcome = if failure.cleaned {
                        Outcome::SpawnError
                    } else {
                        Outcome::CleanupUnconfirmed
                    };
                    write_receipt(
                        &mut output,
                        Receipt {
                            job: &failure.job,
                            started: None,
                            outcome,
                            status: None,
                            metrics: failure.metrics.as_ref(),
                            detail: Some(&failure.detail),
                            ledger: &ledger,
                        },
                    )
                    .map_err(|error| {
                        stop_on_output_failure(error, &mut active, &mut ledger, &policy)
                    })?;
                    if !failure.cleaned {
                        stop_active(&mut active, &mut ledger, &policy);
                        return Err(command_error(format!(
                            "worker setup cleanup unconfirmed; {} bytes remain reserved",
                            ledger.reserved_worker_bytes
                        )));
                    }
                }
            }
        }

        let mut index = 0;
        while index < active.len() {
            let state = {
                let worker = &mut active[index];
                let capture = capture_lock(&worker.capture);
                if capture.exceeded {
                    Some((Outcome::OutputLimit, None, None))
                } else if let Some(error) = &capture.reader_error {
                    Some((Outcome::ReaderError, None, Some(error.clone())))
                } else if Instant::now() >= worker.job.deadline {
                    // Completion not observed by the absolute deadline cannot
                    // be reported as an on-time success after a late poll.
                    Some((Outcome::Timeout, None, None))
                } else {
                    drop(capture);
                    match worker.worker.child.try_wait() {
                        Ok(Some(status)) => Some((
                            if status.success() {
                                Outcome::Completed
                            } else {
                                Outcome::WorkerExit
                            },
                            Some(status),
                            None,
                        )),
                        Err(error) => Some((Outcome::WorkerExit, None, Some(short_error(&error)))),
                        Ok(None) => None,
                    }
                }
            };
            let Some((mut outcome, status, mut detail)) = state else {
                index += 1;
                continue;
            };
            let mut worker = active.swap_remove(index);
            // Cleanup can consume its own bounded grace. Request termination
            // for every other already-expired worker before waiting for one
            // process group or output reader to acknowledge retirement.
            for other in &active {
                if Instant::now() >= other.job.deadline {
                    let _ = other.worker.terminate_group();
                }
            }
            let deadline = Instant::now() + CLEANUP_GRACE;
            let metrics = match cleanup_worker(&mut worker, deadline) {
                Ok(metrics) => {
                    ledger.release_worker(&policy);
                    if outcome == Outcome::WorkerExit
                        && metrics.memory_limit_termination == Some(true)
                    {
                        outcome = Outcome::MemoryLimit;
                    }
                    Some(metrics)
                }
                Err(error) => {
                    outcome = Outcome::CleanupUnconfirmed;
                    detail = Some(error);
                    worker.worker.snapshot().ok()
                }
            };
            let capture = capture_lock(&worker.capture);
            // An exit can race the final pipe chunk. Evaluate the final capture
            // again after readers retire so late output is not accepted.
            if outcome == Outcome::Completed && capture.exceeded {
                outcome = Outcome::OutputLimit;
            }
            if outcome == Outcome::Completed && capture.reader_error.is_some() {
                outcome = Outcome::ReaderError;
                detail.clone_from(&capture.reader_error);
            }
            if outcome != Outcome::Completed {
                failed += 1;
            }
            let receipt = write_receipt(
                &mut output,
                Receipt {
                    job: &worker.job,
                    started: Some(worker.started),
                    outcome,
                    status,
                    metrics: metrics.as_ref(),
                    detail: detail.as_deref(),
                    ledger: &ledger,
                },
            )
            .and_then(|()| write_capture(&mut output, &capture));
            drop(capture);
            if let Err(error) = receipt {
                return Err(stop_on_output_failure(
                    error,
                    &mut active,
                    &mut ledger,
                    &policy,
                ));
            }
            if outcome == Outcome::CleanupUnconfirmed {
                stop_active(&mut active, &mut ledger, &policy);
                return Err(command_error(format!(
                    "worker cleanup unconfirmed; {} bytes remain reserved",
                    ledger.reserved_worker_bytes
                )));
            }
        }
        if !active.is_empty() && (queue.is_empty() || active.len() >= reserve.max_running) {
            thread::sleep(POLL_INTERVAL);
        }
    }
    writeln!(output, "{{\"type\":\"summary\",\"failed\":{failed},\"active_workers\":{},\"reserved_worker_bytes\":{},\"peak_active_workers\":{},\"peak_reserved_worker_bytes\":{}}}", ledger.active_workers, ledger.reserved_worker_bytes, ledger.peak_active_workers, ledger.peak_reserved_worker_bytes)
        .map_err(|error| command_error(format!("cannot publish coordinator summary: {error}")))?;
    if failed == 0 {
        Ok(())
    } else {
        Err(command_error(format!(
            "{failed} independent execution unit(s) failed; no retries were performed"
        )))
    }
}

struct StartFailure {
    job: QueuedJob,
    detail: String,
    cleaned: bool,
    metrics: Option<Metrics>,
}

fn start_worker(
    domain: &mut Domain,
    job: QueuedJob,
    policy: &Policy,
    executable: Option<&Path>,
) -> Result<ActiveWorker, Box<StartFailure>> {
    let capture = match CaptureState::new(policy.max_output_bytes) {
        Ok(capture) => Arc::new(Mutex::new(capture)),
        Err(error) => {
            return Err(Box::new(StartFailure {
                job,
                detail: short_error(&error),
                cleaned: true,
                metrics: None,
            }))
        }
    };
    let mut command = if let Some(executable) = executable {
        let mut command = Command::new(executable);
        command.arg("run").arg("--offline").arg(&job.path);
        command
    } else {
        Command::new(&job.path)
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let worker = match domain.spawn(&mut command, job.id) {
        Ok(worker) => worker,
        Err(error) => {
            return Err(Box::new(StartFailure {
                job,
                detail: short_error(&error),
                cleaned: error.cleanup_confirmed,
                metrics: None,
            }))
        }
    };
    let mut active = ActiveWorker {
        job,
        started,
        worker,
        capture,
        readers: Vec::with_capacity(2),
    };
    let setup = (|| -> io::Result<()> {
        let stdout = active
            .worker
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("worker stdout pipe is unavailable"))?;
        let stderr = active
            .worker
            .child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("worker stderr pipe is unavailable"))?;
        active.readers.push(spawn_reader(
            stdout,
            Stream::Stdout,
            Arc::clone(&active.capture),
        )?);
        active.readers.push(spawn_reader(
            stderr,
            Stream::Stderr,
            Arc::clone(&active.capture),
        )?);
        Ok(())
    })();
    if let Err(error) = setup {
        let cleanup = cleanup_worker(&mut active, Instant::now() + CLEANUP_GRACE);
        let cleaned = cleanup.is_ok();
        let metrics = cleanup.ok();
        return Err(Box::new(StartFailure {
            job: active.job,
            detail: short_error(&error),
            cleaned,
            metrics,
        }));
    }
    Ok(active)
}

fn cleanup_worker(worker: &mut ActiveWorker, deadline: Instant) -> Result<Metrics, String> {
    let process_result = worker
        .worker
        .cleanup(deadline)
        .map_err(|error| short_error(&error));
    let producers_retired = process_result.is_ok();
    let mut reader_error = None;
    for reader in &mut worker.readers {
        if let Err(error) = reader.finish(deadline, producers_retired) {
            capture_lock(&worker.capture).reader_error = Some(error.clone());
            reader_error = Some(error);
        }
    }
    match (process_result, reader_error) {
        (Ok(metrics), None) => Ok(metrics),
        (Err(error), None) | (Ok(_), Some(error)) => Err(error),
        (Err(process), Some(reader)) => Err(format!("{process}; {reader}")),
    }
}

fn stop_active(active: &mut Vec<ActiveWorker>, ledger: &mut Ledger, policy: &Policy) {
    for worker in active.iter() {
        let _ = worker.worker.terminate_group();
    }
    let deadline = Instant::now() + CLEANUP_GRACE;
    for mut worker in active.drain(..) {
        if cleanup_worker(&mut worker, deadline).is_ok() {
            ledger.release_worker(policy);
        }
    }
}

fn stop_on_output_failure(
    error: io::Error,
    active: &mut Vec<ActiveWorker>,
    ledger: &mut Ledger,
    policy: &Policy,
) -> KuError {
    stop_active(active, ledger, policy);
    command_error(format!(
        "coordinator output failed: {error}; {} bytes remain reserved",
        ledger.reserved_worker_bytes
    ))
}

fn parse_policy(args: &[String]) -> Result<(Policy, usize), KuError> {
    let mut policy = Policy::default();
    let mut index = 2;
    let mut seen = 0_u16;
    while let Some(option) = args.get(index) {
        if option == "--" {
            if index + 1 == args.len() {
                return Err(command_error("at least one entry is required after '--'"));
            }
            validate_policy(&policy)?;
            return Ok((policy, index + 1));
        }
        let bit = match option.as_str() {
            "--worker-memory-bytes" => 0,
            "--total-memory-bytes" => 1,
            "--max-workers" => 2,
            "--max-queued-jobs" => 3,
            "--max-queued-bytes" => 4,
            "--max-output-bytes" => 5,
            "--deadline-ms" => 6,
            "--cgroup-root" => 7,
            "--native" => 8,
            _ => {
                return Err(command_error(format!(
                    "unknown supervise option '{option}'"
                )))
            }
        };
        if seen & (1 << bit) != 0 {
            return Err(command_error(format!(
                "duplicate supervise option '{option}'"
            )));
        }
        seen |= 1 << bit;
        index += 1;
        if option == "--native" {
            policy.native = true;
            continue;
        }
        let value = args
            .get(index)
            .ok_or_else(|| command_error(format!("missing value for '{option}'")))?;
        index += 1;
        if option == "--cgroup-root" {
            if value.is_empty() || os_path_bytes(Path::new(value)) > MAX_PATH_BYTES {
                return Err(command_error("cgroup root path is empty or too long"));
            }
            policy.cgroup_root = Some(PathBuf::from(value));
            continue;
        }
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(command_error(format!(
                "'{option}' requires a positive decimal integer"
            )));
        }
        let number: u64 = value.parse().map_err(|_| {
            command_error(format!("'{option}' is outside the supported integer range"))
        })?;
        let count = || {
            usize::try_from(number)
                .map_err(|_| command_error(format!("'{option}' exceeds this host's integer range")))
        };
        match option.as_str() {
            "--worker-memory-bytes" => policy.worker_memory_bytes = number,
            "--total-memory-bytes" => policy.total_memory_bytes = number,
            "--max-workers" => policy.max_workers = count()?,
            "--max-queued-jobs" => policy.max_queued_jobs = count()?,
            "--max-queued-bytes" => policy.max_queued_bytes = count()?,
            "--max-output-bytes" => policy.max_output_bytes = count()?,
            "--deadline-ms" => policy.deadline = Duration::from_millis(number),
            _ => unreachable!("option was validated above"),
        }
    }
    Err(command_error(
        "supervise requires '--' followed by one or more entries",
    ))
}

fn validate_policy(policy: &Policy) -> Result<(), KuError> {
    if !(1..=MAX_WORKER_MEMORY).contains(&policy.worker_memory_bytes)
        || !(1..=MAX_TOTAL_MEMORY).contains(&policy.total_memory_bytes)
        || !(1..=MAX_WORKERS).contains(&policy.max_workers)
        || !(1..=MAX_QUEUED_JOBS).contains(&policy.max_queued_jobs)
        || !(1..=MAX_QUEUED_BYTES).contains(&policy.max_queued_bytes)
        || !(1..=MAX_OUTPUT_BYTES).contains(&policy.max_output_bytes)
        || policy.deadline == Duration::ZERO
        || policy.deadline > Duration::from_millis(MAX_DEADLINE_MS)
    {
        return Err(command_error("supervise limits must be positive and within the documented maxima (workers 64, jobs 4096, queue/output 16 MiB, deadline 24 hours, worker 1 TiB, total 16 TiB)"));
    }
    Ok(())
}

fn prepare_queue(entries: &[String], policy: &Policy) -> Result<VecDeque<QueuedJob>, KuError> {
    if entries.len() > policy.max_queued_jobs {
        return Err(command_error(
            "submitted entry count exceeds max-queued-jobs",
        ));
    }
    let mut queue = VecDeque::new();
    queue
        .try_reserve_exact(entries.len())
        .map_err(|error| command_error(format!("cannot reserve queued descriptors: {error}")))?;
    let enqueued = Instant::now();
    let deadline = enqueued
        .checked_add(policy.deadline)
        .ok_or_else(|| command_error("execution deadline overflow"))?;
    let mut bytes = 0_usize;
    for (index, entry) in entries.iter().enumerate() {
        let raw = Path::new(entry);
        let raw_bytes = os_path_bytes(raw);
        if raw_bytes == 0 || raw_bytes > MAX_PATH_BYTES {
            return Err(command_error("entry path is empty or exceeds 32 KiB"));
        }
        bytes = bytes
            .checked_add(raw_bytes)
            .filter(|value| *value <= policy.max_queued_bytes)
            .ok_or_else(|| command_error("submitted paths exceed max-queued-bytes"))?;
        let path = fs::canonicalize(raw).map_err(|error| {
            command_error(format!("cannot resolve entry '{}': {error}", raw.display()))
        })?;
        let path_bytes = os_path_bytes(&path);
        if path_bytes > MAX_PATH_BYTES {
            return Err(command_error("canonical entry path exceeds 32 KiB"));
        }
        // Charge the larger representation once; original and canonical path
        // copies are separately included in the fixed control reserve.
        if path_bytes > raw_bytes {
            bytes = bytes
                .checked_add(path_bytes - raw_bytes)
                .filter(|value| *value <= policy.max_queued_bytes)
                .ok_or_else(|| command_error("canonical paths exceed max-queued-bytes"))?;
        }
        let metadata = fs::metadata(&path).map_err(|error| {
            command_error(format!(
                "cannot inspect entry '{}': {error}",
                path.display()
            ))
        })?;
        if policy.native && !metadata.is_file() {
            return Err(command_error(
                "native entry must be a prebuilt regular file",
            ));
        }
        if !(policy.native || metadata.is_file() || metadata.is_dir()) {
            return Err(command_error(
                "Ku entry must be a source file or package directory",
            ));
        }
        queue.push_back(QueuedJob {
            id: index as u64 + 1,
            path,
            path_bytes: path_bytes.max(raw_bytes),
            enqueued,
            deadline,
        });
    }
    Ok(queue)
}

#[cfg(unix)]
fn os_path_bytes(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().len()
}

#[cfg(windows)]
fn os_path_bytes(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count().saturating_mul(2)
}

#[cfg(not(any(unix, windows)))]
fn os_path_bytes(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn command_error(message: impl Into<String>) -> KuError {
    KuError::message(format!("ku supervise: {}", message.into()))
}

fn write_configuration(
    output: &mut impl Write,
    policy: &Policy,
    reserve: &ControlReserve,
    ledger: &Ledger,
) -> io::Result<()> {
    output.write_all(b"{\"type\":\"configuration\",\"execution_unit\":\"whole_program\"")?;
    for (name, value) in [
        ("worker_memory_bytes", policy.worker_memory_bytes),
        ("total_memory_bytes", policy.total_memory_bytes),
        ("worker_group_total_bytes", reserve.worker_total),
        ("control_reserve_bytes", reserve.total),
        ("control_queued_path_bytes", reserve.queued_paths),
        ("control_descriptor_bytes", reserve.descriptors),
        ("control_capture_capacity_bytes", reserve.captures),
        ("control_reader_chunk_bytes", reserve.reader_chunks),
        (
            "control_requested_reader_stack_bytes",
            reserve.reader_stacks,
        ),
        ("control_pipe_allowance_bytes", reserve.pipe_allowance),
        ("control_fixed_bytes", reserve.fixed),
        ("control_alignment_padding_bytes", reserve.alignment_padding),
        ("max_workers", policy.max_workers as u64),
        ("max_running", reserve.max_running as u64),
        ("max_queued_jobs", policy.max_queued_jobs as u64),
        ("max_queued_bytes", policy.max_queued_bytes as u64),
        ("max_output_bytes", policy.max_output_bytes as u64),
        ("deadline_ms", policy.deadline.as_millis() as u64),
        ("queued_jobs", ledger.queued_jobs as u64),
        ("queued_bytes", ledger.queued_bytes as u64),
    ] {
        write!(output, ",\"{name}\":{value}")?;
    }
    writeln!(output, ",\"native\":{},\"deadline_includes_queue\":true,\"coordinator_rss_hard_limit\":false,\"kernel_object_bytes_exact\":false,\"retries\":0}}", policy.native)
}

struct Receipt<'a> {
    job: &'a QueuedJob,
    started: Option<Instant>,
    outcome: Outcome,
    status: Option<ExitStatus>,
    metrics: Option<&'a Metrics>,
    detail: Option<&'a str>,
    ledger: &'a Ledger,
}

fn write_receipt(output: &mut impl Write, receipt: Receipt<'_>) -> io::Result<()> {
    let Receipt {
        job,
        started,
        outcome,
        status,
        metrics,
        detail,
        ledger,
    } = receipt;
    write!(
        output,
        "{{\"type\":\"execution\",\"id\":{},\"entry\":",
        job.id
    )?;
    write_json_string(output, &job.path.to_string_lossy())?;
    write!(
        output,
        ",\"outcome\":\"{}\",\"elapsed_ms\":{},\"queue_ms\":",
        outcome.text(),
        job.enqueued.elapsed().as_millis()
    )?;
    if let Some(started) = started {
        write!(
            output,
            "{}",
            started.saturating_duration_since(job.enqueued).as_millis()
        )?;
    } else {
        output.write_all(b"null")?;
    }
    output.write_all(b",\"exit_code\":")?;
    if let Some(code) = status.and_then(|status| status.code()) {
        write!(output, "{code}")?;
    } else {
        output.write_all(b"null")?;
    }
    output.write_all(b",\"detail\":")?;
    if let Some(detail) = detail {
        write_json_string(output, detail)?;
    } else {
        output.write_all(b"null")?;
    }
    if let Some(metrics) = metrics {
        output.write_all(b",\"limit_metric\":")?;
        write_json_string(output, metrics.limit_metric)?;
        output.write_all(b",\"resource_group\":")?;
        write_json_string(output, &metrics.resource_group)?;
        for (name, value) in [
            ("memory_peak_bytes", metrics.memory_peak_bytes),
            ("memory_current_bytes", metrics.memory_current_bytes),
            ("oom_kill_count", metrics.oom_kill_count),
            ("process_count", metrics.process_count),
        ] {
            write!(output, ",\"{name}\":")?;
            if let Some(value) = value {
                write!(output, "{value}")?;
            } else {
                output.write_all(b"null")?;
            }
        }
        output.write_all(b",\"memory_limit_event\":")?;
        if let Some(observed) = metrics.memory_limit_event {
            write!(output, "{observed}")?;
        } else {
            output.write_all(b"null")?;
        }
        output.write_all(b",\"memory_limit_termination\":")?;
        if let Some(observed) = metrics.memory_limit_termination {
            write!(output, "{observed}")?;
        } else {
            output.write_all(b"null")?;
        }
    }
    write!(
        output,
        ",\"side_effects\":\"{}\",\"retries\":0,\"active_workers\":{},\"reserved_worker_bytes\":{}",
        if outcome == Outcome::Completed {
            "program_completed"
        } else {
            "unknown"
        },
        ledger.active_workers,
        ledger.reserved_worker_bytes
    )?;
    // Active workers append their capture fields before closing this record.
    if started.is_none() {
        output.write_all(b",\"stdout_base64\":\"\",\"stderr_base64\":\"\",\"output_seen_bytes\":0,\"output_truncated\":false}\n")?;
    }
    Ok(())
}

fn write_capture(output: &mut impl Write, capture: &CaptureState) -> io::Result<()> {
    output.write_all(b",\"stdout_base64\":\"")?;
    write_base64(output, &capture.stdout)?;
    output.write_all(b"\",\"stderr_base64\":\"")?;
    write_base64(output, &capture.stderr)?;
    writeln!(
        output,
        "\",\"output_seen_bytes\":{},\"output_truncated\":{}}}",
        capture.total_seen,
        capture.exceeded || capture.reader_error.is_some()
    )
}

fn write_json_string(output: &mut impl Write, text: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    for character in text.chars() {
        match character {
            '"' => output.write_all(b"\\\"")?,
            '\\' => output.write_all(b"\\\\")?,
            '\n' => output.write_all(b"\\n")?,
            '\r' => output.write_all(b"\\r")?,
            '\t' => output.write_all(b"\\t")?,
            character if character <= '\u{1f}' => write!(output, "\\u{:04x}", character as u32)?,
            character => {
                let mut bytes = [0_u8; 4];
                output.write_all(character.encode_utf8(&mut bytes).as_bytes())?;
            }
        }
    }
    output.write_all(b"\"")
}

fn write_base64(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let encoded = [
            ALPHABET[(first >> 2) as usize],
            ALPHABET[(((first & 3) << 4) | (second >> 4)) as usize],
            if chunk.len() > 1 {
                ALPHABET[(((second & 15) << 2) | (third >> 6)) as usize]
            } else {
                b'='
            },
            if chunk.len() > 2 {
                ALPHABET[(third & 63) as usize]
            } else {
                b'='
            },
        ];
        output.write_all(&encoded)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tail: &[&str]) -> Vec<String> {
        ["ku", "supervise"]
            .into_iter()
            .chain(tail.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn policy_rejects_zero_duplicates_missing_entries_and_overflow() {
        for tail in [
            vec!["--max-workers", "0", "--", "entry.ku"],
            vec!["--native", "--native", "--", "entry"],
            vec!["--"],
            vec![
                "--worker-memory-bytes",
                "18446744073709551616",
                "--",
                "entry.ku",
            ],
            vec!["--deadline-ms", "86400001", "--", "entry.ku"],
        ] {
            assert!(parse_policy(&args(&tail)).is_err());
        }
    }

    #[test]
    fn reservation_keeps_control_capacity_out_of_worker_pool() {
        let policy = Policy::default();
        let reserve = ControlReserve::for_policy(&policy).expect("default policy");
        assert_eq!(
            reserve.worker_total + reserve.total,
            policy.total_memory_bytes
        );
        assert_eq!(reserve.worker_total % DOMAIN_ALIGNMENT_BYTES, 0);
        assert!(reserve.alignment_padding < DOMAIN_ALIGNMENT_BYTES);
        let mut ledger = Ledger::default();
        for _ in 0..reserve.max_running {
            ledger.reserve_worker(&policy, &reserve).unwrap();
        }
        assert!(ledger.reserve_worker(&policy, &reserve).is_err());
        for _ in 0..reserve.max_running {
            ledger.release_worker(&policy);
        }
        assert_eq!(
            (ledger.active_workers, ledger.reserved_worker_bytes),
            (0, 0)
        );
        assert_eq!(ledger.peak_active_workers, reserve.max_running);
    }

    #[test]
    fn alignment_padding_never_enlarges_a_non_aligned_total() {
        let policy = Policy {
            total_memory_bytes: Policy::default().total_memory_bytes + 17,
            ..Policy::default()
        };
        let reserve = ControlReserve::for_policy(&policy).unwrap();
        assert_eq!(reserve.worker_total % DOMAIN_ALIGNMENT_BYTES, 0);
        assert_eq!(
            reserve.worker_total + reserve.total,
            policy.total_memory_bytes
        );
        assert!(reserve.alignment_padding < DOMAIN_ALIGNMENT_BYTES);
    }

    #[test]
    fn retired_producers_drain_buffered_output_without_waiting_for_eof() {
        struct HeldOpenPipe(io::Cursor<Vec<u8>>);
        impl Read for HeldOpenPipe {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let read = self.0.read(bytes)?;
                if read == 0 {
                    Err(io::ErrorKind::WouldBlock.into())
                } else {
                    Ok(read)
                }
            }
        }
        impl CapturePipe for HeldOpenPipe {
            fn prepare(&self) -> io::Result<()> {
                Ok(())
            }
            fn readable_bytes(&self) -> io::Result<usize> {
                Ok((self.0.get_ref().len() - self.0.position() as usize).min(READER_CHUNK_BYTES))
            }
        }
        let capture = Arc::new(Mutex::new(CaptureState::new(32).unwrap()));
        let mut reader = spawn_reader(
            HeldOpenPipe(io::Cursor::new(b"buffered-tail".to_vec())),
            Stream::Stdout,
            Arc::clone(&capture),
        )
        .unwrap();
        reader
            .finish(Instant::now() + Duration::from_millis(500), true)
            .unwrap();
        assert_eq!(capture_lock(&capture).stdout, b"buffered-tail");
    }

    #[test]
    fn combined_capture_limit_applies_across_both_streams() {
        let mut capture = CaptureState::new(7).unwrap();
        capture.append(Stream::Stdout, b"abc");
        capture.append(Stream::Stderr, b"123456");
        capture.append(Stream::Stdout, b"tail");
        assert_eq!(capture.stdout, b"abc");
        assert_eq!(capture.stderr, b"1234");
        assert_eq!(capture.total_seen, 13);
        assert!(capture.exceeded);
    }

    #[test]
    fn streaming_receipt_encoders_preserve_binary_and_escape_paths() {
        for (raw, encoded) in [
            (b"".as_slice(), ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"\0\xff\n", "AP8K"),
        ] {
            let mut output = Vec::new();
            write_base64(&mut output, raw).unwrap();
            assert_eq!(output, encoded.as_bytes());
        }
        let mut output = Vec::new();
        write_json_string(&mut output, "中\"\\\n\0").unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"中\\\"\\\\\\n\\u0000\""
        );
    }
}
