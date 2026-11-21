use std::{
    io,
    sync::atomic::{AtomicI32, Ordering},
};

const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];
static INTERRUPTING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn record(signal: libc::c_int) {
    // The handler only latches the first signal. The build worker still owns
    // spawning, group termination, and reaping, including a spawn in progress.
    let _ = INTERRUPTING_SIGNAL.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed);
}

pub(super) fn received() -> libc::c_int {
    INTERRUPTING_SIGNAL.load(Ordering::Relaxed)
}

pub(super) struct BuildInterruptHandlers {
    previous: [Option<libc::sigaction>; SIGNALS.len()],
}

impl BuildInterruptHandlers {
    pub(super) fn install() -> io::Result<Self> {
        let mut handlers = Self {
            previous: [None, None],
        };
        for (slot, signal) in handlers.previous.iter_mut().zip(SIGNALS) {
            // SAFETY: the C structs are plain data, and sigaction initializes
            // the previous disposition when called with a writable output.
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // Preserve a signal already ignored or handled by the host.
            if previous.sa_sigaction != libc::SIG_DFL {
                continue;
            }
            // SAFETY: an empty mask and an extern C handler form a valid action.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = record as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0
                || unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            *slot = Some(previous);
        }
        Ok(handlers)
    }
}

impl Drop for BuildInterruptHandlers {
    fn drop(&mut self) {
        for (previous, signal) in self.previous.iter().zip(SIGNALS) {
            if let Some(previous) = previous {
                // SAFETY: restore the exact action read for this signal.
                unsafe { libc::sigaction(signal, previous, std::ptr::null_mut()) };
            }
        }
    }
}

pub(super) fn exit_with_signal(signal: libc::c_int) -> ! {
    // SAFETY: the build has returned and restored its signal dispositions.
    // Re-deliver the recorded signal to the current thread so a caller still
    // observes signal termination. SIGKILL cannot run this cleanup path.
    unsafe { libc::raise(signal) };
    std::process::exit(128 + signal)
}
