use super::{GroupConfig, Metrics, ProviderError};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CONTROL_FILE_BYTES: u64 = 64 * 1024;
const WORKER_KERNEL_TASKS: usize = 256;
const PIPE_RESERVATION_BYTES: libc::c_int = 1024 * 1024;
const CGROUP2_MAGIC: i128 = 0x6367_7270;
static DOMAIN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Domain {
    inner: Arc<DomainState>,
}

struct DomainState {
    path: PathBuf,
    worker_memory_bytes: u64,
    worker_slots: usize,
    active_workers: AtomicUsize,
}

pub(crate) struct Worker {
    pub(crate) child: Child,
    group: PathBuf,
    domain: Arc<DomainState>,
    retired: Option<Metrics>,
}

impl Domain {
    pub(crate) fn prepare(config: &GroupConfig) -> Result<Self, ProviderError> {
        let worker_slots = super::validate(config)?;
        let root = config.cgroup_root.as_deref().ok_or_else(|| {
            ProviderError::new(
                "Linux process execution requires --cgroup-root pointing to a delegated cgroup v2 directory with memory and pids already enabled; no weaker memory provider is substituted",
            )
        })?;
        if !root.is_absolute() {
            return Err(ProviderError::new("cgroup_root must be an absolute path"));
        }
        let root = fs::canonicalize(root)?;
        let root_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&root)?;
        // SAFETY: statfs points to a correctly sized output and the fd is owned.
        let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(root_file.as_raw_fd(), &mut filesystem) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if i128::from(filesystem.f_type) != CGROUP2_MAGIC {
            return Err(ProviderError::new(
                "cgroup_root is not a cgroup v2 filesystem",
            ));
        }
        let enabled = read_control(&root.join("cgroup.subtree_control"))?;
        if !enabled
            .split_ascii_whitespace()
            .any(|value| value == "memory")
            || !enabled
                .split_ascii_whitespace()
                .any(|value| value == "pids")
        {
            return Err(ProviderError::new(
                "the delegated cgroup root must already enable memory and pids for its children; Ku does not change unrelated parent controllers",
            ));
        }
        // SAFETY: sysconf is a read-only query with no pointer arguments.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size = u64::try_from(page_size)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| ProviderError::new("could not determine the kernel page size"))?;
        if config.worker_memory_bytes % page_size != 0
            || config.total_worker_memory_bytes % page_size != 0
        {
            return Err(ProviderError::new(format!(
                "Linux memory byte limits must be multiples of the kernel page size ({page_size})"
            )));
        }
        let path = create_private_group(&root)?;
        let setup = (|| {
            // Requiring this writable interface before any launch avoids a
            // provider which can apply limits but cannot retire descendants.
            write_control(&path.join("cgroup.kill"), 1)?;
            write_control(&path.join("memory.max"), config.total_worker_memory_bytes)?;
            write_control(&path.join("memory.swap.max"), 0)?;
            write_control(&path.join("memory.oom.group"), 1)?;
            let total_tasks = worker_slots
                .checked_mul(WORKER_KERNEL_TASKS)
                .ok_or_else(|| ProviderError::new("worker kernel task limit overflow"))?;
            write_control(&path.join("pids.max"), total_tasks)?;
            write_text(&path.join("cgroup.subtree_control"), "+memory +pids")?;
            require_limit(&path.join("memory.max"), config.total_worker_memory_bytes)?;
            require_limit(&path.join("memory.swap.max"), 0)?;
            require_limit(&path.join("memory.oom.group"), 1)?;
            require_limit(&path.join("pids.max"), total_tasks as u64)?;
            Ok::<(), ProviderError>(())
        })();
        if let Err(error) = setup {
            return match fs::remove_dir(&path) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(ProviderError::cleanup_unconfirmed(format!(
                    "{error}; partially configured cgroup retained at {}: {cleanup}",
                    path.display()
                ))),
            };
        }
        Ok(Self {
            inner: Arc::new(DomainState {
                path,
                worker_memory_bytes: config.worker_memory_bytes,
                worker_slots,
                active_workers: AtomicUsize::new(0),
            }),
        })
    }

    /// `command` must be newly constructed for this launch. In particular,
    /// Command's pre_exec hooks cannot be removed and must not be reused.
    pub(crate) fn spawn(
        &mut self,
        command: &mut Command,
        job_id: u64,
    ) -> Result<Worker, ProviderError> {
        reserve_worker(&self.inner)?;
        let group = self.inner.path.join(format!("worker-{job_id}"));
        if let Err(error) = fs::create_dir(&group) {
            self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
            return Err(error.into());
        }
        let launch = (|| {
            write_control(&group.join("cgroup.kill"), 1)?;
            write_control(&group.join("memory.max"), self.inner.worker_memory_bytes)?;
            write_control(&group.join("memory.swap.max"), 0)?;
            write_control(&group.join("memory.oom.group"), 1)?;
            write_control(&group.join("pids.max"), WORKER_KERNEL_TASKS)?;
            require_limit(&group.join("memory.max"), self.inner.worker_memory_bytes)?;
            require_limit(&group.join("memory.swap.max"), 0)?;
            require_limit(&group.join("memory.oom.group"), 1)?;
            require_limit(&group.join("pids.max"), WORKER_KERNEL_TASKS as u64)?;
            // Prepare the fd before fork: the hook performs only fixed syscalls,
            // no allocation, path lookup, formatting, or rendezvous protocol.
            let admission = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(group.join("cgroup.procs"))?;
            // SAFETY: getpid has no pointer arguments and cannot fail.
            let coordinator_pid = unsafe { libc::getpid() };
            command.process_group(0);
            // SAFETY: every operation in this post-fork hook is async-signal-safe
            // and errors use only an OS error code. The fd stays owned by the
            // Command closure and is marked close-on-exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() != coordinator_pid {
                        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                    }
                    // Writing zero moves the calling child, before exec and
                    // before any application allocator or third-party library.
                    let written = libc::write(admission.as_raw_fd(), b"0\n".as_ptr().cast(), 2);
                    if written < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if written != 2 {
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                        let current = libc::fcntl(fd, libc::F_GETPIPE_SZ);
                        if current > 0 {
                            let capacity = libc::fcntl(fd, libc::F_SETPIPE_SZ, 4_096);
                            if capacity < 0 {
                                return Err(io::Error::last_os_error());
                            }
                            if capacity > PIPE_RESERVATION_BYTES {
                                return Err(io::Error::from_raw_os_error(libc::E2BIG));
                            }
                        }
                    }
                    Ok(())
                });
            }
            command.spawn().map_err(ProviderError::from)
        })();
        match launch {
            Ok(child) => Ok(Worker {
                child,
                group,
                domain: Arc::clone(&self.inner),
                retired: None,
            }),
            Err(error) => {
                // std::process reaps a child whose pre_exec/exec fails. An
                // unexpected populated group is retained and keeps its slot.
                if group_empty(&group).unwrap_or(false) && fs::remove_dir(&group).is_ok() {
                    self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
                    Err(error)
                } else {
                    let _ = kill_group(&group);
                    Err(ProviderError::cleanup_unconfirmed(format!(
                        "{error}; failed launch cgroup could not be retired: {}",
                        group.display()
                    )))
                }
            }
        }
    }
}

impl Worker {
    pub(crate) fn terminate_group(&self) -> Result<(), ProviderError> {
        if self.retired.is_some() {
            return Ok(());
        }
        kill_group(&self.group)
    }

    pub(crate) fn snapshot(&self) -> Result<Metrics, ProviderError> {
        if let Some(metrics) = &self.retired {
            return Ok(metrics.clone());
        }
        snapshot(&self.group)
    }

    pub(crate) fn cleanup(&mut self, deadline: Instant) -> Result<Metrics, ProviderError> {
        if let Some(metrics) = &self.retired {
            return Ok(metrics.clone());
        }
        self.terminate_group().map_err(|error| {
            ProviderError::cleanup_unconfirmed(format!(
                "could not terminate worker cgroup: {error}"
            ))
        })?;
        loop {
            let reaped = self
                .child
                .try_wait()
                .map_err(|error| {
                    ProviderError::cleanup_unconfirmed(format!("could not reap worker: {error}"))
                })?
                .is_some();
            let empty = group_empty(&self.group).map_err(|error| {
                ProviderError::cleanup_unconfirmed(format!(
                    "could not verify cgroup retirement: {error}"
                ))
            })?;
            if reaped && empty {
                let metrics = self.snapshot().map_err(|error| {
                    ProviderError::cleanup_unconfirmed(format!(
                        "could not finalize worker metrics: {error}"
                    ))
                })?;
                fs::remove_dir(&self.group).map_err(|error| {
                    ProviderError::cleanup_unconfirmed(format!(
                        "worker exited but its cgroup could not be retired: {error}"
                    ))
                })?;
                self.retired = Some(metrics.clone());
                self.domain.active_workers.fetch_sub(1, Ordering::AcqRel);
                return Ok(metrics);
            }
            if Instant::now() >= deadline {
                return Err(ProviderError::cleanup_unconfirmed(format!(
                    "worker descendants did not retire before cleanup deadline; budget retained for {}",
                    self.group.display()
                )));
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
            let deadline = Instant::now() + Duration::from_secs(2);
            if let Err(error) = self.cleanup(deadline) {
                eprintln!("Ku process worker cleanup unconfirmed: {error}");
            }
        }
    }
}

impl Drop for DomainState {
    fn drop(&mut self) {
        // This runs only after the last worker lease is dropped. A failed
        // cleanup leaves the directory as evidence and never makes it reusable.
        let _ = kill_group(&self.path);
        if let Err(error) = fs::remove_dir(&self.path) {
            eprintln!(
                "Ku process domain retained at {}: {error}",
                self.path.display()
            );
        }
    }
}

fn reserve_worker(domain: &DomainState) -> Result<(), ProviderError> {
    domain
        .active_workers
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < domain.worker_slots).then_some(active + 1)
        })
        .map(|_| ())
        .map_err(|_| ProviderError::new("process worker slots or total reserved memory exhausted"))
}

fn create_private_group(root: &Path) -> Result<PathBuf, ProviderError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..16 {
        let sequence = DOMAIN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            "ku-execution-{}-{stamp:x}-{sequence:x}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(ProviderError::new(
        "could not allocate a unique cgroup domain",
    ))
}

fn read_control(path: &Path) -> Result<String, ProviderError> {
    let mut value = String::new();
    File::open(path)?
        .take(CONTROL_FILE_BYTES + 1)
        .read_to_string(&mut value)?;
    if value.len() as u64 > CONTROL_FILE_BYTES {
        return Err(ProviderError::new(
            "cgroup control file exceeds its read bound",
        ));
    }
    Ok(value)
}

fn read_number(path: &Path) -> Result<u64, ProviderError> {
    read_control(path)?
        .trim()
        .parse()
        .map_err(|_| ProviderError::new(format!("invalid cgroup counter at {}", path.display())))
}

fn write_control(path: &Path, value: impl fmt::Display) -> Result<(), ProviderError> {
    write_text(path, &value.to_string())
}

use std::fmt;

fn write_text(path: &Path, value: &str) -> Result<(), ProviderError> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?
        .write_all(value.as_bytes())?;
    Ok(())
}

fn require_limit(path: &Path, expected: u64) -> Result<(), ProviderError> {
    let actual = read_number(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ProviderError::new(format!(
            "kernel did not accept cgroup limit at {}: requested {expected}, read {actual}",
            path.display()
        )))
    }
}

fn group_empty(path: &Path) -> Result<bool, ProviderError> {
    let events = read_control(&path.join("cgroup.events"))?;
    for line in events.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some("populated") {
            return match fields.next() {
                Some("0") => Ok(true),
                Some("1") => Ok(false),
                _ => Err(ProviderError::new("invalid cgroup populated counter")),
            };
        }
    }
    Err(ProviderError::new("cgroup populated counter was absent"))
}

fn kill_group(path: &Path) -> Result<(), ProviderError> {
    if group_empty(path)? {
        return Ok(());
    }
    write_control(&path.join("cgroup.kill"), 1)
}

fn snapshot(path: &Path) -> Result<Metrics, ProviderError> {
    let memory_current_bytes = Some(read_number(&path.join("memory.current"))?);
    let memory_peak_bytes = match fs::metadata(path.join("memory.peak")) {
        Ok(_) => Some(read_number(&path.join("memory.peak"))?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let events = read_control(&path.join("memory.events"))?;
    let mut oom_kill_count = None;
    let mut memory_limit_event = false;
    let mut local_oom = false;
    for line in events.lines() {
        let mut fields = line.split_ascii_whitespace();
        let key = fields.next().unwrap_or_default();
        let Some(raw_value) = fields.next() else {
            return Err(ProviderError::new("invalid cgroup memory event counter"));
        };
        let value = raw_value
            .parse::<u64>()
            .map_err(|_| ProviderError::new("invalid cgroup memory event counter"))?;
        if key == "oom_kill" {
            oom_kill_count = Some(value);
        }
        if key == "oom" && value != 0 {
            local_oom = true;
        }
        // oom_kill also includes a global host OOM victim. Preserve that count
        // without claiming it proves this leaf's configured limit was reached.
        if matches!(key, "max" | "oom") && value != 0 {
            memory_limit_event = true;
        }
    }
    // This is the leaf's process count, unlike pids.current which also counts
    // individual kernel threads. populated additionally covers descendants.
    let process_count = Some(
        read_control(&path.join("cgroup.procs"))?
            .lines()
            .filter(|line| !line.is_empty())
            .count() as u64,
    );
    Ok(Metrics {
        limit_metric: "linux_cgroup_v2_charged_memory_bytes",
        memory_peak_bytes,
        memory_current_bytes,
        oom_kill_count,
        memory_limit_event: Some(memory_limit_event),
        memory_limit_termination: Some(local_oom && oom_kill_count.is_some_and(|count| count != 0)),
        process_count,
        resource_group: path.display().to_string(),
    })
}
