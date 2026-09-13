//! Checked, bounded supervision for known direct-child-only test services.
//!
//! Unlike run_bounded, a service must remain alive until explicit finish. This
//! does not contain descendants and must not be used for commands that spawn
//! subprocesses. A forced-stop receipt is not natural-exit or LSan evidence.

use super::*;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

type ServiceResult = Result<ForcedStopOutput, Box<BoundedProcessError>>;

#[derive(Debug)]
pub struct ForcedStopOutput {
    pub output: BoundedOutput,
}

enum Stop {
    Checked,
    Abandon,
}

#[derive(Clone)]
struct Context {
    command: String,
    timeout: Duration,
    limits: OutputLimits,
    capture: Arc<Mutex<CaptureState>>,
}

impl Context {
    fn error(&self, kind: FailureKind, detail: String) -> Box<BoundedProcessError> {
        Box::new(make_error(
            kind,
            self.command.clone(),
            self.timeout,
            self.limits,
            lock_capture(&self.capture).snapshot(),
            Some(detail),
        ))
    }
}

pub struct BoundedService {
    context: Context,
    stop: Option<Sender<Stop>>,
    worker: Option<JoinHandle<ServiceResult>>,
}

/// Start a service with one absolute deadline measured before worker creation.
/// Process spawn failures are reported by finish_checked; thread creation and
/// invalid budgets fail here. The command's three standard streams are replaced.
pub fn spawn_bounded_service(
    command: Command,
    timeout: Duration,
    limits: OutputLimits,
) -> io::Result<BoundedService> {
    start_service(command, timeout, limits, None)
}

/// Explicit fault injection for Rust harness contracts only. These helpers
/// are under tests/support; Ku has no setting, environment switch or user API
/// enabling them. Only one boundary fails; subsequent cleanup uses real I/O.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceFault {
    Wait,
    Terminate,
    ReaderSpawn,
    ReaderRead,
}

pub fn spawn_bounded_service_with_fault(
    command: Command,
    timeout: Duration,
    limits: OutputLimits,
    fault: ServiceFault,
) -> io::Result<BoundedService> {
    start_service(command, timeout, limits, Some(fault))
}

fn start_service(
    mut command: Command,
    timeout: Duration,
    limits: OutputLimits,
    fault: Option<ServiceFault>,
) -> io::Result<BoundedService> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).filter(|_| !timeout.is_zero());
    let Some(deadline) = deadline.filter(|_| limits.per_stream > 0 && limits.total > 0) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid service budget",
        ));
    };
    let context = Context {
        command: format!("{command:?}"),
        timeout,
        limits,
        capture: Arc::new(Mutex::new(CaptureState::default())),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (stop_tx, stop_rx) = mpsc::channel();
    let worker_context = context.clone();
    let worker = thread::Builder::new()
        .name("bounded-test-service".into())
        .spawn(move || supervise(command, deadline, stop_rx, &worker_context, fault))?;
    Ok(BoundedService {
        context,
        stop: Some(stop_tx),
        worker: Some(worker),
    })
}

impl BoundedService {
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn output_snapshot(&self) -> (Vec<u8>, Vec<u8>) {
        let snapshot = lock_capture(&self.context.capture).snapshot();
        (snapshot.stdout, snapshot.stderr)
    }

    pub fn finish_checked(mut self) -> ServiceResult {
        self.request_stop(Stop::Checked);
        self.join_bounded()
    }

    fn request_stop(&mut self, reason: Stop) {
        if let Some(stop) = self.stop.take() {
            // A closed receiver means the worker already has a failure receipt.
            let _ = stop.send(reason);
        }
    }

    fn join_bounded(&mut self) -> ServiceResult {
        // Normal reap/drain share one grace. The second grace is reserved for
        // the child guard if cleanup itself failed or the worker unwound.
        let deadline = Instant::now() + CLEANUP_GRACE * 2 + POLL_INTERVAL * 10;
        while !self.is_finished() && Instant::now() < deadline {
            sleep_until_poll(deadline);
        }
        if !self.is_finished() {
            self.worker.take(); // detach, never perform an unbounded join
            return Err(self.context.error(
                FailureKind::Wait,
                "service supervisor did not finish within cleanup budget".into(),
            ));
        }
        match self.worker.take().expect("owned service worker").join() {
            Ok(result) => result,
            Err(_) => Err(self.context.error(
                FailureKind::Wait,
                "service supervisor panicked; no checked receipt".into(),
            )),
        }
    }
}

impl Drop for BoundedService {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.request_stop(Stop::Abandon);
            // Drop is only bounded cleanup, never successful verification.
            let _ = self.join_bounded();
        }
    }
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.reaped || matches!(self.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + CLEANUP_GRACE;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => sleep_until_poll(deadline),
                Ok(None) | Err(_) => break,
            }
        }
    }
}

struct ReaderSlot {
    thread: ReaderThread,
    result: Option<Result<(), String>>,
}

impl ReaderSlot {
    fn poll(&mut self) {
        if self.result.is_none() {
            self.result = self.thread.join_if_done();
        }
    }
}

fn record(fault: &mut Option<(FailureKind, String)>, kind: FailureKind, detail: String) {
    if let Some((_, previous)) = fault {
        previous.push_str("; ");
        previous.push_str(&detail);
    } else {
        *fault = Some((kind, detail));
    }
}

fn inject_once(fault: &mut Option<ServiceFault>, boundary: ServiceFault) -> io::Result<()> {
    if *fault == Some(boundary) {
        *fault = None;
        Err(io::Error::other(format!("injected {boundary:?} failure")))
    } else {
        Ok(())
    }
}

struct FailingReader {
    _pipe: Box<dyn Read + Send>,
}

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("injected ReaderRead failure"))
    }
}

fn supervise(
    mut command: Command,
    deadline: Instant,
    stop: Receiver<Stop>,
    context: &Context,
    mut injected: Option<ServiceFault>,
) -> ServiceResult {
    let mut child = ChildGuard {
        child: command
            .spawn()
            .map_err(|error| context.error(FailureKind::Spawn, error.to_string()))?,
        reaped: false,
    };
    let exceeded = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::with_capacity(2);
    let mut fault = None;
    // Fallible reader creation keeps the direct child under its guard even if
    // only the first drain thread can be created. Cleanup still joins that one.
    let streams: [(Stream, Box<dyn Read + Send>); 2] = [
        (
            Stream::Stdout,
            Box::new(child.child.stdout.take().expect("piped stdout")),
        ),
        (
            Stream::Stderr,
            Box::new(child.child.stderr.take().expect("piped stderr")),
        ),
    ];
    for (stream, mut pipe) in streams {
        // Fail the second reader so the contract also checks cleanup of the
        // already running stdout reader. No injected failure skips cleanup.
        let creation = if matches!(stream, Stream::Stderr) {
            if injected == Some(ServiceFault::ReaderRead) {
                injected = None;
                pipe = Box::new(FailingReader { _pipe: pipe });
            }
            inject_once(&mut injected, ServiceFault::ReaderSpawn)
        } else {
            Ok(())
        };
        match creation.and_then(|()| {
            try_spawn_reader(
                pipe,
                stream,
                Arc::clone(&context.capture),
                Arc::clone(&exceeded),
                context.limits,
            )
        }) {
            Ok(thread) => readers.push(ReaderSlot {
                thread,
                result: None,
            }),
            Err(error) => record(&mut fault, FailureKind::Reader, error.to_string()),
        }
    }
    let mut status = None;
    let mut checked_stop = false;
    while fault.is_none() {
        if exceeded.load(Ordering::Acquire) {
            record(
                &mut fault,
                FailureKind::OutputLimit,
                "service output budget exceeded".into(),
            );
            break;
        }
        for reader in &mut readers {
            reader.poll();
            if let Some(Err(error)) = &reader.result {
                record(&mut fault, FailureKind::Reader, error.clone());
            }
        }
        if fault.is_some() {
            break;
        }
        // A completed process (even exit 0) cannot be retroactively labelled a
        // successful forced stop when the finish request arrives afterward.
        match inject_once(&mut injected, ServiceFault::Wait).and_then(|()| child.try_wait()) {
            Ok(Some(done)) => {
                status = Some(done);
                record(
                    &mut fault,
                    FailureKind::UnexpectedExit,
                    "service exited before checked stop".into(),
                );
                break;
            }
            Ok(None) => {}
            Err(error) => {
                record(&mut fault, FailureKind::Wait, error.to_string());
                break;
            }
        }
        if Instant::now() >= deadline {
            record(
                &mut fault,
                FailureKind::Timeout,
                "absolute service deadline expired".into(),
            );
            break;
        }
        match stop.try_recv() {
            Ok(Stop::Checked) => {
                checked_stop = true;
                break;
            }
            Ok(Stop::Abandon) | Err(TryRecvError::Disconnected) => {
                record(
                    &mut fault,
                    FailureKind::Termination,
                    "service abandoned without checked finish".into(),
                );
                break;
            }
            Err(TryRecvError::Empty) => sleep_until_poll(deadline),
        }
    }
    let cleanup_deadline = Instant::now() + CLEANUP_GRACE;
    if status.is_none() {
        if let Err(error) = inject_once(&mut injected, ServiceFault::Terminate)
            .and_then(|()| terminate_direct(&mut child.child))
        {
            record(
                &mut fault,
                FailureKind::Termination,
                format!("termination failed: {error}"),
            );
            // Preserve the first failure even if this independent, retained-
            // child fallback succeeds. A cleanup retry cannot yield success.
            if let Err(error) = child.child.kill() {
                record(
                    &mut fault,
                    FailureKind::Termination,
                    format!("direct fallback failed: {error}"),
                );
            }
        }
        loop {
            match child.try_wait() {
                Ok(Some(done)) => {
                    status = Some(done);
                    break;
                }
                Ok(None) if Instant::now() < cleanup_deadline => sleep_until_poll(cleanup_deadline),
                Ok(None) => {
                    record(
                        &mut fault,
                        FailureKind::Wait,
                        "direct child not reaped within cleanup grace".into(),
                    );
                    break;
                }
                Err(error) => {
                    record(&mut fault, FailureKind::Wait, error.to_string());
                    break;
                }
            }
        }
    }
    loop {
        for reader in &mut readers {
            reader.poll();
        }
        if readers.iter().all(|reader| reader.result.is_some())
            || Instant::now() >= cleanup_deadline
        {
            break;
        }
        sleep_until_poll(cleanup_deadline);
    }
    for reader in &readers {
        match &reader.result {
            Some(Ok(())) => {}
            Some(Err(error)) => record(&mut fault, FailureKind::Reader, error.clone()),
            None => record(
                &mut fault,
                FailureKind::Reader,
                "output reader did not finish after cleanup".into(),
            ),
        }
    }
    // Read all tail output before evaluating the receipt: a late flag or byte
    // written before termination cannot be erased by the stop branch.
    if exceeded.load(Ordering::Acquire) {
        record(
            &mut fault,
            FailureKind::OutputLimit,
            "final output budget exceeded".into(),
        );
    }
    if checked_stop && !status.is_some_and(expected_forced_status) {
        record(
            &mut fault,
            FailureKind::Termination,
            "actual exit status does not confirm requested forced stop".into(),
        );
    }
    if !lock_capture(&context.capture).stderr.is_empty() {
        record(
            &mut fault,
            FailureKind::Stderr,
            "service stderr must be empty".into(),
        );
    }
    if let Some((kind, detail)) = fault {
        // Unix Debug exposes raw wait-status bits, not the portable exit code.
        let actual = status.map_or_else(
            || "unconfirmed".to_string(),
            |status| format!("{status} (code={:?})", status.code()),
        );
        return Err(context.error(kind, format!("{detail}; actual status: {actual}")));
    }
    let snapshot = lock_capture(&context.capture).snapshot();
    Ok(ForcedStopOutput {
        output: BoundedOutput {
            status: status.expect("checked stop has a reaped status"),
            stdout: snapshot.stdout,
            stderr: snapshot.stderr,
        },
    })
}

#[cfg(windows)]
const FORCED_EXIT_CODE: u32 = 0x4b55_0071;

#[cfg(windows)]
fn terminate_direct(child: &mut Child) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::TerminateProcess;
    // SAFETY: the Child retains the actual process handle throughout this
    // call. Do not reopen a numeric PID. TerminateProcess is asynchronous, so
    // the supervisor separately verifies try_wait and the chosen exit code.
    // https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess
    let result = unsafe { TerminateProcess(child.as_raw_handle().cast(), FORCED_EXIT_CODE) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn terminate_direct(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(windows)]
fn expected_forced_status(status: ExitStatus) -> bool {
    status.code() == Some(FORCED_EXIT_CODE as i32)
}

#[cfg(unix)]
fn expected_forced_status(status: ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGKILL)
}

#[cfg(not(any(windows, unix)))]
fn expected_forced_status(_status: ExitStatus) -> bool {
    false
}
