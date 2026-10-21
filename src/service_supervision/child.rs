use super::{command_error, platform, CLEANUP_GRACE, STOP_REQUESTED};
use crate::error::KuError;
use std::{
    io::{self, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::Ordering,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub(super) enum ManagedChild {
    Ordinary(OrdinaryChild),
    Limited(platform::Worker),
}

impl ManagedChild {
    fn child(&mut self) -> &mut Child {
        match self {
            Self::Ordinary(process) => &mut process.child,
            Self::Limited(process) => &mut process.child,
        }
    }

    pub(super) fn id(&mut self) -> u32 {
        self.child().id()
    }

    pub(super) fn release(&mut self) -> io::Result<()> {
        self.child()
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("missing launch gate"))?
            .write_all(b"K")
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Ordinary(process) => process.observe_exit(),
            Self::Limited(process) => process.child.try_wait(),
        }
    }

    pub(super) fn terminate(&mut self) -> Result<(), platform::ProviderError> {
        match self {
            Self::Ordinary(process) => process.request_exit(),
            Self::Limited(process) => process.terminate_group(),
        }
    }

    pub(super) fn cleanup(&mut self, deadline: Instant) -> Result<(), platform::ProviderError> {
        match self {
            Self::Ordinary(process) => process.cleanup(deadline),
            Self::Limited(process) => process.cleanup(deadline).map(|metrics| {
                // The provider retains its own lease until verified retirement.
                // Read all fields so the shared provider remains build-clean in
                // this second consumer, without inventing RSS measurements.
                let _ = (
                    metrics.limit_metric,
                    metrics.memory_peak_bytes,
                    metrics.memory_current_bytes,
                    metrics.oom_kill_count,
                    metrics.memory_limit_event,
                    metrics.memory_limit_termination,
                    metrics.process_count,
                    metrics.resource_group,
                );
            }),
        }
    }

    pub(super) fn metrics(&self) -> Result<Option<platform::Metrics>, platform::ProviderError> {
        match self {
            Self::Ordinary(_) => Ok(None),
            Self::Limited(process) => process.snapshot().map(Some),
        }
    }

    /// Cleanup has already reaped the launcher into std::process::Child's
    /// cached status. This does not reopen or signal a numeric PID.
    pub(super) fn reaped_status(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child().try_wait()
    }
}

pub(super) struct OrdinaryChild {
    pub(super) child: Child,
    retired: bool,
    #[cfg(unix)]
    group_terminated: bool,
    #[cfg(windows)]
    job: win::Job,
}

impl OrdinaryChild {
    pub(super) fn spawn(command: &mut Command) -> Result<Self, platform::ProviderError> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        #[cfg(windows)]
        let job = match win::Job::assign(&child) {
            Ok(job) => job,
            Err(error) => {
                // The internal launcher is still blocked at its stdin gate;
                // no user program has started or created descendants.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        // On Unix the launcher observes this lifetime pipe; on Windows the Job
        // closes with its supervisor handle. Neither runs application code
        // before the parent's explicit one-byte release.
        if child.stdin.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(platform::ProviderError::new("service launch gate missing"));
        }
        Ok(Self {
            child,
            retired: false,
            #[cfg(unix)]
            group_terminated: false,
            #[cfg(windows)]
            job,
        })
    }

    pub(super) fn request_exit(&mut self) -> Result<(), platform::ProviderError> {
        // EOF asks the launcher to terminate and reap its target. Keep that
        // launcher alive long enough to reap, instead of killing both at once.
        self.child.stdin.take();
        Ok(())
    }

    fn observe_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            // WNOWAIT retains the group leader as an unreaped child until the
            // final group termination. Its PID/PGID therefore cannot be reused
            // for an unrelated process between observation and cleanup.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
            #[cfg(not(target_os = "linux"))]
            let (pid, status) = (info.si_pid, info.si_status);
            if pid == 0 {
                return Ok(None);
            }
            let raw = if info.si_code == libc::CLD_EXITED {
                status << 8
            } else if info.si_code == libc::CLD_DUMPED {
                status | 0x80
            } else {
                status
            };
            Ok(Some(ExitStatus::from_raw(raw)))
        }
        #[cfg(not(unix))]
        self.child.try_wait()
    }

    pub(super) fn cleanup(&mut self, deadline: Instant) -> Result<(), platform::ProviderError> {
        if self.retired {
            return Ok(());
        }
        self.request_exit()?;
        let soft_deadline = (Instant::now() + Duration::from_millis(500)).min(deadline);
        while self.observe_exit()?.is_none() && Instant::now() < soft_deadline {
            thread::sleep(Duration::from_millis(5));
        }
        #[cfg(unix)]
        {
            // The process group is created before exec. Known native services
            // do not escape that group; host containment covers deliberately
            // detached grandchildren (documented in the service manager units).
            let group = i32::try_from(self.child.id())
                .map_err(|_| platform::ProviderError::new("invalid service process group"))?;
            if !self.group_terminated {
                let leader_exited = self.observe_exit()?.is_some();
                unsafe {
                    if libc::kill(-group, libc::SIGKILL) != 0 {
                        let error = io::Error::last_os_error();
                        // Darwin returns EPERM for a group consisting only of
                        // an unreaped zombie. Reap our retained leader, then
                        // still require ESRCH below; EPERM is never itself a
                        // successful group-retirement receipt.
                        if error.raw_os_error() != Some(libc::ESRCH)
                            && !(cfg!(target_os = "macos")
                                && leader_exited
                                && error.raw_os_error() == Some(libc::EPERM))
                        {
                            return Err(platform::ProviderError::new(format!(
                                "ordinary service group termination: {error}"
                            )));
                        }
                    }
                }
                self.group_terminated = true;
            }
        }
        #[cfg(windows)]
        self.job.terminate()?;
        loop {
            let reaped = self.child.try_wait()?.is_some();
            #[cfg(windows)]
            let empty = self.job.empty()?;
            #[cfg(unix)]
            let empty = unsafe {
                let result = libc::kill(-(self.child.id() as i32), 0);
                if result == 0 {
                    false
                } else if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    true
                } else {
                    return Err(platform::ProviderError::new(format!(
                        "ordinary service group retirement check: {}",
                        io::Error::last_os_error()
                    )));
                }
            };
            #[cfg(not(any(unix, windows)))]
            let empty = true;
            if reaped && empty {
                self.retired = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(platform::ProviderError {
                    detail: "service process cleanup deadline exceeded; replacement refused".into(),
                    cleanup_confirmed: false,
                });
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for OrdinaryChild {
    fn drop(&mut self) {
        if !self.retired {
            // Errors are propagated by all normal paths. Drop is the last
            // bounded cleanup attempt during unwind, not a successful receipt.
            let _ = self.cleanup(Instant::now() + CLEANUP_GRACE);
        }
    }
}

/// Internal gated launcher. The service inherits no stdin; a separate lifetime
/// pipe stays owned by Ku so supervisor loss cannot leave a normal native
/// service running. This command has no user-facing scheduling semantics.
pub fn run_service_child(args: &[String]) -> Result<(), KuError> {
    if args.get(2).map(String::as_str) != Some("--") || args.len() < 4 {
        return Err(command_error("invalid internal service launcher arguments"));
    }
    let mut stdin = io::stdin();
    let mut gate = [0_u8; 1];
    stdin.read_exact(&mut gate).map_err(command_error)?;
    if gate != *b"K" {
        return Err(command_error("invalid service launch gate"));
    }
    let target = Command::new(&args[3])
        .args(&args[4..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(command_error)?;
    let target = Arc::new(Mutex::new(target));
    let watched = Arc::clone(&target);
    // Only a single lifetime reader is added per service process, not per
    // request/Task. It never owns or runs application initialization.
    if let Err(error) = thread::Builder::new()
        .name("ku-service-owner".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut byte = [0_u8; 1];
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) | Err(_) => {
                        // Child owns the original Windows handle and caches a
                        // reaped Unix exit. Never reopen a potentially reused
                        // numeric process ID after target exit.
                        let _ = watched
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .kill();
                        break;
                    }
                    Ok(_) => {}
                }
            }
        })
    {
        let mut target = target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = target.kill();
        let _ = target.wait();
        return Err(command_error(format!("service lifetime reader: {error}")));
    }
    let exit = loop {
        if let Some(exit) = target
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .try_wait()
            .map_err(command_error)?
        {
            break exit;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A blocked lifetime reader must not keep a completed native process alive.
    // OS process teardown owns that thread and its stdin handle.
    std::process::exit(exit.code().unwrap_or(1));
}

pub(super) struct StopSignals {
    #[cfg(unix)]
    previous: [(libc::c_int, libc::sighandler_t); 2],
}

impl StopSignals {
    pub(super) fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            extern "C" fn stop(_: libc::c_int) {
                STOP_REQUESTED.store(true, Ordering::Release);
            }
            let previous = unsafe {
                [
                    (
                        libc::SIGINT,
                        libc::signal(libc::SIGINT, stop as libc::sighandler_t),
                    ),
                    (
                        libc::SIGTERM,
                        libc::signal(libc::SIGTERM, stop as libc::sighandler_t),
                    ),
                ]
            };
            if previous.iter().any(|(_, old)| *old == libc::SIG_ERR) {
                for (signal, old) in previous {
                    if old != libc::SIG_ERR {
                        unsafe {
                            libc::signal(signal, old);
                        }
                    }
                }
                return Err(io::Error::last_os_error());
            }
            Ok(Self { previous })
        }
        #[cfg(windows)]
        {
            win::install_stop_handler()?;
            Ok(Self {})
        }
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "resident service process management is unavailable on this host",
        ))
    }
}

impl Drop for StopSignals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (signal, old) in self.previous {
            unsafe {
                libc::signal(signal, old);
            }
        }
        #[cfg(windows)]
        win::uninstall_stop_handler();
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::{mem::size_of, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    unsafe extern "system" fn stop(event: u32) -> i32 {
        if matches!(event, 0 | 1 | 2 | 5 | 6) {
            STOP_REQUESTED.store(true, Ordering::Release);
            1
        } else {
            0
        }
    }

    pub(super) fn install_stop_handler() -> io::Result<()> {
        if unsafe { SetConsoleCtrlHandler(Some(stop), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn uninstall_stop_handler() {
        unsafe {
            SetConsoleCtrlHandler(Some(stop), 0);
        }
    }

    pub(super) struct Job(HANDLE);

    impl Job {
        pub(super) fn assign(child: &Child) -> io::Result<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn empty(&self) -> io::Result<bool> {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(info.ActiveProcesses == 0)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}
