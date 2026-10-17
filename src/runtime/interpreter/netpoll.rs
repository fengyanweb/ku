use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(unix)]
use libc::{pollfd as PollFd, POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    POLLERR, POLLHUP, POLLNVAL, POLLRDNORM as POLLIN, POLLWRNORM as POLLOUT, WSAPOLLFD as PollFd,
};

const MAX_INTERESTS: usize = 4098;
const WAKE_INIT_TIMEOUT: Duration = Duration::from_secs(1);
const WAKE_DRAIN_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub(super) enum Socket<'a> {
    Listener(&'a TcpListener),
    Stream(&'a TcpStream),
}

pub(super) struct Interest<'a> {
    pub socket: Socket<'a>,
    pub read: bool,
    pub write: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ready {
    pub index: usize,
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
}

/// Wait for one batch, retaining the borrowed socket owners until the OS call
/// has finished. Empty batches return immediately rather than waiting without
/// a descriptor that could wake the caller.
pub(super) fn wait(
    interests: &[Interest<'_>],
    deadline: Option<Instant>,
) -> io::Result<Vec<Ready>> {
    if interests.len() > MAX_INTERESTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket poll batch exceeds 4098 entries",
        ));
    }
    if interests.is_empty() {
        return Ok(Vec::new());
    }
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(interests.len())
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "socket poll allocation failed"))?;
    for interest in interests {
        if !interest.read && !interest.write {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket poll entry has no read or write interest",
            ));
        }
        descriptors.push(poll_descriptor(interest));
    }
    loop {
        for descriptor in &mut descriptors {
            descriptor.revents = 0;
        }
        let count = match poll_descriptors(&mut descriptors, timeout_millis(deadline)) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if deadline.is_some_and(|limit| Instant::now() >= limit) {
                    return Ok(Vec::new());
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        if count == 0 {
            if deadline.is_some_and(|limit| Instant::now() < limit) {
                // Very long waits are split at the OS signed millisecond limit;
                // interruptions and rounding never replace the original limit.
                continue;
            }
            return Ok(Vec::new());
        }
        if descriptors.iter().any(|item| item.revents & POLLNVAL != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket poll received an invalid descriptor",
            ));
        }
        let mut ready = Vec::new();
        ready.try_reserve_exact(count).map_err(|_| {
            io::Error::new(io::ErrorKind::OutOfMemory, "socket poll allocation failed")
        })?;
        for (index, descriptor) in descriptors.iter().enumerate() {
            let events = descriptor.revents;
            if events == 0 {
                continue;
            }
            let error = events & (POLLERR | POLLHUP) != 0;
            ready.push(Ready {
                index,
                // A peer may close after sending its final bytes. Let the
                // caller drain those bytes before acting on the closure.
                readable: events & POLLIN != 0 || error,
                writable: events & POLLOUT != 0,
                error,
            });
        }
        return Ok(ready);
    }
}

fn timeout_millis(deadline: Option<Instant>) -> i32 {
    let Some(deadline) = deadline else {
        return -1;
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let millis = remaining.as_nanos().div_ceil(1_000_000);
    millis.min(i32::MAX as u128) as i32
}

#[cfg(unix)]
fn poll_descriptor(interest: &Interest<'_>) -> PollFd {
    use std::os::fd::AsRawFd;

    let fd = match interest.socket {
        Socket::Listener(listener) => listener.as_raw_fd(),
        Socket::Stream(stream) => stream.as_raw_fd(),
    };
    PollFd {
        fd,
        events: if interest.read { POLLIN } else { 0 } | if interest.write { POLLOUT } else { 0 },
        revents: 0,
    }
}

#[cfg(unix)]
fn poll_descriptors(descriptors: &mut [PollFd], timeout: i32) -> io::Result<usize> {
    // The vector remains exclusively borrowed and every fd owner is borrowed
    // by the input interests for the complete syscall.
    let count = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            timeout,
        )
    };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

#[cfg(windows)]
fn poll_descriptor(interest: &Interest<'_>) -> PollFd {
    use std::os::windows::io::AsRawSocket;

    let fd = match interest.socket {
        Socket::Listener(listener) => listener.as_raw_socket(),
        Socket::Stream(stream) => stream.as_raw_socket(),
    };
    PollFd {
        fd: fd as _,
        // WSAPoll does not accept POLLPRI. Only normal TCP data is requested.
        events: if interest.read { POLLIN } else { 0 } | if interest.write { POLLOUT } else { 0 },
        revents: 0,
    }
}

#[cfg(windows)]
fn poll_descriptors(descriptors: &mut [PollFd], timeout: i32) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{WSAGetLastError, WSAPoll, WSAEINTR};

    // The validated batch size fits u32, and all socket owners outlive the call.
    let count = unsafe { WSAPoll(descriptors.as_mut_ptr(), descriptors.len() as u32, timeout) };
    if count < 0 {
        let code = unsafe { WSAGetLastError() };
        if code == WSAEINTR {
            Err(io::ErrorKind::Interrupted.into())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    } else {
        Ok(count as usize)
    }
}

pub(super) struct WakePair {
    pub reader: TcpStream,
    pub sender: WakeSender,
}

#[derive(Clone)]
pub(super) struct WakeSender {
    stream: Arc<TcpStream>,
}

impl WakePair {
    pub(super) fn new() -> io::Result<Self> {
        let deadline = Instant::now() + WAKE_INIT_TIMEOUT;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(wake_init_timeout)?;
        let stream = TcpStream::connect_timeout(&address, remaining)?;
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        let expected_peer = stream.local_addr()?;
        loop {
            if Instant::now() >= deadline {
                return Err(wake_init_timeout());
            }
            match listener.accept() {
                Ok((reader, peer)) => {
                    if peer != expected_peer {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "socket poll wake channel received an unexpected peer",
                        ));
                    }
                    reader.set_nonblocking(true)?;
                    return Ok(Self {
                        reader,
                        sender: WakeSender {
                            stream: Arc::new(stream),
                        },
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if wait(
                        &[Interest {
                            socket: Socket::Listener(&listener),
                            read: true,
                            write: false,
                        }],
                        Some(deadline),
                    )?
                    .is_empty()
                    {
                        return Err(wake_init_timeout());
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Consume a bounded amount so a stream of notifications cannot starve
    /// socket processing. Remaining notifications keep the reader ready.
    pub(super) fn drain(&self) -> io::Result<()> {
        let mut reader = &self.reader;
        let mut buffer = [0; 512];
        for _ in 0..WAKE_DRAIN_BYTES / buffer.len() {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "socket poll wake channel closed",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

impl WakeSender {
    pub(super) fn notify(&self) -> io::Result<()> {
        let mut stream = &*self.stream;
        // A transient signal must not stop a live listener. Bound retries so
        // repeated interruptions cannot turn notification into a busy loop;
        // the caller retains its stop/shutdown path after an actual error.
        for attempt in 0..8 {
            match stream.write(&[1]) {
                Ok(1) => return Ok(()),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "socket poll wake channel write failed",
                    ));
                }
                // A full nonblocking buffer already contains a pending wakeup.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted && attempt < 7 => {}
                Err(error) => return Err(error),
            }
        }
        unreachable!("the final notification attempt always returns")
    }

    /// Closing the sending half wakes a reader even when a stop notification
    /// could not be written. Callers use this only when stopping the reactor;
    /// it applies to every sender clone sharing the same socket.
    pub(super) fn shutdown(&self) -> io::Result<()> {
        self.stream.shutdown(Shutdown::Write)
    }
}

fn wake_init_timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "socket poll wake channel initialization timed out",
    )
}
