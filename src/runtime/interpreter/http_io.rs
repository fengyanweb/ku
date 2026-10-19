//! One socket owner for the interpreter HTTP transport. Ku handlers and their
//! Values remain on the caller; only parsed/wire data crosses this boundary.

use super::http_flight::{self, FlightRecord, Stage};
use super::{
    encode_http_response, entry_span, finish_http_request, http_deadline, netpoll, parse_http_head,
    status_response, HttpConnectionPermit, HttpRejection, HttpServerRuntimeLimits, HttpSlowTrace,
    HttpSlowTraceSample, HttpWireResponse, ParsedHttpHead, ParsedHttpRequest,
    PendingHttpConnection, HTTP_ACCEPT_BATCH,
};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
        Arc, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const READ_CHUNK: usize = 8192;

// Requested backing capacities of transport-owned strings/bytes. Allocator
// metadata, map buckets, Ku values retained by user code and library internals
// are deliberately outside this counter. Control slots and the static 503
// response below are fixed overhead bounded by max_active_requests.
struct BufferBudget {
    limit: usize,
    current: AtomicUsize,
    peak: AtomicUsize,
    rejected: AtomicUsize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BufferSnapshot {
    pub(super) current: usize,
    pub(super) peak: usize,
    pub(super) rejected: usize,
    pub(super) limit: usize,
}

impl BufferBudget {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
        })
    }

    fn charge(self: &Arc<Self>, bytes: usize) -> Option<BufferCharge> {
        if let Ok(previous) =
            self.current
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(bytes)
                        .filter(|next| *next <= self.limit)
                })
        {
            self.peak.fetch_max(previous + bytes, Ordering::Relaxed);
            Some(BufferCharge {
                budget: Arc::clone(self),
                bytes,
            })
        } else {
            let _ = self
                .rejected
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(1))
                });
            None
        }
    }

    fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            current: self.current.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            limit: self.limit,
        }
    }
}

// Producers never perform diagnostic I/O. A single process-wide writer may
// wait for stderr, but fixed-size try_send events cannot park either HTTP actor.
// This thread is intentionally not joined: a stopped log consumer must not
// prevent HTTP teardown. The OS retires this one thread at process exit.
const TRACE_CHANNEL_CAPACITY: usize = 256;
const TRACE_TRANSPORT_LINES: usize = 256;
const TRACE_SNAPSHOT_LINES: usize = 512;
const TRACE_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct HttpTraceSink {
    sender: SyncSender<HttpTraceEvent>,
    state: Arc<HttpTraceSinkState>,
}

struct HttpTraceSinkState {
    slow: AtomicUsize,
    transport: AtomicUsize,
    snapshots: AtomicUsize,
    flight: AtomicUsize,
    dropped: AtomicUsize,
}

impl HttpTraceSink {
    fn channel() -> (Self, Receiver<HttpTraceEvent>) {
        let (sender, receiver) = mpsc::sync_channel(TRACE_CHANNEL_CAPACITY);
        (
            Self {
                sender,
                state: Arc::new(HttpTraceSinkState {
                    slow: AtomicUsize::new(0),
                    transport: AtomicUsize::new(0),
                    snapshots: AtomicUsize::new(0),
                    flight: AtomicUsize::new(0),
                    dropped: AtomicUsize::new(0),
                }),
            },
            receiver,
        )
    }

    fn process() -> Option<Self> {
        static SINK: OnceLock<Option<HttpTraceSink>> = OnceLock::new();
        SINK.get_or_init(|| {
            let (sink, receiver) = Self::channel();
            let mut output = trace_output()?;
            let state = Arc::clone(&sink.state);
            thread::Builder::new()
                .name("ku-http-trace".into())
                .spawn(move || {
                    for event in receiver {
                        // A duplicated OS handle avoids holding std's global
                        // stderr lock while the downstream consumer is stopped.
                        if event.write(&mut output).is_err() {
                            increment(&state.dropped);
                            break;
                        }
                    }
                })
                .ok()?;
            Some(sink)
        })
        .clone()
    }

    fn emit(&self, event: HttpTraceEvent) {
        let (counter, limit) = match &event {
            HttpTraceEvent::Slow { .. } => (&self.state.slow, HttpSlowTrace::MAX_LINES),
            HttpTraceEvent::Transport { .. } => (&self.state.transport, TRACE_TRANSPORT_LINES),
            HttpTraceEvent::Snapshot { .. } => (&self.state.snapshots, TRACE_SNAPSHOT_LINES),
            HttpTraceEvent::Flight(_) => (&self.state.flight, 32),
        };
        if counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < limit).then_some(count + 1)
            })
            .is_err()
        {
            increment(&self.state.dropped);
            return;
        }
        if self.sender.try_send(event).is_err() {
            increment(&self.state.dropped);
        }
    }
}

#[cfg(unix)]
fn trace_output() -> Option<std::fs::File> {
    use std::os::fd::AsFd;
    io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .map(Into::into)
}

#[cfg(windows)]
fn trace_output() -> Option<std::fs::File> {
    use std::os::windows::io::AsHandle;
    io::stderr()
        .as_handle()
        .try_clone_to_owned()
        .ok()
        .map(Into::into)
}

#[cfg(not(any(unix, windows)))]
fn trace_output() -> Option<std::fs::File> {
    None
}

#[derive(Default)]
struct HttpTraceCounters {
    queued: AtomicUsize,
    // Backing capacities of parsed input payloads retained by queued jobs;
    // excludes map buckets, user Task heaps and allocator metadata.
    queued_bytes: AtomicUsize,
    peak_queued: AtomicUsize,
    peak_queued_bytes: AtomicUsize,
    rejected_jobs: AtomicUsize,
    rejected_connections: AtomicUsize,
    running: AtomicUsize,
    completed: AtomicUsize,
    reactor_expired: AtomicUsize,
    queue_expired: AtomicUsize,
    handler_expired: AtomicUsize,
    write_expired: AtomicUsize,
    write_failed: AtomicUsize,
    late_replies: AtomicUsize,
}

fn increment(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

struct HttpTraceCredit {
    counters: Arc<HttpTraceCounters>,
    running: bool,
    bytes: usize,
}

impl HttpTraceCredit {
    fn acquire(counters: &Arc<HttpTraceCounters>, running: bool, bytes: usize) -> Self {
        let counter = if running {
            &counters.running
        } else {
            &counters.queued
        };
        let current = counter.fetch_add(1, Ordering::AcqRel) + 1;
        if !running {
            let queued_bytes = counters.queued_bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
            counters.peak_queued.fetch_max(current, Ordering::Relaxed);
            counters
                .peak_queued_bytes
                .fetch_max(queued_bytes, Ordering::Relaxed);
        }
        Self {
            counters: Arc::clone(counters),
            running,
            bytes,
        }
    }
}

impl Drop for HttpTraceCredit {
    fn drop(&mut self) {
        let counter = if self.running {
            &self.counters.running
        } else {
            &self.counters.queued
        };
        counter.fetch_sub(1, Ordering::AcqRel);
        if !self.running {
            self.counters
                .queued_bytes
                .fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

#[derive(Clone)]
struct HttpTraceConfig {
    threshold: Duration,
    sink: HttpTraceSink,
    counters: Arc<HttpTraceCounters>,
}

impl HttpTraceConfig {
    fn from_env() -> Option<Self> {
        let trace = HttpSlowTrace::from_env()?;
        Some(Self {
            threshold: trace.threshold,
            sink: HttpTraceSink::process()?,
            counters: Arc::new(HttpTraceCounters::default()),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HttpTraceOrigin {
    Response,
    QueueExpired,
    HandlerExpired,
    ReactorExpired,
    WriteExpired,
    WriteFailed,
    LateReply,
}

impl HttpTraceOrigin {
    fn name(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::QueueExpired => "queue_expired",
            Self::HandlerExpired => "handler_expired",
            Self::ReactorExpired => "reactor_handler_expired",
            Self::WriteExpired => "write_expired",
            Self::WriteFailed => "write_failed",
            Self::LateReply => "late_reply",
        }
    }
}

#[derive(Clone, Copy)]
struct HttpRequestTrace {
    id: Option<u64>,
    token: HttpIoToken,
    accepted: Option<Instant>,
    promoted: Option<Instant>,
    read_started: Option<Instant>,
    ready: Instant,
    handler_started: Option<Instant>,
    handler_finished: Option<Instant>,
    handler_cpu: Option<Duration>,
    published: Option<Instant>,
    observed: Option<Instant>,
    write_started: Option<Instant>,
    status: i64,
    origin: HttpTraceOrigin,
}

impl HttpRequestTrace {
    fn emit(self, config: &HttpTraceConfig, origin: HttpTraceOrigin, ended: Instant) {
        let queue = self
            .handler_started
            .map(|at| at.saturating_duration_since(self.ready));
        let handler = self
            .handler_started
            .zip(self.handler_finished)
            .map(|(start, end)| end.saturating_duration_since(start));
        let elapsed = ended.saturating_duration_since(self.ready);
        if origin == HttpTraceOrigin::Response
            && self.status < 500
            && queue.is_none_or(|value| value < config.threshold)
            && handler.is_none_or(|value| value < config.threshold)
            && elapsed < config.threshold
        {
            return;
        }
        config.sink.emit(HttpTraceEvent::Transport {
            trace: self,
            origin,
            ended,
        });
    }
}

enum HttpTraceEvent {
    Flight(super::http_flight::Snapshot),
    Slow {
        token: HttpIoToken,
        id: Option<u64>,
        sample: HttpSlowTraceSample,
        status: i64,
    },
    Transport {
        trace: HttpRequestTrace,
        origin: HttpTraceOrigin,
        ended: Instant,
    },
    Snapshot {
        elapsed: Duration,
        connections: usize,
        pending: usize,
        active: usize,
        headers: usize,
        bodies: usize,
        handler_slots: usize,
        writing: usize,
        queued: usize,
        queued_bytes: usize,
        peak_queued: usize,
        peak_queued_bytes: usize,
        rejected_jobs: usize,
        rejected_connections: usize,
        running: usize,
        buffer: BufferSnapshot,
        completed: usize,
        reactor_expired: usize,
        queue_expired: usize,
        handler_expired: usize,
        write_expired: usize,
        write_failed: usize,
        late_replies: usize,
        dropped: usize,
    },
}

fn trace_number(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".into())
}

fn trace_micros(value: Option<Duration>) -> String {
    value
        .map(|value| value.as_micros().to_string())
        .unwrap_or_else(|| "unavailable".into())
}

impl HttpTraceEvent {
    fn write(&self, output: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Flight(snapshot) => snapshot.write(output),
            Self::Slow { token, id, sample, status } => writeln!(output,
                "ku_http_slow token={token:?} queue_us={} handler_wall_us={} handler_cpu_us={} status={status} request_id={}",
                sample.queued.as_micros(), sample.handler_wall.as_micros(), trace_micros(sample.handler_cpu), trace_number(*id)),
            Self::Transport { trace, origin, ended } => {
                let since = |end: Option<Instant>, start: Option<Instant>| {
                    end.zip(start).map(|(end, start)| end.saturating_duration_since(start))
                };
                writeln!(output,
                    "ku_http_transport token={:?} request_id={} origin={} status={} accept_to_slot_us={} read_us={} queue_us={} handler_wall_us={} handler_cpu_us={} publish_attempt_to_observe_us={} response_phase_us={} ready_to_end_us={}",
                    trace.token, trace_number(trace.id), origin.name(), trace.status,
                    trace_micros(since(trace.promoted, trace.accepted)),
                    trace_micros(since(Some(trace.ready), trace.read_started)),
                    trace_micros(since(trace.handler_started, Some(trace.ready))),
                    trace_micros(since(trace.handler_finished, trace.handler_started)), trace_micros(trace.handler_cpu),
                    trace_micros(since(trace.observed, trace.published)),
                    trace_micros(since(Some(*ended), trace.write_started)),
                    ended.saturating_duration_since(trace.ready).as_micros())
            }
            Self::Snapshot { elapsed, connections, pending, active, headers, bodies,
                handler_slots, writing, queued, queued_bytes, peak_queued, peak_queued_bytes,
                rejected_jobs, rejected_connections, running, buffer, completed,
                reactor_expired, queue_expired, handler_expired, write_expired,
                write_failed, late_replies, dropped } => writeln!(output,
                "ku_http_snapshot elapsed_ms={} connections={connections} pending={pending} active={active} headers={headers} bodies={bodies} handler_slots={handler_slots} writing={writing} queued={queued} queued_bytes={queued_bytes} peak_queued={peak_queued} peak_queued_bytes={peak_queued_bytes} rejected_jobs={rejected_jobs} rejected_connections={rejected_connections} running={running} buffer_current={} buffer_peak={} buffer_limit={} buffer_rejected={} completed={completed} reactor_expired={reactor_expired} queue_expired={queue_expired} handler_expired={handler_expired} write_expired={write_expired} write_failed={write_failed} late_replies={late_replies} trace_dropped={dropped}",
                elapsed.as_millis(), buffer.current, buffer.peak, buffer.limit, buffer.rejected),
        }
    }
}

// A trace identifier is optional numeric metadata, never an arbitrary header
// value. Invalid bytes, signs, spaces and overflowing decimal input are absent.
fn request_trace_id(request: &ParsedHttpRequest) -> Option<u64> {
    let value = request.headers.get("x-ku-trace-id")?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

pub(super) fn emit_flight(snapshot: super::http_flight::Snapshot) {
    if let Some(sink) = HttpTraceSink::process() {
        sink.emit(HttpTraceEvent::Flight(snapshot));
    }
}

struct BufferCharge {
    budget: Arc<BufferBudget>,
    bytes: usize,
}

impl BufferCharge {
    fn resize(&mut self, bytes: usize) -> Option<()> {
        if bytes > self.bytes {
            let extra = self.budget.charge(bytes - self.bytes)?;
            self.merge(extra);
        } else {
            self.budget
                .current
                .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
            self.bytes = bytes;
        }
        Some(())
    }

    fn merge(&mut self, mut other: Self) {
        debug_assert!(Arc::ptr_eq(&self.budget, &other.budget));
        self.bytes += other.bytes;
        other.bytes = 0;
    }
}

impl Drop for BufferCharge {
    fn drop(&mut self) {
        self.budget.current.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct Buffer {
    // Field order releases the actual allocation before returning its credit.
    bytes: Vec<u8>,
    charge: BufferCharge,
}

impl Buffer {
    fn empty(budget: &Arc<BufferBudget>) -> Self {
        Self {
            bytes: Vec::new(),
            charge: budget.charge(0).expect("zero charge"),
        }
    }

    fn allocate(capacity: usize, budget: &Arc<BufferBudget>) -> Option<Self> {
        let mut charge = budget.charge(capacity)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).ok()?;
        // Vec's reported capacity is requested backing, not allocator usable
        // size. Keep its real reported value if the collection changes it.
        charge.resize(bytes.capacity())?;
        Some(Self { bytes, charge })
    }

    fn copy(bytes: &[u8], budget: &Arc<BufferBudget>) -> Option<Self> {
        let mut buffer = Self::allocate(bytes.len(), budget)?;
        buffer.bytes.extend_from_slice(bytes);
        Some(buffer)
    }

    fn grow(&mut self, capacity: usize) -> Option<()> {
        if capacity <= self.bytes.capacity() {
            return Some(());
        }
        // A replacement can coexist with the old allocation. Reserve the full
        // new capacity, rather than assuming realloc grows in place.
        let mut replacement = Self::allocate(capacity, &self.charge.budget)?;
        replacement.bytes.extend_from_slice(&self.bytes);
        *self = replacement;
        Some(())
    }
}

fn head_bytes(head: &ParsedHttpHead) -> Option<usize> {
    let mut bytes = head.method.capacity().checked_add(head.path.capacity())?;
    for (key, value) in head.query.iter().chain(head.headers.iter()) {
        bytes = bytes
            .checked_add(key.capacity())?
            .checked_add(value.capacity())?;
    }
    Some(bytes)
}

struct ChargedResponse {
    response: HttpWireResponse,
    charge: BufferCharge,
}

impl ChargedResponse {
    fn new(response: HttpWireResponse, budget: &Arc<BufferBudget>) -> Option<Self> {
        let mut bytes = response.body.capacity();
        for (key, value) in &response.headers {
            bytes = bytes
                .checked_add(key.capacity())?
                .checked_add(value.capacity())?;
        }
        let charge = budget.charge(bytes)?;
        Some(Self { response, charge })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HttpIoToken {
    slot: usize,
    generation: u64,
}

struct Completion {
    token: HttpIoToken,
    // None is the fixed, allocation-free over-budget 503 response.
    response: Option<ChargedResponse>,
    trace: Option<HttpRequestTrace>,
}

pub(super) struct HttpIoReply {
    token: HttpIoToken,
    sender: SyncSender<Completion>,
    wake: netpoll::WakeSender,
    input_charge: BufferCharge,
    trace: Option<HttpRequestTrace>,
    trace_config: Option<HttpTraceConfig>,
    handler_credit: Option<HttpTraceCredit>,
    flight: Option<Arc<FlightRecord>>,
}

impl HttpIoReply {
    pub(super) fn handler_started(&mut self) {
        if let (Some(trace), Some(config)) = (&mut self.trace, &self.trace_config) {
            trace.handler_started = Some(Instant::now());
            self.handler_credit = Some(HttpTraceCredit::acquire(&config.counters, true, 0));
        }
    }

    pub(super) fn handler_finished(
        &mut self,
        sample: Option<&HttpSlowTraceSample>,
        status: i64,
        origin: HttpTraceOrigin,
    ) {
        if let Some(flight) = &self.flight {
            flight.mark(Stage::Finished);
            if let Some(snapshot) = flight.snapshot(Stage::Finished) {
                emit_flight(snapshot);
            }
        }
        if let (Some(trace), Some(config)) = (&mut self.trace, &self.trace_config) {
            trace.handler_finished = Some(Instant::now());
            trace.handler_cpu = sample.and_then(|sample| sample.handler_cpu);
            trace.status = status;
            trace.origin = origin;
            match origin {
                HttpTraceOrigin::QueueExpired => increment(&config.counters.queue_expired),
                HttpTraceOrigin::HandlerExpired => increment(&config.counters.handler_expired),
                _ => {}
            }
        }
        self.handler_credit = None;
    }

    pub(super) fn slow_sample(&self, sample: HttpSlowTraceSample, status: i64) {
        if let (Some(trace), Some(config)) = (&self.trace, &self.trace_config) {
            config.sink.emit(HttpTraceEvent::Slow {
                token: self.token,
                id: trace.id,
                sample,
                status,
            });
        }
    }

    pub(super) fn respond(
        mut self,
        token: HttpIoToken,
        response: HttpWireResponse,
    ) -> io::Result<()> {
        if token != self.token {
            return Err(io::Error::other("HTTP reply token does not match its job"));
        }
        let budget = Arc::clone(&self.input_charge.budget);
        // The handler has actually returned. A socket timeout alone never
        // releases the request charge while this reply owner is still alive.
        drop(self.input_charge);
        let response = ChargedResponse::new(response, &budget);
        if let Some(trace) = &mut self.trace {
            trace.published = Some(Instant::now());
        }
        if let Some(flight) = &self.flight {
            flight.mark(Stage::Published);
        }
        // One consumed reply owns one initially empty cap-1 channel. A closed
        // receiver means timeout/close already won: dropping the response is
        // the complete late-result path, without touching any socket.
        match self.sender.try_send(Completion {
            token,
            response,
            trace: self.trace,
        }) {
            Ok(()) => {
                if let Some(flight) = &self.flight {
                    // A return from try_send proves local acceptance, not
                    // receiver acknowledgement. The reactor may have already
                    // observed the completion before this thread resumes.
                    flight.mark(Stage::PublishReturned);
                    flight.mark(Stage::WakeAttempt);
                }
                let result = self.wake.notify();
                if let Some(flight) = &self.flight {
                    match &result {
                        Ok(()) => {
                            // A successful notify return is not an observation
                            // acknowledgement from the receiving reactor.
                            flight.mark(Stage::WakeReturned);
                            if let Some(snapshot) = flight.snapshot(Stage::WakeReturned) {
                                emit_flight(snapshot);
                            }
                        }
                        Err(_) => {
                            flight.observe_anomaly_at(Stage::WakeFailed, Instant::now());
                        }
                    }
                }
                result
            }
            Err(TrySendError::Disconnected(_)) => {
                if let Some(flight) = &self.flight {
                    flight.mark(Stage::LateReply);
                    if let Some(snapshot) = flight.snapshot(Stage::LateReply) {
                        emit_flight(snapshot);
                    }
                }
                if let (Some(trace), Some(config)) = (self.trace, &self.trace_config) {
                    increment(&config.counters.late_replies);
                    trace.emit(config, HttpTraceOrigin::LateReply, Instant::now());
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(io::Error::other("HTTP reply published twice")),
        }
    }
}

pub(super) struct HttpIoJob {
    pub(super) token: HttpIoToken,
    pub(super) request: ParsedHttpRequest,
    pub(super) deadline: Instant,
    pub(super) reply: HttpIoReply,
    pub(super) flight: Option<Arc<FlightRecord>>,
    queued_credit: Option<HttpTraceCredit>,
}

pub(super) struct HttpIo {
    jobs: Receiver<HttpIoJob>,
    stop: Arc<AtomicBool>,
    wake: netpoll::WakeSender,
    thread: Option<JoinHandle<io::Result<()>>>,
    #[cfg(test)]
    budget: Arc<BufferBudget>,
}

impl HttpIo {
    pub(super) fn start(
        listener: TcpListener,
        limits: HttpServerRuntimeLimits,
    ) -> io::Result<Self> {
        Self::start_with_accept_and_trace(
            listener,
            limits,
            TcpListener::accept,
            HttpTraceConfig::from_env(),
        )
    }

    // Keep the real reactor and socket ownership path available to controlled
    // accept-error regressions, without replacing readiness or connection I/O.
    #[cfg(test)]
    pub(super) fn start_with_accept<F>(
        listener: TcpListener,
        limits: HttpServerRuntimeLimits,
        accept: F,
    ) -> io::Result<Self>
    where
        F: FnMut(&TcpListener) -> io::Result<(TcpStream, std::net::SocketAddr)> + Send + 'static,
    {
        Self::start_with_accept_and_trace(listener, limits, accept, None)
    }

    fn start_with_accept_and_trace<F>(
        listener: TcpListener,
        limits: HttpServerRuntimeLimits,
        accept: F,
        trace: Option<HttpTraceConfig>,
    ) -> io::Result<Self>
    where
        F: FnMut(&TcpListener) -> io::Result<(TcpStream, std::net::SocketAddr)> + Send + 'static,
    {
        // Set up the shared writer before accepting any diagnostic traffic;
        // the first anomaly must only enqueue its fixed-size snapshot.
        if http_flight::enabled() {
            let _ = HttpTraceSink::process();
        }
        listener.set_nonblocking(true)?;
        let wake_pair = netpoll::WakePair::new()?;
        let wake = wake_pair.sender.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (job_tx, jobs) = mpsc::sync_channel(limits.max_active_requests);
        let budget = BufferBudget::new(limits.max_buffer_bytes);
        let mut reactor = Reactor::new(
            listener,
            limits,
            job_tx,
            wake_pair,
            Arc::clone(&stop),
            Arc::clone(&budget),
        );
        reactor.trace = trace;
        let thread = thread::Builder::new()
            .name("ku-http-io".into())
            .spawn(move || reactor.run(accept))?;
        Ok(Self {
            jobs,
            stop,
            wake,
            thread: Some(thread),
            #[cfg(test)]
            budget,
        })
    }

    #[cfg(test)]
    pub(super) fn buffer_snapshot(&self) -> BufferSnapshot {
        self.budget.snapshot()
    }

    pub(super) fn next_job(&mut self, timeout: Duration) -> io::Result<Option<HttpIoJob>> {
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            self.join()?;
            return Err(io::Error::other("HTTP I/O reactor stopped"));
        }
        match self.jobs.recv_timeout(timeout) {
            Ok(mut job) => {
                if let Some(flight) = &job.flight {
                    flight.mark(Stage::Dequeued);
                }
                job.queued_credit = None;
                Ok(Some(job))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.join()?;
                Err(io::Error::other("HTTP I/O reactor stopped"))
            }
        }
    }

    fn join(&mut self) -> io::Result<()> {
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| io::Error::other("HTTP I/O reactor panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for HttpIo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.wake.notify().is_err() {
            // Even an interrupted/failed notification must release a poll with
            // no connection deadline. Half-close makes the reader ready at EOF.
            let _ = self.wake.shutdown();
        }
        let _ = self.join();
    }
}

struct Slot {
    generation: u64,
    connection: Option<Connection>,
}

struct Reactor {
    listener: TcpListener,
    limits: HttpServerRuntimeLimits,
    jobs: SyncSender<HttpIoJob>,
    wake: netpoll::WakePair,
    stop: Arc<AtomicBool>,
    connection_count: Arc<AtomicUsize>,
    pending: VecDeque<PendingHttpConnection>,
    slots: Vec<Slot>,
    free: Vec<usize>,
    rejecting: Option<HttpRejection>,
    rejection_generation: u64,
    accept_retry_deadline: Option<Instant>,
    budget: Arc<BufferBudget>,
    trace: Option<HttpTraceConfig>,
    trace_started: Instant,
    trace_next_snapshot: Instant,
}

#[derive(Clone, Copy)]
enum PollOwner {
    Wake,
    Listener,
    Connection(HttpIoToken),
    Rejection(u64),
}

impl Reactor {
    fn new(
        listener: TcpListener,
        limits: HttpServerRuntimeLimits,
        jobs: SyncSender<HttpIoJob>,
        wake: netpoll::WakePair,
        stop: Arc<AtomicBool>,
        budget: Arc<BufferBudget>,
    ) -> Self {
        Self {
            listener,
            limits,
            jobs,
            wake,
            stop,
            connection_count: Arc::new(AtomicUsize::new(0)),
            pending: VecDeque::new(),
            slots: (0..limits.max_active_requests)
                .map(|_| Slot {
                    generation: 0,
                    connection: None,
                })
                .collect(),
            free: (0..limits.max_active_requests).rev().collect(),
            rejecting: None,
            rejection_generation: 0,
            accept_retry_deadline: None,
            budget,
            trace: None,
            trace_started: Instant::now(),
            trace_next_snapshot: Instant::now(),
        }
    }

    fn trace_snapshot(&mut self, now: Instant) {
        let Some(config) = &self.trace else {
            return;
        };
        if now < self.trace_next_snapshot {
            return;
        }
        self.trace_next_snapshot = now.checked_add(TRACE_SNAPSHOT_INTERVAL).unwrap_or(now);
        let mut active = 0;
        let (mut headers, mut bodies, mut handler_slots, mut writing) = (0, 0, 0, 0);
        for slot in &self.slots {
            if let Some(connection) = &slot.connection {
                active += 1;
                match connection.state {
                    State::Header(_) => headers += 1,
                    State::Body(_) => bodies += 1,
                    State::Handler { .. } => handler_slots += 1,
                    State::Write(_) => writing += 1,
                    State::Request { .. } => {}
                }
            }
        }
        let counters = &config.counters;
        config.sink.emit(HttpTraceEvent::Snapshot {
            elapsed: now.saturating_duration_since(self.trace_started),
            connections: self.connection_count.load(Ordering::Acquire),
            pending: self.pending.len(),
            active,
            headers,
            bodies,
            handler_slots,
            writing,
            queued: counters.queued.load(Ordering::Acquire),
            queued_bytes: counters.queued_bytes.load(Ordering::Acquire),
            peak_queued: counters.peak_queued.load(Ordering::Relaxed),
            peak_queued_bytes: counters.peak_queued_bytes.load(Ordering::Relaxed),
            rejected_jobs: counters.rejected_jobs.load(Ordering::Relaxed),
            rejected_connections: counters.rejected_connections.load(Ordering::Relaxed),
            running: counters.running.load(Ordering::Acquire),
            buffer: self.budget.snapshot(),
            completed: counters.completed.load(Ordering::Relaxed),
            reactor_expired: counters.reactor_expired.load(Ordering::Relaxed),
            queue_expired: counters.queue_expired.load(Ordering::Relaxed),
            handler_expired: counters.handler_expired.load(Ordering::Relaxed),
            write_expired: counters.write_expired.load(Ordering::Relaxed),
            write_failed: counters.write_failed.load(Ordering::Relaxed),
            late_replies: counters.late_replies.load(Ordering::Relaxed),
            dropped: config.sink.state.dropped.load(Ordering::Relaxed),
        });
    }

    fn run<F>(mut self, mut accept: F) -> io::Result<()>
    where
        F: FnMut(&TcpListener) -> io::Result<(TcpStream, std::net::SocketAddr)>,
    {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // A published response wins over merely observing its D later,
            // as the original cap-1 recv_timeout did. Never replace that with
            // a blanket now>=D test before observing the reply queue.
            self.observe_completions();
            self.expire_connections();
            self.promote_pending()?;
            self.advance_buffered_headers();
            self.dispatch_requests()?;
            self.trace_snapshot(Instant::now());
            if let Some(rejection) = self.rejecting.as_mut() {
                if rejection.poll() {
                    self.rejecting = None;
                }
            }
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }

            let mut interests = Vec::with_capacity(self.slots.len() + 3);
            let mut owners = Vec::with_capacity(self.slots.len() + 3);
            let mut deadline = self.trace.as_ref().map(|_| self.trace_next_snapshot);
            interests.push(netpoll::Interest {
                socket: netpoll::Socket::Stream(&self.wake.reader),
                read: true,
                write: false,
            });
            owners.push(PollOwner::Wake);
            // Match the existing single extra rejection owner: do not accept
            // another excess socket while its drain/write is still resident.
            if self.rejecting.is_none() {
                if let Some(retry) = self
                    .accept_retry_deadline
                    .filter(|retry| Instant::now() < *retry)
                {
                    // A peer abort yields only the listener. Active reads,
                    // responses, handler replies and stop wakes still progress.
                    tighten_deadline(&mut deadline, Some(retry));
                } else {
                    // Never retain an expired retry as the nearest deadline:
                    // that would turn subsequent readiness waits into poll(0).
                    self.accept_retry_deadline = None;
                    interests.push(netpoll::Interest {
                        socket: netpoll::Socket::Listener(&self.listener),
                        read: true,
                        write: false,
                    });
                    owners.push(PollOwner::Listener);
                }
            }
            for (slot, entry) in self.slots.iter().enumerate() {
                let Some(connection) = &entry.connection else {
                    continue;
                };
                tighten_deadline(&mut deadline, connection.deadline());
                let (read, write) = connection.interest();
                if read || write {
                    interests.push(netpoll::Interest {
                        socket: netpoll::Socket::Stream(&connection.stream),
                        read,
                        write,
                    });
                    owners.push(PollOwner::Connection(HttpIoToken {
                        slot,
                        generation: entry.generation,
                    }));
                }
            }
            if let Some(rejection) = &self.rejecting {
                tighten_deadline(
                    &mut deadline,
                    Some(rejection.write_deadline.unwrap_or(rejection.drain_deadline)),
                );
                interests.push(netpoll::Interest {
                    socket: netpoll::Socket::Stream(&rejection.stream),
                    read: rejection.write_deadline.is_none(),
                    write: rejection.write_deadline.is_some(),
                });
                owners.push(PollOwner::Rejection(self.rejection_generation));
            }
            let ready = netpoll::wait(&interests, deadline)?;
            drop(interests);
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            for event in ready {
                let Some(owner) = owners.get(event.index).copied() else {
                    return Err(io::Error::other("HTTP readiness index is invalid"));
                };
                match owner {
                    PollOwner::Wake => self.wake.drain()?,
                    PollOwner::Listener => self.accept_connections(&mut accept)?,
                    PollOwner::Connection(token) => {
                        let allow_reuse = self.pending.is_empty();
                        let Some(slot) = self.slots.get_mut(token.slot) else {
                            continue;
                        };
                        if slot.generation != token.generation {
                            continue;
                        }
                        let Some(connection) = slot.connection.as_mut() else {
                            continue;
                        };
                        if (event.readable || event.writable || event.error)
                            && connection.poll(self.limits, allow_reuse)
                        {
                            self.close_slot(token.slot);
                        }
                    }
                    PollOwner::Rejection(generation) => {
                        if generation == self.rejection_generation {
                            if let Some(rejection) = self.rejecting.as_mut() {
                                if rejection.poll() {
                                    self.rejecting = None;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn observe_completions(&mut self) {
        let allow_reuse = self.pending.is_empty();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(connection) = slot.connection.as_mut() else {
                continue;
            };
            let State::Handler { receiver, .. } = &connection.state else {
                continue;
            };
            let response = match receiver.try_recv() {
                Ok(completion)
                    if completion.token
                        == (HttpIoToken {
                            slot: index,
                            generation: slot.generation,
                        }) =>
                {
                    if let Some(flight) = &connection.flight {
                        flight.mark(Stage::Observed);
                        flight.mark(Stage::WriteStart);
                    }
                    connection.trace = completion.trace.map(|mut trace| {
                        trace.observed = Some(Instant::now());
                        trace.write_started = trace.observed;
                        trace
                    });
                    Some(completion.response)
                }
                Ok(_) | Err(TryRecvError::Disconnected) => Some(ChargedResponse::new(
                    status_response(500, "Internal Server Error"),
                    &self.budget,
                )),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(response) = response {
                connection.state = State::Write(Writing::charged(
                    response,
                    self.limits.write_timeout,
                    connection.keep_alive && allow_reuse,
                ));
            }
        }
    }

    fn expire_connections(&mut self) {
        let now = Instant::now();
        for index in 0..self.slots.len() {
            let close = self.slots[index]
                .connection
                .as_mut()
                .is_some_and(|connection| connection.expire(now, self.limits));
            if close {
                self.close_slot(index);
            }
        }
    }

    fn close_slot(&mut self, index: usize) {
        if let Some(connection) = self.slots[index].connection.take() {
            let _ = connection.stream.shutdown(Shutdown::Write);
            drop(connection);
            self.free.push(index);
        }
    }

    fn promote_pending(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() && self.free.is_empty() {
            // A persistent socket with no next-request bytes must not reserve
            // the last activity slot ahead of an already admitted connection.
            // Closing an idle HTTP connection is permitted; partial requests
            // and buffered pipeline bytes keep their original ownership/D.
            if let Some(index) = self.slots.iter().position(|slot| {
                slot.connection.as_ref().is_some_and(|connection| {
                    connection.reused
                        && matches!(&connection.state, State::Header(header) if header.bytes.bytes.is_empty())
                        && match connection.stream.peek(&mut [0_u8; 1]) {
                            Ok(0) => true,
                            Ok(_) => false,
                            Err(error) => error.kind() != io::ErrorKind::Interrupted,
                        }
                })
            }) {
                self.close_slot(index);
            }
        }
        while !self.pending.is_empty() && !self.free.is_empty() {
            let index = self.free.pop().expect("checked free slot");
            let pending = self
                .pending
                .pop_front()
                .expect("checked pending connection");
            let slot = &mut self.slots[index];
            slot.generation = slot
                .generation
                .checked_add(1)
                .ok_or_else(|| io::Error::other("HTTP connection generation exhausted"))?;
            slot.connection = Some(Connection::new(
                pending,
                self.limits,
                Arc::clone(&self.budget),
                self.trace.clone(),
            ));
        }
        Ok(())
    }

    fn accept_connections<F>(&mut self, accept: &mut F) -> io::Result<()>
    where
        F: FnMut(&TcpListener) -> io::Result<(TcpStream, std::net::SocketAddr)>,
    {
        for _ in 0..HTTP_ACCEPT_BATCH {
            if self.rejecting.is_some() || self.stop.load(Ordering::Acquire) {
                break;
            }
            let stream = match accept(&self.listener) {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if accept_error_is_peer_transient(&error) => {
                    // Reuse the existing interpreter observation interval; no
                    // connection/handler deadline or public limit is extended.
                    self.accept_retry_deadline = Some(http_deadline(super::HTTP_EVENT_LOOP_SLEEP));
                    break;
                }
                Err(error) => return Err(error),
            };
            let accepted_at = Instant::now();
            stream.set_nonblocking(true)?;
            let permit = HttpConnectionPermit::try_acquire(
                Arc::clone(&self.connection_count),
                self.limits.max_connections,
            );
            if permit.is_none() || self.pending.len() >= self.limits.max_pending_requests {
                if let Some(config) = &self.trace {
                    increment(&config.counters.rejected_connections);
                }
                self.rejection_generation = self
                    .rejection_generation
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("HTTP rejection generation exhausted"))?;
                self.rejecting = HttpRejection::new(stream, permit, self.limits);
                continue;
            }
            self.pending.push_back(PendingHttpConnection {
                stream,
                accepted_at,
                _permit: permit.expect("checked connection permit"),
            });
            self.promote_pending()?;
        }
        Ok(())
    }

    fn advance_buffered_headers(&mut self) {
        let allow_reuse = self.pending.is_empty();
        for index in 0..self.slots.len() {
            let close = self.slots[index].connection.as_mut().is_some_and(|connection| {
                matches!(&connection.state, State::Header(header) if header.scanned < header.bytes.bytes.len())
                    && connection.poll(self.limits, allow_reuse)
            });
            if close {
                self.close_slot(index);
            }
        }
    }

    fn dispatch_requests(&mut self) -> io::Result<()> {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(connection) = slot.connection.as_mut() else {
                continue;
            };
            let State::Request {
                request,
                deadline,
                carry,
                input_charge,
                ready_at,
            } = &mut connection.state
            else {
                continue;
            };
            let request = request.take().expect("one request dispatch");
            let deadline = *deadline;
            connection.keep_alive = request.keep_alive;
            connection.carry = std::mem::replace(carry, Buffer::empty(&self.budget));
            let input_charge = input_charge.take().expect("one input charge transfer");
            slot.generation = slot
                .generation
                .checked_add(1)
                .ok_or_else(|| io::Error::other("HTTP request generation exhausted"))?;
            let token = HttpIoToken {
                slot: index,
                generation: slot.generation,
            };
            if connection.trace_config.is_some() {
                connection.trace = Some(HttpRequestTrace {
                    id: request_trace_id(&request),
                    token,
                    accepted: (!connection.reused).then_some(connection.accepted_at),
                    promoted: (!connection.reused).then_some(connection.promoted_at),
                    read_started: connection.read_started,
                    ready: *ready_at,
                    handler_started: None,
                    handler_finished: None,
                    handler_cpu: None,
                    published: None,
                    observed: None,
                    write_started: None,
                    status: 0,
                    origin: HttpTraceOrigin::Response,
                });
            }
            let queued_bytes = input_charge.bytes;
            let flight = http_flight::enabled().then(|| {
                let record = FlightRecord::new(request_trace_id(&request), *ready_at, deadline);
                if !connection.reused {
                    record.mark_at(Stage::Accepted, connection.accepted_at);
                    record.mark_at(Stage::SlotPromoted, connection.promoted_at);
                }
                if let Some(at) = connection.read_started {
                    record.mark_at(Stage::ReadStarted, at);
                }
                record
            });
            connection.flight = flight.clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            let job = HttpIoJob {
                token,
                request,
                deadline,
                reply: HttpIoReply {
                    token,
                    sender,
                    wake: self.wake.sender.clone(),
                    input_charge,
                    trace: connection.trace,
                    trace_config: connection.trace_config.clone(),
                    handler_credit: None,
                    flight: flight.clone(),
                },
                flight: flight.clone(),
                queued_credit: connection
                    .trace_config
                    .as_ref()
                    .map(|config| HttpTraceCredit::acquire(&config.counters, false, queued_bytes)),
            };
            if let Some(flight) = &flight {
                flight.mark(Stage::EnqueueAttempt);
            }
            match self.jobs.try_send(job) {
                Ok(()) => {
                    if let Some(flight) = &flight {
                        flight.mark(Stage::Enqueued);
                    }
                    connection.state = State::Handler { receiver, deadline };
                }
                Err(TrySendError::Full(_)) => {
                    if let Some(config) = &connection.trace_config {
                        increment(&config.counters.rejected_jobs);
                    }
                    if let Some(trace) = &mut connection.trace {
                        trace.status = 503;
                        trace.write_started = Some(Instant::now());
                    }
                    connection.state = State::Write(Writing::new(
                        status_response(503, "Service Unavailable"),
                        self.limits.write_timeout,
                        false,
                        &self.budget,
                    ));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::other("HTTP handler queue disconnected"));
                }
            }
        }
        Ok(())
    }
}

fn accept_error_is_peer_transient(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Interrupted
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) {
        return true;
    }
    // Linux accept(2) can report these pending TCP errors from a new
    // connection. Match raw errno, not broad NetworkDown/Unsupported kinds.
    #[cfg(target_os = "linux")]
    if matches!(
        error.raw_os_error(),
        Some(
            libc::ENETDOWN
                | libc::EPROTO
                | libc::ENOPROTOOPT
                | libc::EHOSTDOWN
                | libc::ENONET
                | libc::EHOSTUNREACH
                | libc::EOPNOTSUPP
                | libc::ENETUNREACH
        )
    ) {
        return true;
    }
    // Rust 1.89 maps WSAECONNABORTED/RESET to the kinds above, but not
    // WSAEINTR to Interrupted. Preserve that exact native-runtime case too.
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAEINTR) {
        return true;
    }
    false
}

fn tighten_deadline(saved: &mut Option<Instant>, deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        *saved = Some(saved.map_or(deadline, |previous| previous.min(deadline)));
    }
}

struct Connection {
    stream: TcpStream,
    accepted_at: Instant,
    promoted_at: Instant,
    read_started: Option<Instant>,
    trace: Option<HttpRequestTrace>,
    trace_config: Option<HttpTraceConfig>,
    flight: Option<Arc<FlightRecord>>,
    _permit: HttpConnectionPermit,
    state: State,
    carry: Buffer,
    budget: Arc<BufferBudget>,
    keep_alive: bool,
    reused: bool,
}

enum State {
    Header(Header),
    Body(Body),
    Request {
        request: Option<ParsedHttpRequest>,
        deadline: Instant,
        carry: Buffer,
        input_charge: Option<BufferCharge>,
        ready_at: Instant,
    },
    Handler {
        receiver: Receiver<Completion>,
        deadline: Instant,
    },
    Write(Writing),
}

struct Header {
    bytes: Buffer,
    scanned: usize,
    eof: bool,
    idle_deadline: Instant,
    header_deadline: Option<Instant>,
    read_started: Option<Instant>,
}

struct Body {
    head: Option<ParsedHttpHead>,
    head_charge: Option<BufferCharge>,
    bytes: Buffer,
    received: usize,
    deadline: Instant,
    handler_timeout: Duration,
}

struct Writing {
    head: Cow<'static, str>,
    body: Cow<'static, str>,
    head_sent: usize,
    body_sent: usize,
    deadline: Instant,
    keep_alive: bool,
    _charge: Option<BufferCharge>,
}

enum WriteProgress {
    Pending,
    Complete,
    Failed,
}

impl Writing {
    fn wire_status(&self) -> i64 {
        self.head
            .get(9..12)
            .and_then(|status| status.parse().ok())
            .unwrap_or(500)
    }

    fn unavailable(timeout: Duration) -> Self {
        Self {
            head: Cow::Borrowed("HTTP/1.1 503 Service Unavailable\r\ncontent-length: 19\r\nconnection: close\r\ncontent-type: text/plain; charset=utf-8\r\n\r\n"),
            body: Cow::Borrowed("Service Unavailable"),
            head_sent: 0,
            body_sent: 0,
            deadline: http_deadline(timeout),
            keep_alive: false,
            _charge: None,
        }
    }

    fn new(
        response: HttpWireResponse,
        timeout: Duration,
        keep_alive: bool,
        budget: &Arc<BufferBudget>,
    ) -> Self {
        Self::charged(ChargedResponse::new(response, budget), timeout, keep_alive)
    }

    fn charged(response: Option<ChargedResponse>, timeout: Duration, keep_alive: bool) -> Self {
        let Some(ChargedResponse {
            response,
            mut charge,
        }) = response
        else {
            return Self::unavailable(timeout);
        };
        // Header normalization and encoding coexist with the original owned
        // fields. Reserve a conservative scratch bound before either runs.
        // The body is moved, never copied into the wire header allocation.
        let scratch = response
            .headers
            .iter()
            .try_fold(512usize, |bytes, (name, value)| {
                bytes
                    .checked_add(name.capacity().checked_mul(4)?)?
                    .checked_add(value.capacity().checked_mul(3)?)
            });
        let Some(total) = scratch.and_then(|scratch| charge.bytes.checked_add(scratch)) else {
            drop(response);
            drop(charge);
            return Self::unavailable(timeout);
        };
        if charge.resize(total).is_none() {
            drop(response);
            drop(charge);
            return Self::unavailable(timeout);
        }
        let (head, body, keep_alive) = encode_http_response(response, keep_alive);
        let Some(bytes) = head.capacity().checked_add(body.capacity()) else {
            drop(head);
            drop(body);
            drop(charge);
            return Self::unavailable(timeout);
        };
        // The pre-reserved scratch must cover every reported wire capacity.
        if bytes > charge.bytes {
            drop(head);
            drop(body);
            drop(charge);
            return Self::unavailable(timeout);
        }
        charge.resize(bytes).expect("shrinking a charge");
        Self {
            head: Cow::Owned(head),
            body: Cow::Owned(body),
            head_sent: 0,
            body_sent: 0,
            deadline: http_deadline(timeout),
            keep_alive,
            _charge: Some(charge),
        }
    }

    fn poll(&mut self, stream: &mut TcpStream) -> WriteProgress {
        let (bytes, offset) = if self.head_sent < self.head.len() {
            (self.head.as_bytes(), &mut self.head_sent)
        } else {
            (self.body.as_bytes(), &mut self.body_sent)
        };
        if *offset == bytes.len() {
            return WriteProgress::Complete;
        }
        let end = bytes.len().min(offset.saturating_add(READ_CHUNK));
        match stream.write(&bytes[*offset..end]) {
            Ok(0) => WriteProgress::Failed,
            Ok(written) => {
                *offset += written;
                if self.head_sent == self.head.len() && self.body_sent == self.body.len() {
                    WriteProgress::Complete
                } else {
                    WriteProgress::Pending
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                WriteProgress::Pending
            }
            Err(_) => WriteProgress::Failed,
        }
    }
}

impl Connection {
    fn new(
        pending: PendingHttpConnection,
        limits: HttpServerRuntimeLimits,
        budget: Arc<BufferBudget>,
        trace_config: Option<HttpTraceConfig>,
    ) -> Self {
        Self {
            stream: pending.stream,
            accepted_at: pending.accepted_at,
            promoted_at: Instant::now(),
            read_started: None,
            trace: None,
            trace_config,
            flight: None,
            _permit: pending._permit,
            state: State::Header(Header {
                bytes: Buffer::empty(&budget),
                scanned: 0,
                eof: false,
                idle_deadline: http_deadline(limits.idle_timeout),
                header_deadline: None,
                read_started: None,
            }),
            carry: Buffer::empty(&budget),
            budget,
            keep_alive: false,
            reused: false,
        }
    }

    fn emit_trace(&mut self, origin: HttpTraceOrigin, now: Instant) {
        if let Some(flight) = self.flight.take() {
            if matches!(origin, HttpTraceOrigin::WriteFailed) {
                flight.observe_anomaly_at(Stage::WriteFailed, now);
            } else if matches!(origin, HttpTraceOrigin::WriteExpired) {
                flight.observe_anomaly_at(Stage::WriteExpired, now);
            } else {
                flight.mark_at(Stage::WriteEnd, now);
                if let Some(snapshot) = flight.snapshot(Stage::WriteEnd) {
                    emit_flight(snapshot);
                } else if flight.past_planned_deadline(now) {
                    // This may be a 200 published before D and written later;
                    // it is a slow response, not an invented handler timeout.
                    flight.observe_anomaly_at(Stage::SlowResponse, now);
                }
            }
        }
        if let (Some(mut trace), Some(config)) = (self.trace.take(), &self.trace_config) {
            if let State::Write(writing) = &self.state {
                // Admission/encoding can replace an otherwise successful
                // handler reply with the fixed over-budget response.
                trace.status = writing.wire_status();
            }
            trace.emit(config, origin, now);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match &self.state {
            State::Header(header) => Some(header.header_deadline.unwrap_or(header.idle_deadline)),
            State::Body(body) => Some(body.deadline),
            State::Handler { deadline, .. } => Some(*deadline),
            State::Write(write) => Some(write.deadline),
            State::Request { deadline, .. } => Some(*deadline),
        }
    }

    fn interest(&self) -> (bool, bool) {
        match &self.state {
            State::Header(_) | State::Body(_) => (true, false),
            State::Write(_) => (false, true),
            State::Request { .. } | State::Handler { .. } => (false, false),
        }
    }

    fn expire(&mut self, now: Instant, limits: HttpServerRuntimeLimits) -> bool {
        if self.deadline().is_none_or(|deadline| now < deadline) {
            return false;
        }
        let response = match self.state {
            State::Header(_) | State::Body(_) => status_response(408, "Request Timeout"),
            State::Handler { .. } => {
                if let Some(flight) = &self.flight {
                    flight.expire(Stage::ReactorExpired);
                    flight.mark(Stage::WriteStart);
                }
                if let Some(config) = &self.trace_config {
                    increment(&config.counters.reactor_expired);
                    if let Some(trace) = &mut self.trace {
                        trace.status = 504;
                        trace.origin = HttpTraceOrigin::ReactorExpired;
                        trace.emit(config, HttpTraceOrigin::ReactorExpired, now);
                    }
                }
                status_response(504, "Gateway Timeout")
            }
            State::Write(_) => {
                if let Some(config) = &self.trace_config {
                    increment(&config.counters.write_expired);
                }
                self.emit_trace(HttpTraceOrigin::WriteExpired, now);
                return true;
            }
            State::Request { .. } => return false,
        };
        self.keep_alive = false;
        self.carry = Buffer::empty(&self.budget);
        if let Some(trace) = &mut self.trace {
            trace.write_started = Some(now);
        }
        self.state = State::Write(Writing::new(
            response,
            limits.write_timeout,
            false,
            &self.budget,
        ));
        false
    }

    fn poll(&mut self, limits: HttpServerRuntimeLimits, allow_reuse: bool) -> bool {
        if self.expire(Instant::now(), limits) {
            return true;
        }
        let next = match &mut self.state {
            State::Header(header) => {
                let next = header.poll(&mut self.stream, limits);
                self.read_started = header.read_started;
                if self.reused && header.eof {
                    return true;
                }
                next
            }
            State::Body(body) => body.poll(&mut self.stream),
            State::Write(write) => {
                return match write.poll(&mut self.stream) {
                    WriteProgress::Pending => false,
                    WriteProgress::Failed => {
                        if let Some(config) = &self.trace_config {
                            increment(&config.counters.write_failed);
                        }
                        self.emit_trace(HttpTraceOrigin::WriteFailed, Instant::now());
                        true
                    }
                    WriteProgress::Complete if !write.keep_alive || !allow_reuse => {
                        if let Some(config) = &self.trace_config {
                            increment(&config.counters.completed);
                        }
                        let origin = self
                            .trace
                            .map_or(HttpTraceOrigin::Response, |trace| trace.origin);
                        self.emit_trace(origin, Instant::now());
                        true
                    }
                    WriteProgress::Complete => {
                        if let Some(config) = &self.trace_config {
                            increment(&config.counters.completed);
                        }
                        let origin = self
                            .trace
                            .map_or(HttpTraceOrigin::Response, |trace| trace.origin);
                        self.emit_trace(origin, Instant::now());
                        let bytes = std::mem::replace(&mut self.carry, Buffer::empty(&self.budget));
                        let header_deadline = (!bytes.bytes.is_empty())
                            .then(|| http_deadline(limits.read_header_timeout));
                        self.state = State::Header(Header {
                            bytes,
                            scanned: 0,
                            eof: false,
                            idle_deadline: http_deadline(limits.idle_timeout),
                            header_deadline,
                            read_started: None,
                        });
                        self.read_started = None;
                        self.reused = true;
                        self.keep_alive = false;
                        false
                    }
                };
            }
            State::Request { .. } | State::Handler { .. } => None,
        };
        if let Some(next) = next {
            self.state = match next {
                Ok(state) => state,
                Err(response) => {
                    self.keep_alive = false;
                    self.carry = Buffer::empty(&self.budget);
                    State::Write(Writing::new(
                        response,
                        limits.write_timeout,
                        false,
                        &self.budget,
                    ))
                }
            };
        }
        false
    }
}

impl Header {
    fn poll(
        &mut self,
        stream: &mut TcpStream,
        limits: HttpServerRuntimeLimits,
    ) -> Option<Result<State, HttpWireResponse>> {
        let budget = Arc::clone(&self.bytes.charge.budget);
        let start = self.scanned;
        if start == self.bytes.bytes.len() {
            let mut incoming = [0; READ_CHUNK];
            let read = match stream.read(&mut incoming) {
                Ok(0) => {
                    self.eof = self.bytes.bytes.is_empty();
                    return Some(Err(status_response(400, "Bad Request")));
                }
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return None;
                }
                Err(_) => return Some(Err(status_response(408, "Request Timeout"))),
            };
            if self.read_started.is_none() {
                self.read_started = Some(Instant::now());
            }
            let needed = self.bytes.bytes.len() + read;
            if self.bytes.grow(needed.next_power_of_two()).is_none() {
                return Some(Err(status_response(503, "Service Unavailable")));
            }
            self.bytes.bytes.extend_from_slice(&incoming[..read]);
            if self.header_deadline.is_none() {
                self.header_deadline = Some(http_deadline(limits.read_header_timeout));
            }
        }
        for index in start..self.bytes.bytes.len() {
            // Preserve the original error precedence: byte max+1 is 431,
            // including when the same byte also violates line framing.
            if index + 1 > limits.max_header_bytes {
                return Some(Err(status_response(431, "Request Header Fields Too Large")));
            }
            if (self.bytes.bytes[index] == b'\n'
                && (index < 1 || self.bytes.bytes[index - 1] != b'\r'))
                || (index >= 1
                    && self.bytes.bytes[index - 1] == b'\r'
                    && self.bytes.bytes[index] != b'\n')
            {
                return Some(Err(status_response(400, "Bad Request")));
            }
            if index >= 3 && self.bytes.bytes[index - 3..=index] == *b"\r\n\r\n" {
                // At most one read chunk can trail this header. Reserve the
                // independent prefix before allocating while header stays live.
                let Some(mut prefix) = Buffer::copy(&self.bytes.bytes[index + 1..], &budget) else {
                    return Some(Err(status_response(503, "Service Unavailable")));
                };
                self.bytes.bytes.truncate(index - 3);
                // Parsing copies method/target/fields/query payloads before the
                // raw header is freed. This bound includes transient copies;
                // map buckets remain outside the documented backing counter.
                let parse_bound = self.bytes.bytes.len().checked_mul(4)?.checked_add(64)?;
                let Some(mut head_charge) = budget.charge(parse_bound) else {
                    return Some(Err(status_response(503, "Service Unavailable")));
                };
                let Buffer {
                    bytes: header,
                    charge: header_charge,
                } = std::mem::replace(&mut self.bytes, Buffer::empty(&budget));
                let head = match parse_http_head(header, limits, entry_span()) {
                    Ok(head) => head,
                    Err(response) => return Some(Err(response)),
                };
                drop(header_charge);
                let Some(parsed_bytes) = head_bytes(&head) else {
                    return Some(Err(status_response(503, "Service Unavailable")));
                };
                // All parser payloads fit within the reservation made above.
                if parsed_bytes > head_charge.bytes {
                    return Some(Err(status_response(503, "Service Unavailable")));
                }
                head_charge
                    .resize(parsed_bytes)
                    .expect("shrinking parsed payload charge");
                let Some(mut bytes) = Buffer::allocate(head.content_length, &budget) else {
                    return Some(Err(status_response(503, "Service Unavailable")));
                };
                bytes.bytes.resize(head.content_length, 0);
                let received = prefix.bytes.len().min(bytes.bytes.len());
                bytes.bytes[..received].copy_from_slice(&prefix.bytes[..received]);
                if received == bytes.bytes.len() {
                    prefix.bytes.drain(..received);
                    if prefix.bytes.is_empty() || !head.keep_alive {
                        // clear alone would retain capacity and its charge.
                        prefix = Buffer::empty(&budget);
                    }
                    debug_assert!(prefix.bytes.len() <= READ_CHUNK);
                    let Buffer {
                        bytes: body,
                        charge: body_charge,
                    } = bytes;
                    head_charge.merge(body_charge);
                    return Some(finish_http_request(head, body).map(|request| {
                        let ready_at = Instant::now();
                        State::Request {
                            request: Some(request),
                            deadline: ready_at
                                .checked_add(limits.handler_timeout)
                                .unwrap_or(ready_at),
                            ready_at,
                            carry: prefix,
                            input_charge: Some(head_charge),
                        }
                    }));
                }
                return Some(Ok(State::Body(Body {
                    head: Some(head),
                    head_charge: Some(head_charge),
                    bytes,
                    received,
                    deadline: http_deadline(limits.read_body_timeout),
                    handler_timeout: limits.handler_timeout,
                })));
            }
        }
        self.scanned = self.bytes.bytes.len();
        None
    }
}

impl Body {
    fn poll(&mut self, stream: &mut TcpStream) -> Option<Result<State, HttpWireResponse>> {
        let end = self
            .bytes
            .bytes
            .len()
            .min(self.received.saturating_add(READ_CHUNK));
        match stream.read(&mut self.bytes.bytes[self.received..end]) {
            Ok(0) => Some(Err(status_response(408, "Request Timeout"))),
            Ok(read) => {
                self.received += read;
                (self.received == self.bytes.bytes.len()).then(|| {
                    let budget = Arc::clone(&self.bytes.charge.budget);
                    let Buffer { bytes, charge } =
                        std::mem::replace(&mut self.bytes, Buffer::empty(&budget));
                    let mut input_charge = self.head_charge.take().expect("one head charge");
                    input_charge.merge(charge);
                    finish_http_request(self.head.take().expect("one body completion"), bytes).map(
                        |request| {
                            let ready_at = Instant::now();
                            State::Request {
                                request: Some(request),
                                deadline: ready_at
                                    .checked_add(self.handler_timeout)
                                    .unwrap_or(ready_at),
                                ready_at,
                                carry: Buffer::empty(&budget),
                                input_charge: Some(input_charge),
                            }
                        },
                    )
                })
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                None
            }
            Err(_) => Some(Err(status_response(408, "Request Timeout"))),
        }
    }
}

#[cfg(test)]
mod buffer_tests {
    use super::*;

    fn trace_fixture() -> HttpRequestTrace {
        let at = Instant::now();
        HttpRequestTrace {
            id: Some(u64::MAX),
            token: HttpIoToken {
                slot: 7,
                generation: 42,
            },
            accepted: Some(at),
            promoted: Some(at),
            read_started: Some(at),
            ready: at,
            handler_started: Some(at),
            handler_finished: Some(at),
            handler_cpu: Some(Duration::ZERO),
            published: Some(at),
            observed: Some(at),
            write_started: Some(at),
            status: 200,
            origin: HttpTraceOrigin::Response,
        }
    }

    #[test]
    fn trace_producers_finish_while_the_actual_writer_is_blocked() {
        struct BlockedWriter {
            entered: Option<SyncSender<()>>,
            release: Receiver<()>,
        }
        impl Write for BlockedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (sink, receiver) = HttpTraceSink::channel();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let writer = thread::spawn(move || {
            let mut output = BlockedWriter {
                entered: Some(entered_tx),
                release: release_rx,
            };
            receiver.recv().unwrap().write(&mut output).unwrap();
            receiver
        });
        sink.emit(HttpTraceEvent::Transport {
            trace: trace_fixture(),
            origin: HttpTraceOrigin::WriteFailed,
            ended: Instant::now(),
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let producer_sink = sink.clone();
        let producer = thread::spawn(move || {
            // More attempts than channel and event limits. The caller must
            // finish while the consumer is still blocked inside Write::write.
            for _ in 0..TRACE_TRANSPORT_LINES + TRACE_CHANNEL_CAPACITY {
                producer_sink.emit(HttpTraceEvent::Transport {
                    trace: trace_fixture(),
                    origin: HttpTraceOrigin::WriteFailed,
                    ended: Instant::now(),
                });
            }
            for _ in 0..HttpSlowTrace::MAX_LINES {
                producer_sink.emit(HttpTraceEvent::Slow {
                    token: HttpIoToken {
                        slot: 0,
                        generation: 1,
                    },
                    id: None,
                    sample: HttpSlowTraceSample {
                        queued: Duration::ZERO,
                        handler_wall: Duration::ZERO,
                        handler_cpu: None,
                    },
                    status: 504,
                });
            }
            finished_tx.send(()).unwrap();
        });
        let producer_finished = finished_rx.recv_timeout(Duration::from_secs(2));
        // Always release before assertions so a failure cannot retain a writer.
        release_tx.send(()).unwrap();
        let receiver = writer.join().unwrap();
        if producer_finished.is_err() {
            drop(receiver);
            producer.join().unwrap();
            panic!("diagnostics waited for the blocked writer");
        }
        producer.join().unwrap();
        assert_eq!(
            sink.state.transport.load(Ordering::Relaxed),
            TRACE_TRANSPORT_LINES
        );
        assert!(sink.state.dropped.load(Ordering::Relaxed) > 0);
        assert!(receiver.try_iter().count() <= TRACE_CHANNEL_CAPACITY);
    }

    #[test]
    fn trace_ids_and_stage_output_only_contain_numeric_metadata() {
        let mut request = ParsedHttpRequest {
            keep_alive: false,
            method: "PRIVATE_METHOD".into(),
            path: "/PRIVATE_PATH".into(),
            query: Default::default(),
            headers: Default::default(),
            body: "PRIVATE_BODY".into(),
        };
        for (text, expected) in [
            ("0", Some(0)),
            ("18446744073709551615", Some(u64::MAX)),
            ("18446744073709551616", None),
            ("-1", None),
            ("+1", None),
            ("1\nPRIVATE_HEADER", None),
            (" 1", None),
            ("1 ", None),
            ("", None),
            ("中文", None),
        ] {
            request.headers.insert("x-ku-trace-id".into(), text.into());
            assert_eq!(request_trace_id(&request), expected);
        }
        let mut output = Vec::new();
        for origin in [
            HttpTraceOrigin::QueueExpired,
            HttpTraceOrigin::HandlerExpired,
            HttpTraceOrigin::ReactorExpired,
            HttpTraceOrigin::WriteExpired,
            HttpTraceOrigin::WriteFailed,
            HttpTraceOrigin::LateReply,
        ] {
            HttpTraceEvent::Transport {
                trace: trace_fixture(),
                origin,
                ended: Instant::now(),
            }
            .write(&mut output)
            .unwrap();
        }
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.lines().count(), 6);
        assert!(output.contains("request_id=18446744073709551615"));
        for field in [
            "queue_expired",
            "handler_expired",
            "reactor_handler_expired",
            "write_expired",
            "write_failed",
            "late_reply",
            "publish_attempt_to_observe_us=",
            "response_phase_us=",
        ] {
            assert!(output.contains(field));
        }
        assert!(!output.contains("PRIVATE"));
        assert!(!output.contains("中文"));
    }

    #[test]
    fn trace_reports_wire_rejection_and_counts_suppressed_events() {
        let budget = BufferBudget::new(1);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (sink, events) = HttpTraceSink::channel();
        let limits = super::super::finally_tests::http_test_limits();
        let count = Arc::new(AtomicUsize::new(0));
        let permit = HttpConnectionPermit::try_acquire(Arc::clone(&count), 1).unwrap();
        let mut connection = Connection::new(
            PendingHttpConnection {
                stream,
                accepted_at: Instant::now(),
                _permit: permit,
            },
            limits,
            Arc::clone(&budget),
            Some(HttpTraceConfig {
                threshold: Duration::from_secs(10),
                sink: sink.clone(),
                counters: Arc::new(HttpTraceCounters::default()),
            }),
        );
        connection.trace = Some(trace_fixture());
        connection.state = State::Write(Writing::new(
            status_response(200, "ok"),
            Duration::from_secs(1),
            false,
            &budget,
        ));
        connection.emit_trace(HttpTraceOrigin::Response, Instant::now());
        let mut output = Vec::new();
        events.try_recv().unwrap().write(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("status=503"), "{output}");
        assert!(!output.contains("status=200"));
        for _ in 0..TRACE_TRANSPORT_LINES + 1 {
            sink.emit(HttpTraceEvent::Transport {
                trace: trace_fixture(),
                origin: HttpTraceOrigin::WriteFailed,
                ended: Instant::now(),
            });
        }
        assert!(sink.state.dropped.load(Ordering::Relaxed) >= 2);
        drop((connection, client));
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert_eq!(budget.snapshot().current, 0);
    }

    #[test]
    fn trace_queue_and_running_credits_retire_on_dequeue_and_abandonment() {
        let counters = Arc::new(HttpTraceCounters::default());
        let first = HttpTraceCredit::acquire(&counters, false, 120);
        let second = HttpTraceCredit::acquire(&counters, false, 80);
        let running = HttpTraceCredit::acquire(&counters, true, 0);
        assert_eq!(counters.queued.load(Ordering::Acquire), 2);
        assert_eq!(counters.queued_bytes.load(Ordering::Acquire), 200);
        drop(first);
        assert_eq!(counters.queued.load(Ordering::Acquire), 1);
        assert_eq!(counters.queued_bytes.load(Ordering::Acquire), 80);
        drop((second, running));
        assert_eq!(counters.queued.load(Ordering::Acquire), 0);
        assert_eq!(counters.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(counters.running.load(Ordering::Acquire), 0);
        assert_eq!(counters.peak_queued.load(Ordering::Relaxed), 2);
        assert_eq!(counters.peak_queued_bytes.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn traced_real_http_jobs_release_their_queue_bytes_and_late_reply_owners() {
        let mut limits = super::super::finally_tests::http_test_limits();
        limits.handler_timeout = Duration::from_secs(1);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sink, events) = HttpTraceSink::channel();
        let counters = Arc::new(HttpTraceCounters::default());
        let config = HttpTraceConfig {
            threshold: Duration::ZERO,
            sink,
            counters: Arc::clone(&counters),
        };
        let mut io = HttpIo::start_with_accept_and_trace(
            listener,
            limits,
            TcpListener::accept,
            Some(config),
        )
        .unwrap();
        let budget = Arc::clone(&io.budget);
        let request = |id| {
            let mut client = TcpStream::connect(address).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            write!(client, "GET /PRIVATE_PATH HTTP/1.1\r\nHost: localhost\r\nx-ku-trace-id: {id}\r\nConnection: close\r\n\r\n").unwrap();
            client
        };
        let mut first = request(42);
        let mut job = io.next_job(Duration::from_secs(2)).unwrap().unwrap();
        let published_flight = FlightRecord::new(Some(42), Instant::now(), job.deadline);
        job.reply.flight = Some(Arc::clone(&published_flight));
        assert_eq!(counters.queued.load(Ordering::Acquire), 0);
        assert_eq!(counters.queued_bytes.load(Ordering::Acquire), 0);
        job.reply.handler_started();
        drop(job.request);
        assert_eq!(counters.running.load(Ordering::Acquire), 1);
        job.reply
            .handler_finished(None, 200, HttpTraceOrigin::Response);
        job.reply
            .respond(job.token, status_response(200, "PRIVATE_BODY"))
            .unwrap();
        for stage in [
            Stage::Published,
            Stage::PublishReturned,
            Stage::WakeAttempt,
            Stage::WakeReturned,
        ] {
            assert!(published_flight.checkpoint_ns(stage).is_some(), "{stage:?}");
        }
        assert!(published_flight.checkpoint_ns(Stage::WakeFailed).is_none());
        let mut response = String::new();
        first.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 "));
        assert!(response.ends_with("PRIVATE_BODY"));
        let mut second = request(99);
        let mut late = io.next_job(Duration::from_secs(2)).unwrap().unwrap();
        // The actual socket owner expires without waiting for the reply owner.
        response.clear();
        second.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 504 "));
        assert_eq!(counters.reactor_expired.load(Ordering::Relaxed), 1);
        assert!(budget.snapshot().current > 0);
        late.reply.handler_started();
        drop(late.request);
        late.reply
            .handler_finished(None, 504, HttpTraceOrigin::QueueExpired);
        late.reply
            .respond(late.token, status_response(504, "Gateway Timeout"))
            .unwrap();
        drop(io);
        assert_eq!(counters.queued.load(Ordering::Acquire), 0);
        assert_eq!(counters.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(counters.running.load(Ordering::Acquire), 0);
        assert_eq!(counters.queue_expired.load(Ordering::Relaxed), 1);
        assert_eq!(counters.late_replies.load(Ordering::Relaxed), 1);
        assert_eq!(budget.snapshot().current, 0);
        assert!(counters.peak_queued_bytes.load(Ordering::Relaxed) > 0);
        let mut output = Vec::new();
        for event in events.try_iter() {
            event.write(&mut output).unwrap();
        }
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("request_id=42 origin=response status=200"));
        assert!(output.contains("request_id=99 origin=reactor_handler_expired status=504"));
        assert!(output.contains("request_id=99 origin=late_reply status=504"));
        assert!(!output.contains("PRIVATE"));

        // A real cap-1 channel accepts the completion even when the following
        // wake write fails. Preserve that original error return and distinguish
        // it from a failed publish; no reactor acknowledgement is assumed.
        let wake = netpoll::WakePair::new().unwrap();
        wake.sender.shutdown().unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let token = HttpIoToken {
            slot: 0,
            generation: 1,
        };
        let now = Instant::now();
        let failed_wake_flight = FlightRecord::new(Some(100), now, now + Duration::from_secs(1));
        let reply = HttpIoReply {
            token,
            sender,
            wake: wake.sender.clone(),
            input_charge: budget.charge(0).unwrap(),
            trace: None,
            trace_config: None,
            handler_credit: None,
            flight: Some(Arc::clone(&failed_wake_flight)),
        };
        assert!(reply
            .respond(token, status_response(200, "channel accepted"))
            .is_err());
        let completion = receiver.try_recv().unwrap();
        assert_eq!(completion.token, token);
        for stage in [
            Stage::Published,
            Stage::PublishReturned,
            Stage::WakeAttempt,
            Stage::WakeFailed,
        ] {
            assert!(
                failed_wake_flight.checkpoint_ns(stage).is_some(),
                "{stage:?}"
            );
        }
        assert!(failed_wake_flight
            .checkpoint_ns(Stage::WakeReturned)
            .is_none());
        assert!(failed_wake_flight.checkpoint_ns(Stage::Observed).is_none());
        drop(completion);
        assert_eq!(budget.snapshot().current, 0);
    }

    #[test]
    fn trace_snapshots_have_an_independent_finite_allowance_and_show_zero_owners() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let limits = super::super::finally_tests::http_test_limits();
        let (jobs, _receiver) = mpsc::sync_channel(limits.max_active_requests);
        let budget = BufferBudget::new(limits.max_buffer_bytes);
        let mut reactor = Reactor::new(
            listener,
            limits,
            jobs,
            netpoll::WakePair::new().unwrap(),
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&budget),
        );
        let (sink, events) = HttpTraceSink::channel();
        reactor.trace = Some(HttpTraceConfig {
            threshold: Duration::from_millis(1),
            sink: sink.clone(),
            counters: Arc::new(HttpTraceCounters::default()),
        });
        let start = Instant::now();
        let mut output = Vec::new();
        for index in 0..TRACE_SNAPSHOT_LINES + 2 {
            reactor.trace_snapshot(start + TRACE_SNAPSHOT_INTERVAL * (index as u32));
            for event in events.try_iter() {
                event.write(&mut output).unwrap();
            }
        }
        assert_eq!(
            sink.state.snapshots.load(Ordering::Relaxed),
            TRACE_SNAPSHOT_LINES
        );
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.lines().count(), TRACE_SNAPSHOT_LINES);
        assert!(output
            .lines()
            .all(|line| line.contains("connections=0 pending=0 active=0")));
        assert!(output
            .lines()
            .all(|line| line.contains("queued=0 queued_bytes=0")));
        assert!(output
            .lines()
            .all(|line| line.contains("running=0 buffer_current=0")));
    }

    #[test]
    fn failed_growth_preserves_old_credit_and_clear_retains_capacity() {
        let budget = BufferBudget::new(96);
        let mut bytes = Buffer::allocate(64, &budget).unwrap();
        bytes.bytes.resize(64, 0);
        assert!(bytes.grow(128).is_none());
        bytes.bytes.clear();
        assert_eq!(budget.snapshot().current, 64);
        assert_eq!(budget.snapshot().peak, 64);
        assert_eq!(budget.snapshot().rejected, 1);
        drop(bytes);
        assert_eq!(budget.snapshot().current, 0);
    }

    #[test]
    fn pending_connection_does_not_evict_a_reused_socket_with_unread_request_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let limits = HttpServerRuntimeLimits {
            read_header_timeout: Duration::from_secs(1),
            read_body_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            handler_timeout: Duration::from_secs(1),
            max_header_bytes: 1024,
            max_body_bytes: 1024,
            max_connections: 2,
            max_active_requests: 1,
            max_pending_requests: 1,
            max_buffer_bytes: 65536,
        };
        let (jobs, _receiver) = mpsc::sync_channel(1);
        let budget = BufferBudget::new(limits.max_buffer_bytes);
        let mut reactor = Reactor::new(
            listener,
            limits,
            jobs,
            netpoll::WakePair::new().unwrap(),
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&budget),
        );
        let count = Arc::clone(&reactor.connection_count);
        let mut first_peer = TcpStream::connect(address).unwrap();
        let (first, _) = reactor.listener.accept().unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        first_peer
            .write_all(b"GET /next HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        // Observe actual kernel receive bytes before invoking promotion: this
        // reproduces the ready-before-next-reactor-pass ordering without sleeps.
        assert_eq!(first.peek(&mut [0; 1]).unwrap(), 1);
        first.set_read_timeout(None).unwrap();
        first.set_nonblocking(true).unwrap();
        let mut connection = Connection::new(
            PendingHttpConnection {
                stream: first,
                accepted_at: Instant::now(),
                _permit: HttpConnectionPermit::try_acquire(Arc::clone(&count), 2).unwrap(),
            },
            limits,
            Arc::clone(&budget),
            None,
        );
        connection.reused = true;
        reactor.slots[0].generation = 7;
        reactor.slots[0].connection = Some(connection);
        reactor.free.clear();
        let second_peer = TcpStream::connect(address).unwrap();
        let (second, _) = reactor.listener.accept().unwrap();
        second.set_nonblocking(true).unwrap();
        reactor.pending.push_back(PendingHttpConnection {
            stream: second,
            accepted_at: Instant::now(),
            _permit: HttpConnectionPermit::try_acquire(Arc::clone(&count), 2).unwrap(),
        });
        reactor.promote_pending().unwrap();
        let generation = reactor.slots[0].generation;
        let pending_count = reactor.pending.len();
        let kept_bytes = reactor.slots[0]
            .connection
            .as_ref()
            .map(|connection| connection.stream.peek(&mut [0; 1]).unwrap_or(0));
        drop(reactor);
        drop((first_peer, second_peer));
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert_eq!(budget.snapshot().current, 0);
        assert_eq!(generation, 7, "pending promotion evicted a ready request");
        assert_eq!(pending_count, 1);
        assert_eq!(kept_bytes, Some(1));
    }

    #[test]
    fn replacement_growth_charges_the_actual_coexisting_buffers() {
        let budget = BufferBudget::new(192);
        let mut bytes = Buffer::allocate(64, &budget).unwrap();
        bytes.bytes.extend_from_slice(b"kept");
        bytes.grow(128).unwrap();
        assert_eq!(bytes.bytes, b"kept");
        assert_eq!(budget.snapshot().current, 128);
        assert_eq!(budget.snapshot().peak, 192);
        drop(bytes);
        assert_eq!(budget.snapshot().current, 0);
    }
}
