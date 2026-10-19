//! Opt-in, bounded request timeline. These clocks never control a deadline.
use std::{
    cell::RefCell,
    io::{self, Write},
    sync::{
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

const STAGES: usize = 20;
const MAX_SNAPSHOTS: usize = 32;
const CPU_SAMPLE_INTERVAL_NS: u64 = 1_000_000;
const PROGRESS_GAP_NS: u64 = 50_000_000;

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(super) enum Stage {
    Accepted,
    SlotPromoted,
    ReadStarted,
    Ready,
    EnqueueAttempt,
    Enqueued,
    Dequeued,
    ActualHandlerStart,
    QueueExpired,
    InterpreterExpired,
    ReactorExpired,
    Finished,
    Published,
    Observed,
    WriteStart,
    WriteEnd,
    LateReply,
    SlowResponse,
    WriteFailed,
    WriteExpired,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::SlotPromoted => "slot_promoted",
            Self::ReadStarted => "read_started",
            Self::Ready => "ready",
            Self::EnqueueAttempt => "enqueue_attempt",
            // The receiver may consume a job before try_send returns. This is
            // an upper bound on acceptance, not the queue's insertion instant.
            Self::Enqueued => "enqueue_returned",
            Self::Dequeued => "dequeued",
            Self::ActualHandlerStart => "actual_handler_start",
            Self::QueueExpired => "queue_expired",
            Self::InterpreterExpired => "interpreter_expired",
            Self::ReactorExpired => "reactor_expired",
            Self::Finished => "finished",
            Self::Published => "published",
            Self::Observed => "observed",
            Self::WriteStart => "write_start",
            Self::WriteEnd => "write_end",
            Self::LateReply => "late_reply",
            Self::SlowResponse => "slow_response",
            Self::WriteFailed => "write_failed",
            Self::WriteExpired => "write_expired",
        }
    }
}

const ALL_STAGES: [Stage; STAGES] = [
    Stage::Accepted,
    Stage::SlotPromoted,
    Stage::ReadStarted,
    Stage::Ready,
    Stage::EnqueueAttempt,
    Stage::Enqueued,
    Stage::Dequeued,
    Stage::ActualHandlerStart,
    Stage::QueueExpired,
    Stage::InterpreterExpired,
    Stage::ReactorExpired,
    Stage::Finished,
    Stage::Published,
    Stage::Observed,
    Stage::WriteStart,
    Stage::WriteEnd,
    Stage::LateReply,
    Stage::SlowResponse,
    Stage::WriteFailed,
    Stage::WriteExpired,
];

#[derive(Default)]
struct Selection {
    first_anomaly: AtomicU64,
}

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("KU_HTTP_FLIGHT_RECORD").is_some_and(|v| v == "1"))
}

pub(super) struct FlightRecord {
    sequence: u64,
    request_id: Option<u64>,
    anchor: Instant,
    anchor_ns: Option<u64>,
    clock_pair_error_ns: u64,
    deadline: Instant,
    planned_deadline_ns: u64,
    stages: [AtomicU64; STAGES],
    recorded_stages: AtomicU32,
    handler_tid: AtomicU64,
    progress_count: AtomicU64,
    last_progress_ns: AtomicU64,
    previous_poll_ns: AtomicU64,
    last_pre_deadline_poll_ns: AtomicU64,
    cross_deadline_gap_start_ns: AtomicU64,
    cross_deadline_gap_end_ns: AtomicU64,
    gap_start_ns: AtomicU64,
    gap_end_ns: AtomicU64,
    cpu_first_ns: AtomicU64,
    cpu_last_ns: AtomicU64,
    cpu_sample_at_ns: AtomicU64,
    cpu_samples: AtomicU64,
    snapshots: AtomicUsize,
    selection: Arc<Selection>,
}

impl FlightRecord {
    pub(super) fn new(request_id: Option<u64>, ready: Instant, deadline: Instant) -> Arc<Self> {
        static SELECTION: OnceLock<Arc<Selection>> = OnceLock::new();
        Self::with_selection(
            request_id,
            ready,
            deadline,
            Arc::clone(SELECTION.get_or_init(|| Arc::new(Selection::default()))),
        )
    }

    fn with_selection(
        request_id: Option<u64>,
        ready: Instant,
        deadline: Instant,
        selection: Arc<Selection>,
    ) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        // The midpoint only aligns diagnostic clocks. Real HTTP control keeps
        // using its original Instant; the measured pairing window is reported.
        let before = Instant::now();
        let anchor_ns = monotonic_ns();
        let after = Instant::now();
        let window = after.saturating_duration_since(before);
        let anchor = before + window / 2;
        let mut record = Self {
            sequence: NEXT.fetch_add(1, Ordering::Relaxed),
            request_id,
            anchor,
            anchor_ns,
            clock_pair_error_ns: nanos(window),
            deadline,
            planned_deadline_ns: 0,
            stages: std::array::from_fn(|_| AtomicU64::new(0)),
            recorded_stages: AtomicU32::new(0),
            handler_tid: AtomicU64::new(0),
            progress_count: AtomicU64::new(0),
            last_progress_ns: AtomicU64::new(0),
            previous_poll_ns: AtomicU64::new(0),
            last_pre_deadline_poll_ns: AtomicU64::new(0),
            cross_deadline_gap_start_ns: AtomicU64::new(0),
            cross_deadline_gap_end_ns: AtomicU64::new(0),
            gap_start_ns: AtomicU64::new(0),
            gap_end_ns: AtomicU64::new(0),
            cpu_first_ns: AtomicU64::new(0),
            cpu_last_ns: AtomicU64::new(0),
            cpu_sample_at_ns: AtomicU64::new(0),
            cpu_samples: AtomicU64::new(0),
            snapshots: AtomicUsize::new(0),
            selection,
        };
        record.planned_deadline_ns = record.at_ns(deadline);
        record.mark_at(Stage::Ready, ready);
        Arc::new(record)
    }

    fn at_ns(&self, at: Instant) -> u64 {
        let Some(anchor_ns) = self.anchor_ns else {
            return 0;
        };
        if at >= self.anchor {
            anchor_ns.saturating_add(nanos(at.duration_since(self.anchor)))
        } else {
            anchor_ns.saturating_sub(nanos(self.anchor.duration_since(at)))
        }
    }

    pub(super) fn mark(&self, stage: Stage) {
        self.mark_at(stage, Instant::now());
    }

    // Diagnostic classification only; this must not extend, replace or decide
    // the transport/handler's actual timeout response.
    pub(super) fn past_planned_deadline(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    pub(super) fn mark_at(&self, stage: Stage, at: Instant) {
        if !self.record_stage(stage, at) {
            return;
        }
        if matches!(stage, Stage::ActualHandlerStart) {
            self.handler_tid.store(kernel_tid(), Ordering::Relaxed);
            self.sample_cpu(self.at_ns(at));
        }
    }

    pub(super) fn progress(&self, now: Instant) {
        // This is a poll checkpoint. It does not establish that user code made
        // semantic progress, nor identify what happened between checkpoints.
        let stamp = self.at_ns(now);
        let previous = self.last_progress_ns.swap(stamp, Ordering::Relaxed);
        self.previous_poll_ns.store(previous, Ordering::Relaxed);
        self.progress_count.fetch_add(1, Ordering::Relaxed);
        if now < self.deadline {
            self.last_pre_deadline_poll_ns
                .store(stamp, Ordering::Relaxed);
        } else if previous > 0
            && previous < self.planned_deadline_ns
            && self
                .cross_deadline_gap_start_ns
                .compare_exchange(0, previous, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.cross_deadline_gap_end_ns
                .store(stamp, Ordering::Release);
        }
        if previous > 0
            && stamp.saturating_sub(previous) >= PROGRESS_GAP_NS
            && self
                .gap_start_ns
                .compare_exchange(0, previous, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.gap_end_ns.store(stamp, Ordering::Release);
        }
        self.sample_cpu(stamp);
    }

    fn sample_cpu(&self, stamp: u64) {
        let previous_sample = self.cpu_sample_at_ns.load(Ordering::Relaxed);
        if stamp > 0
            && (previous_sample == 0
                || stamp.saturating_sub(previous_sample) >= CPU_SAMPLE_INTERVAL_NS)
            && self
                .cpu_sample_at_ns
                .compare_exchange(previous_sample, stamp, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            if let Some(cpu) = thread_cpu_ns() {
                let _ = self.cpu_first_ns.compare_exchange(
                    0,
                    cpu,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                self.cpu_last_ns.store(cpu, Ordering::Relaxed);
                self.cpu_samples.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn expire(&self, stage: Stage) {
        self.expire_at(stage, Instant::now());
    }

    pub(super) fn expire_at(&self, stage: Stage, now: Instant) {
        self.observe_anomaly_at(stage, now);
    }

    pub(super) fn observe_anomaly_at(&self, stage: Stage, now: Instant) {
        if !self.claim_anomaly(stage, now) {
            return;
        }
        if let Some(snapshot) = self.snapshot(stage) {
            super::http_io::emit_flight(snapshot);
        }
    }

    fn claim_anomaly(&self, stage: Stage, now: Instant) -> bool {
        // A repeated cleanup poll does not repeatedly emit the same boundary.
        if !self.record_stage(stage, now) {
            return false;
        }
        let _ = self.selection.first_anomaly.compare_exchange(
            0,
            self.sequence,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.selection.first_anomaly.load(Ordering::Acquire) == self.sequence
    }

    fn record_stage(&self, stage: Stage, now: Instant) -> bool {
        let bit = 1_u32 << stage as usize;
        // The bit also distinguishes an observed event from an unavailable
        // clock, whose timestamp is deliberately zero on non-Unix targets.
        if self.recorded_stages.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
            return false;
        }
        self.stages[stage as usize].store(self.at_ns(now), Ordering::Release);
        true
    }

    pub(super) fn snapshot(&self, reason: Stage) -> Option<Snapshot> {
        if self.selection.first_anomaly.load(Ordering::Acquire) != self.sequence
            || self
                .snapshots
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < MAX_SNAPSHOTS).then_some(n + 1)
                })
                .is_err()
        {
            return None;
        }
        let capture_before = Instant::now();
        let captured_ns = monotonic_ns().unwrap_or(0);
        let capture_after = Instant::now();
        let capture_window = capture_after.saturating_duration_since(capture_before);
        let mapped_capture_ns = self.at_ns(capture_before + capture_window / 2);
        Some(Snapshot {
            sequence: self.sequence,
            request_id: self.request_id,
            reason,
            captured_ns,
            mapped_capture_ns,
            clock_map_error_ns: captured_ns.abs_diff(mapped_capture_ns),
            capture_pair_error_ns: nanos(capture_window),
            clock_monotonic: self.anchor_ns.is_some(),
            clock_pair_error_ns: self.clock_pair_error_ns,
            planned_deadline_ns: self.planned_deadline_ns,
            stages: std::array::from_fn(|i| self.stages[i].load(Ordering::Acquire)),
            handler_tid: self.handler_tid.load(Ordering::Relaxed),
            progress_count: self.progress_count.load(Ordering::Relaxed),
            last_progress_ns: self.last_progress_ns.load(Ordering::Relaxed),
            previous_poll_ns: self.previous_poll_ns.load(Ordering::Relaxed),
            last_pre_deadline_poll_ns: self.last_pre_deadline_poll_ns.load(Ordering::Relaxed),
            cross_deadline_gap_start_ns: self.cross_deadline_gap_start_ns.load(Ordering::Relaxed),
            cross_deadline_gap_end_ns: self.cross_deadline_gap_end_ns.load(Ordering::Acquire),
            gap_start_ns: self.gap_start_ns.load(Ordering::Relaxed),
            gap_end_ns: self.gap_end_ns.load(Ordering::Acquire),
            cpu_first_ns: self.cpu_first_ns.load(Ordering::Relaxed),
            cpu_last_ns: self.cpu_last_ns.load(Ordering::Relaxed),
            cpu_sample_at_ns: self.cpu_sample_at_ns.load(Ordering::Relaxed),
            cpu_samples: self.cpu_samples.load(Ordering::Relaxed),
        })
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<FlightRecord>>> = const { RefCell::new(None) };
}

pub(super) fn current() -> Option<Arc<FlightRecord>> {
    CURRENT.with(|current| current.borrow().clone())
}

pub(super) struct FlightGuard(Option<Arc<FlightRecord>>);

impl FlightGuard {
    pub(super) fn enter(record: Option<Arc<FlightRecord>>) -> Self {
        Self(CURRENT.with(|current| current.replace(record)))
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            current.replace(self.0.take());
        });
    }
}

#[derive(Clone, Copy)]
pub(super) struct Snapshot {
    sequence: u64,
    request_id: Option<u64>,
    reason: Stage,
    captured_ns: u64,
    mapped_capture_ns: u64,
    clock_map_error_ns: u64,
    capture_pair_error_ns: u64,
    clock_monotonic: bool,
    clock_pair_error_ns: u64,
    planned_deadline_ns: u64,
    stages: [u64; STAGES],
    handler_tid: u64,
    progress_count: u64,
    last_progress_ns: u64,
    previous_poll_ns: u64,
    last_pre_deadline_poll_ns: u64,
    cross_deadline_gap_start_ns: u64,
    cross_deadline_gap_end_ns: u64,
    gap_start_ns: u64,
    gap_end_ns: u64,
    cpu_first_ns: u64,
    cpu_last_ns: u64,
    cpu_sample_at_ns: u64,
    cpu_samples: u64,
}

impl Snapshot {
    pub(super) fn write(&self, output: &mut impl Write) -> io::Result<()> {
        write!(
            output,
            "ku_http_flight sequence={} request_id=",
            self.sequence
        )?;
        match self.request_id {
            Some(id) => write!(output, "{id}")?,
            None => write!(output, "unavailable")?,
        }
        write!(output, " reason={} captured_ns={} mapped_capture_ns={} clock_map_error_ns={} capture_pair_error_ns={} clock_monotonic={} clock_pair_error_ns={} planned_deadline_ns={} handler_tid={} poll_count={} last_poll_ns={} previous_poll_ns={} last_pre_deadline_poll_ns={} cross_deadline_gap_start_ns={} cross_deadline_gap_end_ns={} first_poll_gap_start_ns={} first_poll_gap_end_ns={} handler_cpu_first_ns={} handler_cpu_last_ns={} handler_cpu_sample_at_ns={} handler_cpu_samples={}",
            self.reason.name(), self.captured_ns, self.mapped_capture_ns, self.clock_map_error_ns,
            self.capture_pair_error_ns, self.clock_monotonic, self.clock_pair_error_ns,
            self.planned_deadline_ns, self.handler_tid, self.progress_count, self.last_progress_ns,
            self.previous_poll_ns, self.last_pre_deadline_poll_ns,
            self.cross_deadline_gap_start_ns, self.cross_deadline_gap_end_ns,
            self.gap_start_ns, self.gap_end_ns, self.cpu_first_ns, self.cpu_last_ns,
            self.cpu_sample_at_ns, self.cpu_samples)?;
        for stage in ALL_STAGES {
            write!(
                output,
                " {}_ns={}",
                stage.name(),
                self.stages[stage as usize]
            )?;
        }
        writeln!(output)
    }
}

fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

#[cfg(unix)]
fn read_clock(clock: libc::clockid_t) -> Option<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(clock, &mut value) } != 0 {
        return None;
    }
    let seconds = u64::try_from(value.tv_sec).ok()?;
    let fraction = u64::try_from(value.tv_nsec)
        .ok()
        .filter(|v| *v < 1_000_000_000)?;
    seconds.checked_mul(1_000_000_000)?.checked_add(fraction)
}

#[cfg(unix)]
fn monotonic_ns() -> Option<u64> {
    read_clock(libc::CLOCK_MONOTONIC)
}
#[cfg(not(unix))]
fn monotonic_ns() -> Option<u64> {
    None
}
#[cfg(unix)]
fn thread_cpu_ns() -> Option<u64> {
    read_clock(libc::CLOCK_THREAD_CPUTIME_ID)
}
#[cfg(not(unix))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn kernel_tid() -> u64 {
    let mut id = 0;
    if unsafe { libc::pthread_threadid_np(Default::default(), &mut id) } == 0 {
        id
    } else {
        0
    }
}
#[cfg(target_os = "linux")]
fn kernel_tid() -> u64 {
    u64::try_from(unsafe { libc::syscall(libc::SYS_gettid) }).unwrap_or(0)
}
#[cfg(windows)]
fn kernel_tid() -> u64 {
    u64::from(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() })
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn kernel_tid() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_timeline_records_original_deadline_and_only_first_observation() {
        let ready = Instant::now();
        let record = FlightRecord::with_selection(
            Some(42),
            ready,
            ready + Duration::from_millis(100),
            Arc::new(Selection::default()),
        );
        record.mark_at(Stage::EnqueueAttempt, ready + Duration::from_millis(1));
        record.mark_at(Stage::Enqueued, ready + Duration::from_millis(2));
        record.mark_at(Stage::Dequeued, ready + Duration::from_millis(3));
        record.mark_at(Stage::ActualHandlerStart, ready + Duration::from_millis(4));
        record.mark_at(Stage::Dequeued, ready + Duration::from_millis(9));
        record.progress(ready + Duration::from_millis(10));
        record.progress(ready + Duration::from_millis(90));
        record.progress(ready + Duration::from_millis(110));
        record.progress(ready + Duration::from_millis(120));
        assert!(!record.past_planned_deadline(ready + Duration::from_millis(99)));
        assert!(record.past_planned_deadline(ready + Duration::from_millis(100)));
        record
            .selection
            .first_anomaly
            .store(record.sequence, Ordering::Release);
        let snapshot = record.snapshot(Stage::InterpreterExpired).unwrap();
        if snapshot.clock_monotonic {
            assert_eq!(
                snapshot.planned_deadline_ns - snapshot.stages[Stage::Ready as usize],
                100_000_000
            );
            assert_eq!(
                snapshot.stages[Stage::Dequeued as usize] - snapshot.stages[Stage::Ready as usize],
                3_000_000
            );
            let ready_ns = snapshot.stages[Stage::Ready as usize];
            assert_eq!(snapshot.gap_end_ns - snapshot.gap_start_ns, 80_000_000);
            assert_eq!(snapshot.previous_poll_ns - ready_ns, 110_000_000);
            assert_eq!(snapshot.last_pre_deadline_poll_ns - ready_ns, 90_000_000);
            assert_eq!(snapshot.cross_deadline_gap_start_ns - ready_ns, 90_000_000);
            assert_eq!(snapshot.cross_deadline_gap_end_ns - ready_ns, 110_000_000);
        }
        assert_eq!(snapshot.request_id, Some(42));
        assert_eq!(snapshot.progress_count, 4);
    }

    #[test]
    fn first_anomaly_selection_and_snapshot_allowance_are_bounded() {
        let selection = Arc::new(Selection::default());
        let ready = Instant::now();
        let first = FlightRecord::with_selection(None, ready, ready, Arc::clone(&selection));
        let second =
            FlightRecord::with_selection(Some(u64::MAX), ready, ready, Arc::clone(&selection));
        assert!(first.snapshot(Stage::Ready).is_none());
        // A slow successful transport may be selected without fabricating any
        // handler expiry; a later failure on another request cannot replace it.
        assert!(first.claim_anomaly(Stage::SlowResponse, ready));
        assert!(!second.claim_anomaly(Stage::WriteFailed, ready));
        assert!(!first.claim_anomaly(Stage::SlowResponse, ready));
        assert!(first.claim_anomaly(Stage::WriteExpired, ready));
        assert_eq!(
            first.recorded_stages.load(Ordering::Relaxed)
                & (1 << Stage::InterpreterExpired as usize),
            0
        );
        assert!(second.snapshot(Stage::Ready).is_none());
        for _ in 0..MAX_SNAPSHOTS {
            assert!(first.snapshot(Stage::Ready).is_some());
        }
        assert!(first.snapshot(Stage::Ready).is_none());
    }

    #[test]
    fn guard_restores_previous_record_and_output_is_numeric_only() {
        let ready = Instant::now();
        let record =
            FlightRecord::with_selection(Some(42), ready, ready, Arc::new(Selection::default()));
        assert!(current().is_none());
        {
            let _guard = FlightGuard::enter(Some(Arc::clone(&record)));
            assert!(Arc::ptr_eq(&current().unwrap(), &record));
            {
                let _inner = FlightGuard::enter(None);
                assert!(current().is_none());
            }
            assert!(Arc::ptr_eq(&current().unwrap(), &record));
        }
        assert!(current().is_none());
        record
            .selection
            .first_anomaly
            .store(record.sequence, Ordering::Release);
        let mut output = Vec::new();
        record
            .snapshot(Stage::Ready)
            .unwrap()
            .write(&mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("ku_http_flight sequence="));
        assert!(output.contains("request_id=42 reason=ready"));
        assert!(output.contains("planned_deadline_ns="));
        assert!(output.contains("enqueue_returned_ns="));
        assert!(output.contains("last_pre_deadline_poll_ns="));
        assert!(output.contains("cross_deadline_gap_start_ns="));
        assert!(output.contains("slow_response_ns="));
        assert!(output.contains("write_failed_ns="));
        assert!(output.contains("write_expired_ns="));
        assert!(!output.contains("progress_count="));
        assert!(!output.contains("/"));
    }
}
