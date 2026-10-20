//! Kernel-enforced resource domains for the process execution coordinator.
//!
//! The metrics intentionally name the provider's accounting unit. Linux memory
//! charges and Windows committed bytes are not interchangeable RSS measurements.

use std::{fmt, path::PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct GroupConfig {
    pub(crate) worker_memory_bytes: u64,
    pub(crate) total_worker_memory_bytes: u64,
    pub(crate) max_workers: usize,
    pub(crate) cgroup_root: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct Metrics {
    pub(crate) limit_metric: &'static str,
    pub(crate) memory_peak_bytes: Option<u64>,
    pub(crate) memory_current_bytes: Option<u64>,
    pub(crate) oom_kill_count: Option<u64>,
    pub(crate) memory_limit_event: Option<bool>,
    pub(crate) memory_limit_termination: Option<bool>,
    pub(crate) process_count: Option<u64>,
    pub(crate) resource_group: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderError {
    pub(crate) detail: String,
    pub(crate) cleanup_confirmed: bool,
}

impl ProviderError {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            cleanup_confirmed: true,
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn cleanup_unconfirmed(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            cleanup_confirmed: false,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ProviderError {}

impl From<std::io::Error> for ProviderError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

fn validate(config: &GroupConfig) -> Result<usize, ProviderError> {
    if config.worker_memory_bytes == 0
        || config.total_worker_memory_bytes < config.worker_memory_bytes
        || config.max_workers == 0
        || config.max_workers > 1_024
    {
        return Err(ProviderError::new(
            "process memory limits must be positive, the total must cover one worker, and max_workers must be in 1..=1024",
        ));
    }
    let memory_slots = config.total_worker_memory_bytes / config.worker_memory_bytes;
    Ok(config
        .max_workers
        .min(usize::try_from(memory_slots).unwrap_or(usize::MAX)))
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{Domain, Worker};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{Domain, Worker};

#[cfg(not(any(target_os = "linux", windows)))]
mod unsupported {
    use super::{GroupConfig, Metrics, ProviderError};
    use std::{process::Child, process::Command, time::Instant};

    pub(crate) struct Domain;

    pub(crate) struct Worker {
        pub(crate) child: Child,
    }

    impl Domain {
        pub(crate) fn prepare(config: &GroupConfig) -> Result<Self, ProviderError> {
            super::validate(config)?;
            if config.cgroup_root.is_some() {
                return Err(ProviderError::new(
                    "cgroup_root applies only to a Linux execution host; a local directory cannot enable strict process memory limits on this platform",
                ));
            }
            Err(unsupported())
        }

        pub(crate) fn spawn(
            &mut self,
            _command: &mut Command,
            _job_id: u64,
        ) -> Result<Worker, ProviderError> {
            Err(unsupported())
        }
    }

    impl Worker {
        pub(crate) fn terminate_group(&self) -> Result<(), ProviderError> {
            Err(unsupported())
        }

        pub(crate) fn snapshot(&self) -> Result<Metrics, ProviderError> {
            Err(unsupported())
        }

        pub(crate) fn cleanup(&mut self, _deadline: Instant) -> Result<Metrics, ProviderError> {
            Err(unsupported())
        }
    }

    fn unsupported() -> ProviderError {
        ProviderError::new(
            "strict process memory execution is unavailable on this platform; run Ku inside a Linux VM or host with a delegated cgroup v2 memory+pids domain, or on Windows with Job Object commit limits; macOS RLIMIT_AS and sampled RSS are not equivalent providers",
        )
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) use unsupported::{Domain, Worker};
