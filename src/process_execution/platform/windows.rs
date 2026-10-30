use super::{GroupConfig, Metrics, ProviderError};
use std::{
    io,
    mem::{size_of, zeroed},
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, Command},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        },
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::{
            GetProcessIdOfThread, OpenThread, ResumeThread, CREATE_SUSPENDED,
            THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
        },
    },
};

const WORKER_PROCESSES: usize = 64;
const MAX_THREAD_SCAN_ENTRIES: usize = 65_536;

pub(crate) struct Domain {
    inner: Arc<DomainState>,
}

struct DomainState {
    job: Job,
    worker_memory_bytes: usize,
    worker_slots: usize,
    active_workers: AtomicUsize,
}

pub(crate) struct Worker {
    pub(crate) child: Child,
    job: Job,
    group_name: String,
    domain: Arc<DomainState>,
    retired: Option<Metrics>,
}

struct Job(HANDLE);

// SAFETY: Job owns a kernel Job Object handle. Windows permits concurrent
// queries/assignments on this handle; Arc keeps it open until all leases retire.
unsafe impl Send for Job {}
// SAFETY: the same owned kernel handle can be queried from multiple threads;
// this type never exposes mutable references to caller-provided buffers.
unsafe impl Sync for Job {}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the value uniquely owns this successfully opened handle.
        unsafe { CloseHandle(self.0) };
    }
}

impl Domain {
    pub(crate) fn prepare(config: &GroupConfig) -> Result<Self, ProviderError> {
        let worker_slots = super::validate(config)?;
        if config.cgroup_root.is_some() {
            return Err(ProviderError::new("cgroup_root applies only to Linux"));
        }
        let worker_memory_bytes = usize::try_from(config.worker_memory_bytes)
            .map_err(|_| ProviderError::new("worker commit limit exceeds SIZE_T"))?;
        let total_memory_bytes = usize::try_from(config.total_worker_memory_bytes)
            .map_err(|_| ProviderError::new("total commit limit exceeds SIZE_T"))?;
        let total_processes = worker_slots
            .checked_mul(WORKER_PROCESSES)
            .ok_or_else(|| ProviderError::new("process limit overflow"))?;
        let job = Job::create(total_memory_bytes, None, total_processes)?;
        Ok(Self {
            inner: Arc::new(DomainState {
                job,
                worker_memory_bytes,
                worker_slots,
                active_workers: AtomicUsize::new(0),
            }),
        })
    }

    pub(crate) fn spawn(
        &mut self,
        command: &mut Command,
        job_id: u64,
    ) -> Result<Worker, ProviderError> {
        self.inner
            .active_workers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.inner.worker_slots).then_some(active + 1)
            })
            .map_err(|_| {
                ProviderError::new("process worker slots or total reserved memory exhausted")
            })?;
        let job = match Job::create(
            self.inner.worker_memory_bytes,
            Some(self.inner.worker_memory_bytes),
            WORKER_PROCESSES,
        ) {
            Ok(job) => job,
            Err(error) => {
                self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };
        // std retains its normal argument quoting, environment and pipe setup.
        // No application thread, including DLL initializers, runs until resume.
        command.creation_flags(CREATE_SUSPENDED);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
                return Err(error.into());
            }
        };
        let launch = (|| {
            let initial_commit = process_commit(&child)?;
            if initial_commit > self.inner.worker_memory_bytes {
                return Err(ProviderError::new(format!(
                    "suspended worker initial commit {initial_commit} exceeds worker limit {}",
                    self.inner.worker_memory_bytes
                )));
            }
            self.inner.job.assign(&child)?;
            job.assign(&child)?;
            // AssignProcessToJobObject does not inspect pre-assignment memory
            // operations. Check that initial OS-created commit fits explicitly.
            if process_commit(&child)? > self.inner.worker_memory_bytes {
                return Err(ProviderError::new(
                    "suspended worker initial commit grew beyond its admitted limit",
                ));
            }
            job.verify_limits(self.inner.worker_memory_bytes, true, WORKER_PROCESSES)?;
            resume_suspended_child(child.id())
        })();
        if let Err(error) = launch {
            let deadline = Instant::now() + Duration::from_secs(2);
            match retire_failed_launch(&mut child, &job, deadline) {
                Ok(()) => {
                    self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
                    return Err(error);
                }
                Err(cleanup_error) => {
                    return Err(ProviderError::cleanup_unconfirmed(format!(
                        "{error}; suspended worker cleanup unconfirmed: {cleanup_error}"
                    )));
                }
            }
        }
        Ok(Worker {
            child,
            job,
            group_name: format!("windows-job:{}:{job_id}", std::process::id()),
            domain: Arc::clone(&self.inner),
            retired: None,
        })
    }
}

impl Worker {
    pub(crate) fn terminate_group(&self) -> Result<(), ProviderError> {
        if self.retired.is_some() {
            return Ok(());
        }
        self.job.terminate()
    }

    pub(crate) fn snapshot(&self) -> Result<Metrics, ProviderError> {
        if let Some(metrics) = &self.retired {
            return Ok(metrics.clone());
        }
        let extended = self.job.extended()?;
        let accounting = self.job.accounting()?;
        Ok(Metrics {
            limit_metric: "windows_job_committed_memory_bytes",
            memory_peak_bytes: Some(extended.PeakJobMemoryUsed as u64),
            // Extended Job information exposes a kernel-maintained peak, not
            // current commit. Do not substitute RSS or summed per-process peaks.
            memory_current_bytes: None,
            // A denied commit may abort, return NULL, or be handled by the
            // third-party library. Exit codes do not establish an OOM cause.
            // We intentionally do not create an unbounded completion-port event
            // queue merely to label denied allocations. None means unobserved.
            oom_kill_count: None,
            memory_limit_event: None,
            memory_limit_termination: None,
            memory_limit_evidence: None,
            process_count: Some(u64::from(accounting.ActiveProcesses)),
            resource_group: self.group_name.clone(),
        })
    }

    pub(crate) fn cleanup(&mut self, deadline: Instant) -> Result<Metrics, ProviderError> {
        if let Some(metrics) = &self.retired {
            return Ok(metrics.clone());
        }
        self.terminate_group().map_err(|error| {
            ProviderError::cleanup_unconfirmed(format!("could not terminate worker Job: {error}"))
        })?;
        loop {
            let reaped = self
                .child
                .try_wait()
                .map_err(|error| {
                    ProviderError::cleanup_unconfirmed(format!("could not reap worker: {error}"))
                })?
                .is_some();
            let active = self
                .job
                .accounting()
                .map_err(|error| {
                    ProviderError::cleanup_unconfirmed(format!(
                        "could not verify Job retirement: {error}"
                    ))
                })?
                .ActiveProcesses;
            if reaped && active == 0 {
                let metrics = self.snapshot().map_err(|error| {
                    ProviderError::cleanup_unconfirmed(format!(
                        "could not finalize worker metrics: {error}"
                    ))
                })?;
                self.retired = Some(metrics.clone());
                self.domain.active_workers.fetch_sub(1, Ordering::AcqRel);
                return Ok(metrics);
            }
            if Instant::now() >= deadline {
                return Err(ProviderError::cleanup_unconfirmed(
                    "worker Job still has active processes or an unreaped child; budget retained",
                ));
            }
            thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.retired.is_none() {
            if let Err(error) = self.cleanup(Instant::now() + Duration::from_secs(2)) {
                eprintln!("Ku process worker cleanup unconfirmed: {error}");
            }
        }
    }
}

impl Job {
    fn create(
        job_memory_bytes: usize,
        process_memory_bytes: Option<usize>,
        process_limit: usize,
    ) -> Result<Self, ProviderError> {
        // SAFETY: null pointers request an unnamed, non-inheritable Job Object.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let job = Self(handle);
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        information.BasicLimitInformation.ActiveProcessLimit = u32::try_from(process_limit)
            .map_err(|_| ProviderError::new("Windows process count limit overflow"))?;
        information.JobMemoryLimit = job_memory_bytes;
        if let Some(process_memory_bytes) = process_memory_bytes {
            information.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            information.ProcessMemoryLimit = process_memory_bytes;
        }
        // SAFETY: information's type and exact size match the information class.
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        job.verify_limits(
            job_memory_bytes,
            process_memory_bytes.is_some(),
            process_limit,
        )?;
        Ok(job)
    }

    fn assign(&self, child: &Child) -> Result<(), ProviderError> {
        // SAFETY: both handles are live and Child owns its process handle.
        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn extended(&self) -> Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION, ProviderError> {
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        // SAFETY: the live Job handle and output exactly match the query class.
        if unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectExtendedLimitInformation,
                (&mut information as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(information)
    }

    fn accounting(&self) -> Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, ProviderError> {
        let mut information: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        // SAFETY: the live Job handle and output exactly match the query class.
        if unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(information)
    }

    fn verify_limits(
        &self,
        memory_bytes: usize,
        per_process: bool,
        process_limit: usize,
    ) -> Result<(), ProviderError> {
        let information = self.extended()?;
        let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | if per_process {
                JOB_OBJECT_LIMIT_PROCESS_MEMORY
            } else {
                0
            };
        if information.BasicLimitInformation.LimitFlags & required != required
            || information.JobMemoryLimit != memory_bytes
            || (per_process && information.ProcessMemoryLimit != memory_bytes)
            || information.BasicLimitInformation.ActiveProcessLimit as usize != process_limit
        {
            return Err(ProviderError::new(
                "Windows did not retain the requested Job commit limits",
            ));
        }
        Ok(())
    }

    fn terminate(&self) -> Result<(), ProviderError> {
        if self.accounting()?.ActiveProcesses == 0 {
            return Ok(());
        }
        // SAFETY: the owned Job includes this worker and all normal descendants.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: this handle is unique; KILL_ON_JOB_CLOSE also covers unwind.
        unsafe { CloseHandle(self.0) };
    }
}

fn process_commit(child: &Child) -> Result<usize, ProviderError> {
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: PROCESS_MEMORY_COUNTERS_EX begins with the documented base layout,
    // and cb supplies the full buffer length. Child owns a valid query handle.
    if unsafe {
        GetProcessMemoryInfo(
            child.as_raw_handle() as HANDLE,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(counters.PrivateUsage)
}

fn retire_failed_launch(
    child: &mut Child,
    job: &Job,
    deadline: Instant,
) -> Result<(), ProviderError> {
    // Assignment may have failed before the child joined the leaf Job. Kill
    // both the Job and direct process; its primary thread has not run yet.
    let group_error = job.terminate().err();
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    loop {
        if child.try_wait()?.is_some() && job.accounting()?.ActiveProcesses == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(group_error.unwrap_or_else(|| {
                ProviderError::new("failed launch did not retire before cleanup deadline")
            }));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn resume_suspended_child(process_id: u32) -> Result<(), ProviderError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: this creates one owned snapshot handle, with no output pointers.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: entry is correctly sized and the snapshot handle is live.
    if unsafe { Thread32First(snapshot.0, &mut entry) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut thread_id = None;
    let mut scanned = 0;
    loop {
        scanned += 1;
        if scanned > MAX_THREAD_SCAN_ENTRIES || Instant::now() >= deadline {
            return Err(ProviderError::new(
                "suspended worker thread enumeration exceeded its bound",
            ));
        }
        if entry.th32OwnerProcessID == process_id && thread_id.replace(entry.th32ThreadID).is_some()
        {
            return Err(ProviderError::new(
                "suspended worker unexpectedly has multiple threads",
            ));
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        // SAFETY: the snapshot and entry remain valid for this bounded scan.
        if unsafe { Thread32Next(snapshot.0, &mut entry) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error.into());
        }
    }
    if Instant::now() >= deadline {
        return Err(ProviderError::new(
            "worker thread enumeration exceeded its deadline",
        ));
    }
    let thread_id =
        thread_id.ok_or_else(|| ProviderError::new("suspended worker thread not found"))?;
    // SAFETY: the returned handle has only the query and resume rights used here.
    let handle = unsafe {
        OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
            0,
            thread_id,
        )
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    let handle = OwnedHandle(handle);
    // SAFETY: the thread handle has query permission; owner is rechecked after
    // the snapshot lookup to reject a recycled thread id.
    if unsafe { GetProcessIdOfThread(handle.0) } != process_id || Instant::now() >= deadline {
        return Err(ProviderError::new(
            "suspended worker thread owner changed or lookup expired",
        ));
    }
    // SAFETY: CREATE_SUSPENDED establishes an initial count of exactly one.
    let previous = unsafe { ResumeThread(handle.0) };
    if previous != 1 || Instant::now() >= deadline {
        return Err(ProviderError::new(
            "worker resume failed or exceeded its deadline",
        ));
    }
    Ok(())
}
