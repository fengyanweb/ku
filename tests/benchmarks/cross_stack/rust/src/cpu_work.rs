//! Explicit comparison modes. Legacy remains in main; these modes share its
//! finite integer kernel and add bounded yielding/cancellation, not Ku IR rules.
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Legacy,
    Cooperative,
    Pool,
}
impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy-blocking",
            Self::Cooperative => "tokio-coop",
            Self::Pool => "bounded-cancel",
        }
    }
}
#[derive(Clone, Copy)]
pub struct Config {
    pub mode: Mode,
    pub chunk: usize,
    pub event_interval: Option<u32>,
    pub trace: bool,
}
impl Config {
    pub fn from_env() -> Result<Self, io::Error> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
        let mode = match std::env::var("BENCH_CPU_MODE").ok().as_deref() {
            None | Some("legacy-blocking") => Mode::Legacy,
            Some("tokio-coop") => Mode::Cooperative,
            Some("bounded-cancel") => Mode::Pool,
            _ => return Err(invalid("invalid BENCH_CPU_MODE")),
        };
        let chunk = std::env::var("BENCH_CPU_CHUNK")
            .unwrap_or_else(|_| "4096".into())
            .parse::<usize>()
            .map_err(|_| invalid("invalid BENCH_CPU_CHUNK"))?;
        if !(1..=65536).contains(&chunk) {
            return Err(invalid("BENCH_CPU_CHUNK must be 1..65536"));
        }
        let event_interval = std::env::var("BENCH_EVENT_INTERVAL")
            .ok()
            .map(|value| value.parse::<u32>())
            .transpose()
            .map_err(|_| invalid("invalid BENCH_EVENT_INTERVAL"))?;
        if event_interval == Some(0) {
            return Err(invalid("BENCH_EVENT_INTERVAL must be positive"));
        }
        let trace = match std::env::var("BENCH_CPU_TRACE").ok().as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            _ => return Err(invalid("BENCH_CPU_TRACE must be 0 or 1")),
        };
        Ok(Self {
            mode,
            chunk,
            event_interval,
            trace,
        })
    }
}

// Diagnostic events are separate from ordinary scoring: no clock/lock/ring
// allocation occurs here when disabled. Pages keep management output bounded.
const TRACE_CAPACITY: usize = 16384;
const TRACE_PAGE: usize = 256;
#[derive(Serialize)]
struct Event {
    sequence: u64,
    rid: String,
    event: &'static str,
    monotonic_ns: Option<u64>,
    iteration: usize,
    started: bool,
    outcome: &'static str,
    http_status: Option<u16>,
    cpu_permits_available: Option<usize>,
    actual_cpu_jobs: Option<usize>,
}
struct Events {
    rows: VecDeque<Event>,
    dropped: u64,
    sequence: u64,
    drained_events: u64,
    drained_through: u64,
}
pub struct Trace {
    enabled: bool,
    events: Mutex<Events>,
}
impl Trace {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            events: Mutex::new(Events {
                rows: if enabled {
                    VecDeque::with_capacity(TRACE_CAPACITY)
                } else {
                    VecDeque::new()
                },
                dropped: 0,
                sequence: 0,
                drained_events: 0,
                drained_through: 0,
            }),
        }
    }
    pub fn snapshot(&self) -> Value {
        if !self.enabled {
            return json!({"enabled":false});
        }
        let (rows, pending, dropped, sequence) = {
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            let count = events.rows.len().min(TRACE_PAGE);
            let rows = events.rows.drain(..count).collect::<Vec<_>>();
            events.drained_events = events.drained_events.saturating_add(rows.len() as u64);
            if let Some(last) = rows.last() {
                events.drained_through = last.sequence;
            }
            (rows, events.rows.len(), events.dropped, events.sequence)
        };
        json!({"enabled":true,"events":rows,"pending":pending,"dropped":dropped,
            "last_sequence":sequence,"capacity":TRACE_CAPACITY,"page_limit":TRACE_PAGE,
            "clock":"CLOCK_MONOTONIC ns on Linux/macOS; null is unavailable",
            "scope":"diagnostic only; snapshot drains one bounded page"})
    }
    fn record(&self, control: &Control, event: &'static str) {
        if !self.enabled {
            return;
        }
        let now = monotonic_ns();
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.sequence = events.sequence.saturating_add(1);
        if events.rows.len() == TRACE_CAPACITY {
            events.dropped = events.dropped.saturating_add(1);
            return;
        }
        let sequence = events.sequence;
        events.rows.push_back(Event {
            sequence,
            rid: control.rid.clone(),
            event,
            monotonic_ns: now,
            iteration: control.iteration.load(Ordering::Relaxed),
            started: control.started.load(Ordering::Acquire),
            outcome: outcome_name(control.outcome.load(Ordering::Acquire)),
            http_status: None,
            cpu_permits_available: control
                .resource_counts
                .as_ref()
                .map(|(cpu, _)| cpu.available_permits()),
            actual_cpu_jobs: control
                .resource_counts
                .as_ref()
                .map(|(_, actual)| actual.load(Ordering::Relaxed)),
        });
    }

    fn request_event(
        &self,
        rid: &str,
        event: &'static str,
        outcome: &'static str,
        http_status: Option<u16>,
    ) {
        if !self.enabled {
            return;
        }
        let now = monotonic_ns();
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.sequence = events.sequence.saturating_add(1);
        if events.rows.len() == TRACE_CAPACITY {
            events.dropped = events.dropped.saturating_add(1);
            return;
        }
        let sequence = events.sequence;
        events.rows.push_back(Event {
            sequence,
            rid: rid.to_owned(),
            event,
            monotonic_ns: now,
            iteration: 0,
            started: false,
            outcome,
            http_status,
            cpu_permits_available: None,
            actual_cpu_jobs: None,
        });
    }
}

/// Captured only after axum's graceful serve future completed. The caller must
/// drop the entire Tokio runtime before consuming this receipt: a numeric zero
/// in `actual` alone precedes the final `reusable` event in Resources::drop.
pub struct FinalReceipt {
    trace: Arc<Trace>,
    actual: Arc<AtomicUsize>,
    mode: Mode,
}
#[derive(Serialize)]
struct FinalTrace {
    schema: &'static str,
    enabled: bool,
    cpu_mode: &'static str,
    cpu_entry_trace_covered: bool,
    sealed: bool,
    complete: bool,
    graceful_complete: bool,
    runtime_drop_completed: bool,
    pid: u32,
    clock: &'static str,
    exported_monotonic_ns: Option<u64>,
    exported_unix_ns: Option<u64>,
    actual_cpu_jobs: usize,
    trace_arc_owners: usize,
    actual_counter_arc_owners: usize,
    dropped: u64,
    final_sequence: u64,
    prefix_drained_events: u64,
    prefix_drained_through_sequence: u64,
    remaining_events: usize,
    suffix_first_sequence: Option<u64>,
    suffix_last_sequence: Option<u64>,
    sequence_complete: bool,
    export_limit_bytes: usize,
    events: Vec<Event>,
}
const FINAL_TRACE_BYTES: usize = 16 * 1024 * 1024;
impl FinalReceipt {
    pub fn after_graceful(trace: Arc<Trace>, actual: Arc<AtomicUsize>, mode: Mode) -> Self {
        Self {
            trace,
            actual,
            mode,
        }
    }
    fn seal_after_runtime(self) -> FinalTrace {
        let trace_arc_owners = Arc::strong_count(&self.trace);
        let actual_counter_arc_owners = Arc::strong_count(&self.actual);
        let actual_cpu_jobs = self.actual.load(Ordering::Acquire);
        let (rows, dropped, sequence, drained_events, drained_through) = {
            let mut events = self.trace.events.lock().unwrap_or_else(|e| e.into_inner());
            let rows = events.rows.drain(..).collect::<Vec<_>>();
            (
                rows,
                events.dropped,
                events.sequence,
                events.drained_events,
                events.drained_through,
            )
        };
        let sequence_complete = dropped == 0
            && sequence != u64::MAX
            && drained_events == drained_through
            && drained_events.checked_add(rows.len() as u64) == Some(sequence)
            && rows.iter().enumerate().all(|(index, row)| {
                drained_through.checked_add(index as u64 + 1) == Some(row.sequence)
            });
        let complete = self.trace.enabled
            && self.mode != Mode::Legacy
            && actual_cpu_jobs == 0
            && trace_arc_owners == 1
            && actual_counter_arc_owners == 1
            && Arc::weak_count(&self.trace) == 0
            && Arc::weak_count(&self.actual) == 0
            && sequence_complete;
        FinalTrace {
            schema: "ku-cross-stack-rust-cpu-final-v1",
            enabled: self.trace.enabled,
            cpu_mode: self.mode.name(),
            cpu_entry_trace_covered: self.mode != Mode::Legacy,
            sealed: true,
            complete,
            graceful_complete: true,
            runtime_drop_completed: true,
            pid: std::process::id(),
            clock: "CLOCK_MONOTONIC ns on Linux/macOS; null is unavailable",
            exported_monotonic_ns: monotonic_ns(),
            exported_unix_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|value| value.as_nanos().try_into().ok()),
            actual_cpu_jobs,
            trace_arc_owners,
            actual_counter_arc_owners,
            dropped,
            final_sequence: sequence,
            prefix_drained_events: drained_events,
            prefix_drained_through_sequence: drained_through,
            remaining_events: rows.len(),
            suffix_first_sequence: rows.first().map(|row| row.sequence),
            suffix_last_sequence: rows.last().map(|row| row.sequence),
            sequence_complete,
            export_limit_bytes: FINAL_TRACE_BYTES,
            events: rows,
        }
    }
    /// Diagnostic-only final suffix. No file exists on the trace-disabled path.
    /// An old file is never overwritten; partial writes are removed. A valid
    /// but incomplete receipt is retained and returns failure, never success.
    pub fn export_after_runtime(self, path: &Path) -> io::Result<()> {
        let snapshot = self.seal_after_runtime();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let written = (|| -> io::Result<()> {
            {
                let mut output = LimitedWriter {
                    inner: &mut file,
                    remaining: FINAL_TRACE_BYTES,
                };
                serde_json::to_writer(&mut output, &snapshot).map_err(io::Error::other)?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
            file.sync_all()
        })();
        if let Err(error) = written {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        if !snapshot.complete {
            return Err(io::Error::other("final CPU trace is incomplete"));
        }
        Ok(())
    }
}
struct LimitedWriter<'a> {
    inner: &'a mut std::fs::File,
    remaining: usize,
}
impl Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("final CPU trace exceeds byte limit"));
        }
        let count = self.inner.write(bytes)?;
        self.remaining -= count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// HTTP lifecycle observation, separate from physical CPU ownership.
///
/// A parsed request may be rejected before a CPU Control exists. Conversely,
/// absence of this guard's first event is still not proof a request reached the
/// server. `response_ready` is not a socket write or a CPU stop receipt.
pub struct RequestTrace {
    trace: Arc<Trace>,
    rid: String,
    responded: bool,
}

impl RequestTrace {
    pub fn new(trace: Arc<Trace>, rid: &str) -> Option<Self> {
        if !trace.enabled {
            return None;
        }
        let value = Self {
            trace,
            rid: rid.to_owned(),
            responded: false,
        };
        value.note("http_observed", "pending", None);
        Some(value)
    }

    pub fn note(&self, event: &'static str, outcome: &'static str, status: Option<u16>) {
        self.trace.request_event(&self.rid, event, outcome, status);
    }

    pub fn response_ready(&mut self, status: u16) {
        if !self.responded {
            self.responded = true;
            self.note("http_response_ready", "response_ready", Some(status));
        }
    }
}

impl Drop for RequestTrace {
    fn drop(&mut self) {
        if !self.responded {
            self.note("http_future_dropped", "future_dropped", None);
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn monotonic_ns() -> Option<u64> {
    use std::os::raw::{c_int, c_long};
    #[repr(C)]
    struct Timespec {
        sec: c_long,
        nsec: c_long,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: c_int, output: *mut Timespec) -> c_int;
    }
    #[cfg(target_os = "macos")]
    const CLOCK: c_int = 6;
    #[cfg(target_os = "linux")]
    const CLOCK: c_int = 1;
    let mut value = Timespec { sec: 0, nsec: 0 };
    if unsafe { clock_gettime(CLOCK, &mut value) } != 0
        || value.sec < 0
        || !(0..1_000_000_000).contains(&value.nsec)
    {
        return None;
    }
    (value.sec as u64)
        .checked_mul(1_000_000_000)?
        .checked_add(value.nsec as u64)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn monotonic_ns() -> Option<u64> {
    None
}

const PENDING: usize = 0;
const COMPLETE: usize = 1;
const CANCELLED: usize = 2;
const EXPIRED: usize = 3;
const ABANDONED: usize = 4;
fn outcome_name(value: usize) -> &'static str {
    match value {
        PENDING => "pending",
        COMPLETE => "complete",
        CANCELLED => "cancelled",
        EXPIRED => "deadline",
        ABANDONED => "future_dropped",
        _ => "unknown",
    }
}
struct Control {
    cancel: AtomicBool,
    started: AtomicBool,
    iteration: AtomicUsize,
    outcome: AtomicUsize,
    rid: String,
    trace: Arc<Trace>,
    resource_counts: Option<(Arc<Semaphore>, Arc<AtomicUsize>)>,
}
impl Control {
    fn new(rid: &str, trace: Arc<Trace>) -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicBool::new(false),
            started: AtomicBool::new(false),
            iteration: AtomicUsize::new(0),
            outcome: AtomicUsize::new(PENDING),
            rid: if trace.enabled {
                rid.to_owned()
            } else {
                String::new()
            },
            trace,
            resource_counts: None,
        })
    }
    fn note(&self, event: &'static str) {
        self.trace.record(self, event);
    }
    fn finish(&self, outcome: usize) {
        if self.trace.enabled {
            self.outcome.store(outcome, Ordering::Release);
        }
    }
    fn start(&self) {
        if self.trace.enabled {
            self.started.store(true, Ordering::Release);
        }
        self.note("started");
    }
}
struct CancelOnDrop {
    control: Arc<Control>,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel.store(true, Ordering::Release);
            self.control.note("cancel_observed");
        }
    }
}
struct Resources {
    permit: Option<OwnedSemaphorePermit>,
    actual: Arc<AtomicUsize>,
    control: Arc<Control>,
}
impl Resources {
    fn new(actual: Arc<AtomicUsize>, control: Arc<Control>) -> Self {
        actual.fetch_add(1, Ordering::Relaxed);
        Self {
            permit: None,
            actual,
            control,
        }
    }
}
impl Drop for Resources {
    fn drop(&mut self) {
        if self.control.trace.enabled {
            let _ = self.control.outcome.compare_exchange(
                PENDING,
                ABANDONED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.control.note("stopped");
        drop(self.permit.take());
        self.actual.fetch_sub(1, Ordering::Relaxed);
        self.control.note("reusable");
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    QueueFull,
    Closed,
    Cancelled,
    Deadline,
    Worker,
}
fn check(control: &Control, deadline: Instant) -> Result<(), Failure> {
    if control.cancel.load(Ordering::Acquire) {
        control.finish(CANCELLED);
        return Err(Failure::Cancelled);
    }
    if Instant::now() >= deadline {
        control.finish(EXPIRED);
        return Err(Failure::Deadline);
    }
    Ok(())
}
fn step(total: &mut i64, from: usize, to: usize, seed: i64) {
    for index in from..to {
        *total = (*total * 33 + index as i64 + seed) % 100_000_000;
    }
}
fn compute_sync(
    control: &Control,
    iterations: usize,
    seed: i64,
    chunk: usize,
    deadline: Instant,
) -> Result<i64, Failure> {
    let mut total = seed;
    let mut index = 0;
    while index < iterations {
        check(control, deadline)?;
        let end = index.saturating_add(chunk).min(iterations);
        step(&mut total, index, end, seed);
        index = end;
        if control.trace.enabled {
            control.iteration.store(index, Ordering::Relaxed);
        }
    }
    check(control, deadline)?;
    control.finish(COMPLETE);
    Ok(total)
}
async fn compute_cooperative(
    control: &Control,
    iterations: usize,
    seed: i64,
    chunk: usize,
    deadline: Instant,
) -> Result<i64, Failure> {
    let mut total = seed;
    let mut index = 0;
    while index < iterations {
        check(control, deadline)?;
        let end = index.saturating_add(chunk).min(iterations);
        step(&mut total, index, end, seed);
        index = end;
        if control.trace.enabled {
            control.iteration.store(index, Ordering::Relaxed);
        }
        if index < iterations {
            tokio::task::yield_now().await;
        }
    }
    check(control, deadline)?;
    control.finish(COMPLETE);
    Ok(total)
}

pub struct Input<'a> {
    pub rid: &'a str,
    pub iterations: usize,
    pub seed: i64,
    pub deadline: Instant,
    pub config: Config,
    pub cpu: Arc<Semaphore>,
    pub pending: Arc<Semaphore>,
    pub actual: Arc<AtomicUsize>,
    pub trace: Arc<Trace>,
}
pub async fn run(input: Input<'_>) -> Result<i64, Failure> {
    let queue = match input.pending.try_acquire_owned() {
        Ok(queue) => queue,
        Err(_) => {
            input
                .trace
                .request_event(input.rid, "cpu_queue_rejected", "queue_full", Some(503));
            return Err(Failure::QueueFull);
        }
    };
    let mut control = Control::new(input.rid, input.trace);
    if control.trace.enabled {
        Arc::get_mut(&mut control)
            .expect("unshared new control")
            .resource_counts = Some((input.cpu.clone(), input.actual.clone()));
    }
    let mut cancel = CancelOnDrop {
        control: control.clone(),
        armed: true,
    };
    let mut resources = Resources::new(input.actual, control.clone());
    control.note("queued");
    resources.permit = Some(
        input
            .cpu
            .acquire_owned()
            .await
            .map_err(|_| Failure::Closed)?,
    );
    drop(queue);
    let result = match input.config.mode {
        Mode::Cooperative => {
            let _resources = resources;
            control.start();
            compute_cooperative(
                &control,
                input.iterations,
                input.seed,
                input.config.chunk,
                input.deadline,
            )
            .await
        }
        Mode::Pool => {
            let worker_control = control.clone();
            tokio::task::spawn_blocking(move || {
                let _resources = resources;
                worker_control.start();
                compute_sync(
                    &worker_control,
                    input.iterations,
                    input.seed,
                    input.config.chunk,
                    input.deadline,
                )
            })
            .await
            .map_err(|_| Failure::Worker)?
        }
        Mode::Legacy => return Err(Failure::Worker),
    };
    cancel.armed = false;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn final_trace_keeps_late_cpu_and_http_drop_after_an_empty_prefix() {
        for mode in [Mode::Cooperative, Mode::Pool] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let trace = Arc::new(Trace::new(true));
            let actual = Arc::new(AtomicUsize::new(0));
            let cpu = Arc::new(Semaphore::new(1));
            let pending = Arc::new(Semaphore::new(1));
            let prefix = trace.snapshot();
            assert_eq!(prefix["last_sequence"], 0);
            assert_eq!(prefix["pending"], 0);
            assert_eq!(prefix["dropped"], 0);
            assert_eq!(actual.load(Ordering::Relaxed), 0);
            // An earlier empty page is not a lifetime receipt: real work may
            // still enter later and must appear in the final suffix.
            let value = runtime
                .block_on(run(Input {
                    rid: "late-cpu",
                    iterations: 1024,
                    seed: 3,
                    deadline: Instant::now() + Duration::from_secs(3),
                    config: Config {
                        mode,
                        chunk: 128,
                        event_interval: None,
                        trace: true,
                    },
                    cpu: cpu.clone(),
                    pending: pending.clone(),
                    actual: actual.clone(),
                    trace: trace.clone(),
                }))
                .unwrap();
            assert_eq!(value, crate::compute(1024, 3));
            let request = RequestTrace::new(trace.clone(), "late-http-drop").unwrap();
            drop(runtime.spawn(async move {
                let _request = request;
                std::future::pending::<()>().await;
            }));
            let receipt = FinalReceipt::after_graceful(trace, actual, mode);
            drop(runtime); // Drops the queued HTTP future, joins blocking CPU.
            let final_trace = receipt.seal_after_runtime();
            assert!(final_trace.sealed && final_trace.complete && final_trace.sequence_complete);
            assert!(final_trace.cpu_entry_trace_covered);
            assert_eq!(final_trace.cpu_mode, mode.name());
            assert!(final_trace.graceful_complete && final_trace.runtime_drop_completed);
            assert_eq!(final_trace.actual_cpu_jobs, 0);
            assert_eq!(final_trace.dropped, 0);
            assert_eq!(final_trace.prefix_drained_events, 0);
            for name in ["queued", "started", "stopped", "reusable"] {
                assert!(final_trace
                    .events
                    .iter()
                    .any(|row| row.rid == "late-cpu" && row.event == name));
            }
            assert!(final_trace
                .events
                .iter()
                .any(|row| { row.rid == "late-http-drop" && row.event == "http_future_dropped" }));
            for (index, row) in final_trace.events.iter().enumerate() {
                assert_eq!(row.sequence, index as u64 + 1);
            }
            assert_eq!(cpu.available_permits(), 1);
            assert_eq!(pending.available_permits(), 1);
        }
    }

    #[test]
    fn final_trace_seals_zero_jobs_and_drops_without_polling_a_cpu_future() {
        for mode in [Mode::Cooperative, Mode::Pool] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let trace = Arc::new(Trace::new(true));
            let actual = Arc::new(AtomicUsize::new(0));
            let cpu = Arc::new(Semaphore::new(1));
            let pending = Arc::new(Semaphore::new(1));
            let mut request = RequestTrace::new(trace.clone(), "prefix-only").unwrap();
            request.response_ready(503);
            drop(request);
            let prefix = trace.snapshot();
            assert_eq!(prefix["events"].as_array().unwrap().len(), 2);
            // Current-thread runtime is never driven again. Constructing or
            // dropping an unpolled async run must not claim a CPU entry.
            drop(runtime.spawn(run(Input {
                rid: "never-polled",
                iterations: 1024,
                seed: 3,
                deadline: Instant::now() + Duration::from_secs(3),
                config: Config {
                    mode,
                    chunk: 128,
                    event_interval: None,
                    trace: true,
                },
                cpu: cpu.clone(),
                pending: pending.clone(),
                actual: actual.clone(),
                trace: trace.clone(),
            })));
            let receipt = FinalReceipt::after_graceful(trace, actual, mode);
            drop(runtime);
            let final_trace = receipt.seal_after_runtime();
            assert!(final_trace.sealed && final_trace.complete && final_trace.sequence_complete);
            assert!(final_trace.cpu_entry_trace_covered);
            assert_eq!(final_trace.cpu_mode, mode.name());
            assert_eq!(final_trace.actual_cpu_jobs, 0);
            assert_eq!(final_trace.dropped, 0);
            assert_eq!(final_trace.final_sequence, 2);
            assert_eq!(final_trace.prefix_drained_events, 2);
            assert_eq!(final_trace.prefix_drained_through_sequence, 2);
            assert_eq!(final_trace.remaining_events, 0);
            assert!(final_trace.events.is_empty());
            assert_eq!(final_trace.trace_arc_owners, 1);
            assert_eq!(final_trace.actual_counter_arc_owners, 1);
            assert_eq!(cpu.available_permits(), 1);
            assert_eq!(pending.available_permits(), 1);
        }
    }

    #[test]
    fn http_trace_distinguishes_response_from_future_drop_without_cpu_receipt() {
        assert!(RequestTrace::new(Arc::new(Trace::new(false)), "1").is_none());
        let trace = Arc::new(Trace::new(true));
        {
            let mut request = RequestTrace::new(trace.clone(), "1").unwrap();
            request.response_ready(503);
            request.response_ready(200); // A second call cannot forge another response.
        }
        drop(RequestTrace::new(trace.clone(), "2").unwrap());
        let snapshot = trace.snapshot();
        let rows = snapshot["events"].as_array().unwrap();
        let events: Vec<_> = rows
            .iter()
            .map(|row| row["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            events,
            [
                "http_observed",
                "http_response_ready",
                "http_observed",
                "http_future_dropped"
            ]
        );
        assert_eq!(rows[1]["http_status"], 503);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["sequence"], (index + 1) as u64);
            assert!(row["cpu_permits_available"].is_null());
            assert!(row["actual_cpu_jobs"].is_null());
        }
        assert_eq!(snapshot["dropped"], 0);
    }

    #[test]
    fn rejected_cpu_queue_is_visible_without_claiming_resources_existed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let trace = Arc::new(Trace::new(true));
            let cpu = Arc::new(Semaphore::new(1));
            let actual = Arc::new(AtomicUsize::new(0));
            assert_eq!(
                run(Input {
                    rid: "3",
                    iterations: 512000,
                    seed: 3,
                    deadline: Instant::now() + Duration::from_secs(3),
                    config: Config {
                        mode: Mode::Pool,
                        chunk: 512,
                        event_interval: None,
                        trace: true
                    },
                    cpu: cpu.clone(),
                    pending: Arc::new(Semaphore::new(0)),
                    actual: actual.clone(),
                    trace: trace.clone(),
                })
                .await,
                Err(Failure::QueueFull)
            );
            assert_eq!(cpu.available_permits(), 1);
            assert_eq!(actual.load(Ordering::Relaxed), 0);
            let snapshot = trace.snapshot();
            let rows = snapshot["events"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["event"], "cpu_queue_rejected");
            assert_eq!(rows[0]["http_status"], 503);
        });
    }

    #[test]
    fn dropped_real_queued_future_releases_only_its_own_resources() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let cpu = Arc::new(Semaphore::new(1));
            let held = cpu.clone().acquire_owned().await.unwrap();
            let pending = Arc::new(Semaphore::new(1));
            let actual = Arc::new(AtomicUsize::new(0));
            let trace = Arc::new(Trace::new(true));
            let task = tokio::spawn(run(Input {
                rid: "4",
                iterations: 512000,
                seed: 3,
                deadline: Instant::now() + Duration::from_secs(3),
                config: Config {
                    mode: Mode::Pool,
                    chunk: 512,
                    event_interval: None,
                    trace: true,
                },
                cpu: cpu.clone(),
                pending: pending.clone(),
                actual: actual.clone(),
                trace: trace.clone(),
            }));
            tokio::time::timeout(Duration::from_secs(1), async {
                while actual.load(Ordering::Relaxed) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(actual.load(Ordering::Relaxed), 0);
            assert_eq!(pending.available_permits(), 1);
            assert_eq!(cpu.available_permits(), 0); // The unrelated holder still owns it.
            drop(held);
            assert_eq!(cpu.available_permits(), 1);
            let snapshot = trace.snapshot();
            let rows = snapshot["events"].as_array().unwrap();
            assert!(!rows.iter().any(|row| row["event"] == "started"));
            let stopped = rows.iter().find(|row| row["event"] == "stopped").unwrap();
            let reusable = rows.iter().find(|row| row["event"] == "reusable").unwrap();
            assert_eq!(stopped["iteration"], 0);
            assert_eq!(stopped["started"], false);
            assert!(stopped["sequence"].as_u64().unwrap() < reusable["sequence"].as_u64().unwrap());
        });
    }
    #[test]
    fn frozen_kernel_chunks_keep_checksum_and_cancel_before_work() {
        for chunk in [512, 4096] {
            let control = Control::new("1", Arc::new(Trace::new(false)));
            assert_eq!(
                compute_sync(
                    &control,
                    512000,
                    3,
                    chunk,
                    Instant::now() + Duration::from_secs(3)
                ),
                Ok(22944003)
            );
            assert_eq!(
                compute_sync(&control, 512000, 3, chunk, Instant::now()),
                Err(Failure::Deadline)
            );
            control.cancel.store(true, Ordering::Release);
            assert_eq!(
                compute_sync(
                    &control,
                    512000,
                    3,
                    chunk,
                    Instant::now() + Duration::from_secs(3)
                ),
                Err(Failure::Cancelled)
            );
        }
    }
    #[test]
    fn cooperative_chunks_keep_frozen_checksum_and_release_resources() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for chunk in [512, 4096] {
                let pool = Arc::new(Semaphore::new(1));
                let pending = Arc::new(Semaphore::new(1));
                let actual = Arc::new(AtomicUsize::new(0));
                let result = run(Input {
                    rid: "3",
                    iterations: 512000,
                    seed: 3,
                    deadline: Instant::now() + Duration::from_secs(3),
                    config: Config {
                        mode: Mode::Cooperative,
                        chunk,
                        event_interval: None,
                        trace: false,
                    },
                    cpu: pool.clone(),
                    pending: pending.clone(),
                    actual: actual.clone(),
                    trace: Arc::new(Trace::new(false)),
                })
                .await;
                assert_eq!(result, Ok(22944003));
                assert_eq!(pool.available_permits(), 1);
                assert_eq!(pending.available_permits(), 1);
                assert_eq!(actual.load(Ordering::Relaxed), 0);
            }
        });
    }
    #[test]
    fn dropped_waiter_does_not_return_running_worker_permit() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let pool = Arc::new(Semaphore::new(1));
            let actual = Arc::new(AtomicUsize::new(0));
            let control = Control::new("2", Arc::new(Trace::new(true)));
            let mut resources = Resources::new(actual.clone(), control.clone());
            resources.permit = Some(pool.clone().acquire_owned().await.unwrap());
            let guard = CancelOnDrop {
                control: control.clone(),
                armed: true,
            };
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let worker_control = control.clone();
            let worker = tokio::task::spawn_blocking(move || {
                let _resources = resources;
                worker_control.start();
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(2));
                compute_sync(
                    &worker_control,
                    512000,
                    3,
                    512,
                    Instant::now() + Duration::from_secs(3),
                )
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(guard);
            assert_eq!(actual.load(Ordering::Relaxed), 1);
            assert_eq!(pool.available_permits(), 0);
            release_tx.send(()).unwrap();
            assert_eq!(worker.await.unwrap(), Err(Failure::Cancelled));
            assert_eq!(actual.load(Ordering::Relaxed), 0);
            assert_eq!(pool.available_permits(), 1);
            let trace = control.trace.snapshot();
            assert_eq!(trace["dropped"], 0);
            let rows = trace["events"].as_array().unwrap();
            assert_eq!(rows[rows.len() - 2]["event"], "stopped");
            assert_eq!(rows[rows.len() - 1]["event"], "reusable");
        });
    }
}
