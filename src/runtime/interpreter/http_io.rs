//! One socket owner for the interpreter HTTP transport. Ku handlers and their
//! Values remain on the caller; only parsed/wire data crosses this boundary.

use super::{
    encode_http_response, entry_span, finish_http_request, http_deadline, netpoll, parse_http_head,
    status_response, HttpConnectionPermit, HttpRejection, HttpServerRuntimeLimits,
    HttpWireResponse, ParsedHttpHead, ParsedHttpRequest, PendingHttpConnection, HTTP_ACCEPT_BATCH,
};
use std::{
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HttpIoToken {
    slot: usize,
    generation: u64,
}

struct Completion {
    token: HttpIoToken,
    response: HttpWireResponse,
}

pub(super) struct HttpIoReply {
    token: HttpIoToken,
    sender: SyncSender<Completion>,
    wake: netpoll::WakeSender,
}

impl HttpIoReply {
    pub(super) fn respond(self, token: HttpIoToken, response: HttpWireResponse) -> io::Result<()> {
        if token != self.token {
            return Err(io::Error::other("HTTP reply token does not match its job"));
        }
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
}

impl HttpIo {
    pub(super) fn start(
        listener: TcpListener,
        limits: HttpServerRuntimeLimits,
    ) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let wake_pair = netpoll::WakePair::new()?;
        let wake = wake_pair.sender.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (job_tx, jobs) = mpsc::sync_channel(limits.max_active_requests);
        let reactor = Reactor::new(listener, limits, job_tx, wake_pair, Arc::clone(&stop));
        let thread = thread::Builder::new()
            .name("ku-http-io".into())
            .spawn(move || reactor.run())?;
        Ok(Self {
            jobs,
            stop,
            wake,
            thread: Some(thread),
        })
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
        }
    }

    fn run(mut self) -> io::Result<()> {
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
                interests.push(netpoll::Interest {
                    socket: netpoll::Socket::Listener(&self.listener),
                    read: true,
                    write: false,
                });
                owners.push(PollOwner::Listener);
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
                    PollOwner::Listener => self.accept_connections()?,
                    PollOwner::Connection(token) => {
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
                            && connection.poll(self.limits)
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
                Ok(_) | Err(TryRecvError::Disconnected) => {
                    Some(status_response(500, "Internal Server Error"))
                }
                Err(TryRecvError::Empty) => None,
            };
            if let Some(response) = response {
                connection.state = State::Write(Writing::new(response, self.limits.write_timeout));
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
            slot.connection = Some(Connection::new(pending, self.limits));
        }
        Ok(())
    }

    fn accept_connections(&mut self) -> io::Result<()> {
        for _ in 0..HTTP_ACCEPT_BATCH {
            if self.rejecting.is_some() || self.stop.load(Ordering::Acquire) {
                break;
            }
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
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

    fn dispatch_requests(&mut self) -> io::Result<()> {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(connection) = slot.connection.as_mut() else {
                continue;
            };
            let State::Request { request, deadline } = &mut connection.state else {
                continue;
            };
            let request = request.take().expect("one request dispatch");
            let deadline = *deadline;
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
                },
            };
            match self.jobs.try_send(job) {
                Ok(()) => connection.state = State::Handler { receiver, deadline },
                Err(TrySendError::Full(_)) => {
                    connection.state = State::Write(Writing::new(
                        status_response(503, "Service Unavailable"),
                        self.limits.write_timeout,
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

fn tighten_deadline(saved: &mut Option<Instant>, deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        *saved = Some(saved.map_or(deadline, |previous| previous.min(deadline)));
    }
}

struct Connection {
    stream: TcpStream,
    _permit: HttpConnectionPermit,
    state: State,
}

enum State {
    Header(Header),
    Body(Body),
    Request {
        request: Option<ParsedHttpRequest>,
        deadline: Instant,
    },
    Handler {
        receiver: Receiver<Completion>,
        deadline: Instant,
    },
    Write(Writing),
}

struct Header {
    bytes: Vec<u8>,
    idle_deadline: Instant,
    header_deadline: Option<Instant>,
}

struct Body {
    head: Option<ParsedHttpHead>,
    bytes: Vec<u8>,
    received: usize,
    deadline: Instant,
    handler_timeout: Duration,
}

struct Writing {
    head: String,
    body: String,
    head_sent: usize,
    body_sent: usize,
    deadline: Instant,
}

impl Writing {
    fn new(response: HttpWireResponse, timeout: Duration) -> Self {
        let (head, body) = encode_http_response(response);
        Self {
            head,
            body,
            head_sent: 0,
            body_sent: 0,
            deadline: http_deadline(timeout),
        }
    }

    fn poll(&mut self, stream: &mut TcpStream) -> bool {
        let (bytes, offset) = if self.head_sent < self.head.len() {
            (self.head.as_bytes(), &mut self.head_sent)
        } else {
            (self.body.as_bytes(), &mut self.body_sent)
        };
        if *offset == bytes.len() {
            return true;
        }
        let end = bytes.len().min(offset.saturating_add(READ_CHUNK));
        match stream.write(&bytes[*offset..end]) {
            Ok(0) => true,
            Ok(written) => {
                *offset += written;
                self.head_sent == self.head.len() && self.body_sent == self.body.len()
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                false
            }
            Err(_) => true,
        }
    }
}

impl Connection {
    fn new(pending: PendingHttpConnection, limits: HttpServerRuntimeLimits) -> Self {
        Self {
            stream: pending.stream,
            _permit: pending._permit,
            state: State::Header(Header {
                bytes: Vec::new(),
                idle_deadline: http_deadline(limits.idle_timeout),
                header_deadline: None,
            }),
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
        self.state = State::Write(Writing::new(response, limits.write_timeout));
        false
    }

    fn poll(&mut self, limits: HttpServerRuntimeLimits) -> bool {
        if self.expire(Instant::now(), limits) {
            return true;
        }
        let next = match &mut self.state {
            State::Header(header) => header.poll(&mut self.stream, limits),
            State::Body(body) => body.poll(&mut self.stream),
            State::Write(write) => return write.poll(&mut self.stream),
            State::Request { .. } | State::Handler { .. } => None,
        };
        if let Some(next) = next {
            self.state = match next {
                Ok(state) => state,
                Err(response) => State::Write(Writing::new(response, limits.write_timeout)),
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
        let mut incoming = [0; READ_CHUNK];
        let read = match stream.read(&mut incoming) {
            Ok(0) => return Some(Err(status_response(400, "Bad Request"))),
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
        let start = self.bytes.len();
        self.bytes.extend_from_slice(&incoming[..read]);
        if start == 0 {
            self.header_deadline = Some(http_deadline(limits.read_header_timeout));
        }
        for index in start..self.bytes.len() {
            // Preserve the original error precedence: byte max+1 is 431,
            // including when the same byte also violates line framing.
            if index + 1 > limits.max_header_bytes {
                return Some(Err(status_response(431, "Request Header Fields Too Large")));
            }
            if (self.bytes[index] == b'\n' && (index < 1 || self.bytes[index - 1] != b'\r'))
                || (index >= 1 && self.bytes[index - 1] == b'\r' && self.bytes[index] != b'\n')
            {
                return Some(Err(status_response(400, "Bad Request")));
            }
            if index >= 3 && self.bytes[index - 3..=index] == *b"\r\n\r\n" {
                let prefix = self.bytes.split_off(index + 1);
                self.bytes.truncate(index - 3);
                let head =
                    match parse_http_head(std::mem::take(&mut self.bytes), limits, entry_span()) {
                        Ok(head) => head,
                        Err(response) => return Some(Err(response)),
                    };
                let mut bytes = vec![0; head.content_length];
                let received = prefix.len().min(bytes.len());
                bytes[..received].copy_from_slice(&prefix[..received]);
                if received == bytes.len() {
                    return Some(
                        finish_http_request(head, bytes).map(|request| State::Request {
                            request: Some(request),
                            deadline: http_deadline(limits.handler_timeout),
                        }),
                    );
                }
                return Some(Ok(State::Body(Body {
                    head: Some(head),
                    bytes,
                    received,
                    deadline: http_deadline(limits.read_body_timeout),
                    handler_timeout: limits.handler_timeout,
                })));
            }
        }
        None
    }
}

impl Body {
    fn poll(&mut self, stream: &mut TcpStream) -> Option<Result<State, HttpWireResponse>> {
        let end = self
            .bytes
            .len()
            .min(self.received.saturating_add(READ_CHUNK));
        match stream.read(&mut self.bytes[self.received..end]) {
            Ok(0) => Some(Err(status_response(408, "Request Timeout"))),
            Ok(read) => {
                self.received += read;
                (self.received == self.bytes.len()).then(|| {
                    finish_http_request(
                        self.head.take().expect("one body completion"),
                        std::mem::take(&mut self.bytes),
                    )
                    .map(|request| State::Request {
                        request: Some(request),
                        deadline: http_deadline(self.handler_timeout),
                    })
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
