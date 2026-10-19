//! One socket owner for the interpreter HTTP transport. Ku handlers and their
//! Values remain on the caller; only parsed/wire data crosses this boundary.

use super::{
    encode_http_response, entry_span, finish_http_request, http_deadline, netpoll, parse_http_head,
    status_response, HttpConnectionPermit, HttpRejection, HttpServerRuntimeLimits,
    HttpWireResponse, ParsedHttpHead, ParsedHttpRequest, PendingHttpConnection, HTTP_ACCEPT_BATCH,
};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
        Arc,
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

#[cfg(test)]
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

    #[cfg(test)]
    fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            current: self.current.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            limit: self.limit,
        }
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
}

pub(super) struct HttpIoReply {
    token: HttpIoToken,
    sender: SyncSender<Completion>,
    wake: netpoll::WakeSender,
    input_charge: BufferCharge,
}

impl HttpIoReply {
    pub(super) fn respond(self, token: HttpIoToken, response: HttpWireResponse) -> io::Result<()> {
        if token != self.token {
            return Err(io::Error::other("HTTP reply token does not match its job"));
        }
        let budget = Arc::clone(&self.input_charge.budget);
        // The handler has actually returned. A socket timeout alone never
        // releases the request charge while this reply owner is still alive.
        drop(self.input_charge);
        let response = ChargedResponse::new(response, &budget);
        // One consumed reply owns one initially empty cap-1 channel. A closed
        // receiver means timeout/close already won: dropping the response is
        // the complete late-result path, without touching any socket.
        match self.sender.try_send(Completion { token, response }) {
            Ok(()) => self.wake.notify(),
            Err(TrySendError::Disconnected(_)) => Ok(()),
            Err(TrySendError::Full(_)) => Err(io::Error::other("HTTP reply published twice")),
        }
    }
}

pub(super) struct HttpIoJob {
    pub(super) token: HttpIoToken,
    pub(super) request: ParsedHttpRequest,
    pub(super) deadline: Instant,
    pub(super) reply: HttpIoReply,
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
        Self::start_with_accept(listener, limits, TcpListener::accept)
    }

    // Keep the real reactor and socket ownership path available to controlled
    // accept-error regressions, without replacing readiness or connection I/O.
    pub(super) fn start_with_accept<F>(
        listener: TcpListener,
        limits: HttpServerRuntimeLimits,
        accept: F,
    ) -> io::Result<Self>
    where
        F: FnMut(&TcpListener) -> io::Result<(TcpStream, std::net::SocketAddr)> + Send + 'static,
    {
        listener.set_nonblocking(true)?;
        let wake_pair = netpoll::WakePair::new()?;
        let wake = wake_pair.sender.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (job_tx, jobs) = mpsc::sync_channel(limits.max_active_requests);
        let budget = BufferBudget::new(limits.max_buffer_bytes);
        let reactor = Reactor::new(
            listener,
            limits,
            job_tx,
            wake_pair,
            Arc::clone(&stop),
            Arc::clone(&budget),
        );
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
            Ok(job) => Ok(Some(job)),
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
        }
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
            let mut deadline = None;
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
            stream.set_nonblocking(true)?;
            let permit = HttpConnectionPermit::try_acquire(
                Arc::clone(&self.connection_count),
                self.limits.max_connections,
            );
            if permit.is_none() || self.pending.len() >= self.limits.max_pending_requests {
                self.rejection_generation = self
                    .rejection_generation
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("HTTP rejection generation exhausted"))?;
                self.rejecting = HttpRejection::new(stream, permit, self.limits);
                continue;
            }
            self.pending.push_back(PendingHttpConnection {
                stream,
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
                },
            };
            match self.jobs.try_send(job) {
                Ok(()) => connection.state = State::Handler { receiver, deadline },
                Err(TrySendError::Full(_)) => {
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
    ) -> Self {
        Self {
            stream: pending.stream,
            _permit: pending._permit,
            state: State::Header(Header {
                bytes: Buffer::empty(&budget),
                scanned: 0,
                eof: false,
                idle_deadline: http_deadline(limits.idle_timeout),
                header_deadline: None,
            }),
            carry: Buffer::empty(&budget),
            budget,
            keep_alive: false,
            reused: false,
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
            State::Handler { .. } => status_response(504, "Gateway Timeout"),
            State::Write(_) => return true,
            State::Request { .. } => return false,
        };
        self.keep_alive = false;
        self.carry = Buffer::empty(&self.budget);
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
                if self.reused && header.eof {
                    return true;
                }
                next
            }
            State::Body(body) => body.poll(&mut self.stream),
            State::Write(write) => {
                return match write.poll(&mut self.stream) {
                    WriteProgress::Pending => false,
                    WriteProgress::Failed => true,
                    WriteProgress::Complete if !write.keep_alive || !allow_reuse => true,
                    WriteProgress::Complete => {
                        let bytes = std::mem::replace(&mut self.carry, Buffer::empty(&self.budget));
                        let header_deadline = (!bytes.bytes.is_empty())
                            .then(|| http_deadline(limits.read_header_timeout));
                        self.state = State::Header(Header {
                            bytes,
                            scanned: 0,
                            eof: false,
                            idle_deadline: http_deadline(limits.idle_timeout),
                            header_deadline,
                        });
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
                    return Some(
                        finish_http_request(head, body).map(|request| State::Request {
                            request: Some(request),
                            deadline: http_deadline(limits.handler_timeout),
                            carry: prefix,
                            input_charge: Some(head_charge),
                        }),
                    );
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
                        |request| State::Request {
                            request: Some(request),
                            deadline: http_deadline(self.handler_timeout),
                            carry: Buffer::empty(&budget),
                            input_charge: Some(input_charge),
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
