//! One startup FD-table preparation, before Tokio creates any runtime threads.
//! It reserves table capacity, not open connections or all future process FDs.
use std::{io, time::Instant};

const RUNTIME_RESERVE_FDS: u64 = 64;

#[derive(Debug, serde::Serialize)]
pub struct Preparation {
    supported: bool,
    method: &'static str,
    configured_max_connections: u64,
    runtime_reserve_fds: u64,
    max_open_fd_before: Option<u64>,
    target_fd: Option<u64>,
    soft_limit: Option<u64>,
    limit_clamped: bool,
    duplicate_fd: Option<i32>,
    duplicate_closed: bool,
    source_closed: bool,
    close_method: &'static str,
    elapsed_ns: u64,
}

fn parse_max_connections(value: Option<&str>) -> io::Result<u64> {
    match value {
        None => Ok(4096),
        Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
            value.parse::<u64>().ok().filter(|&n| n > 0).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "BENCH_MAX_CONNECTIONS must be positive u64 digits",
                )
            })
        }
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "BENCH_MAX_CONNECTIONS must be positive u64 digits",
        )),
    }
}

pub fn max_connections_from_env() -> io::Result<u64> {
    match std::env::var("BENCH_MAX_CONNECTIONS") {
        Ok(value) => parse_max_connections(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_max_connections(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "BENCH_MAX_CONNECTIONS must be valid UTF-8 digits",
        )),
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
struct Target {
    fd: u64,
    clamped: bool,
}

#[cfg(any(target_os = "linux", test))]
fn target(max_open_fd: u64, connections: u64, soft_limit: u64) -> io::Result<Target> {
    if connections == 0 || soft_limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FD table preparation requires positive connection and soft limits",
        ));
    }
    // The fcntl minimum is a signed C int even on 64-bit Linux. This also
    // handles RLIM_INFINITY and a soft limit lowered below an existing FD.
    let ceiling = (soft_limit - 1).min(i32::MAX as u64);
    let requested = max_open_fd
        .saturating_add(connections)
        .saturating_add(RUNTIME_RESERVE_FDS);
    Ok(Target {
        fd: requested.min(ceiling),
        clamped: requested > ceiling,
    })
}

#[cfg(target_os = "linux")]
pub fn prepare(connections: u64) -> io::Result<Preparation> {
    use std::{
        fs::File,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };
    let started = Instant::now();
    let context = |operation: &str, error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("FD table startup preparation {operation}: {error}"),
        )
    };
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limit is a writable, correctly sized libc rlimit structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(context("getrlimit", io::Error::last_os_error()));
    }
    let soft_limit = limit.rlim_cur as u64;
    // Validate before acquiring even the temporary source descriptor.
    target(0, connections, soft_limit)?;
    let source = File::open("/dev/null").map_err(|e| context("open source", e))?;
    let mut highest = source.as_raw_fd() as u64;
    let entries =
        std::fs::read_dir("/proc/self/fd").map_err(|e| context("open current descriptors", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| context("read current descriptors", e))?;
        let name = entry.file_name();
        let fd = name
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid /proc/self/fd descriptor name",
                )
            })?;
        highest = highest.max(fd);
    }
    // ReadDir has closed here. Including its transient FD only adds headroom.
    let target = target(highest, connections, soft_limit)?;
    // SAFETY: source owns a live FD; the checked target is representable as C int.
    // F_DUPFD_CLOEXEC chooses an unused descriptor and never overwrites one.
    let duplicate_fd = unsafe {
        libc::fcntl(
            source.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            target.fd as libc::c_int,
        )
    };
    if duplicate_fd < 0 {
        return Err(context("F_DUPFD_CLOEXEC", io::Error::last_os_error()));
    }
    // SAFETY: this call returned a new descriptor whose sole owner is this scope.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate_fd) };
    // RAII drops each acquired descriptor once, including every earlier error
    // path. Linux close must not be retried after EINTR; no socket is opened.
    drop(duplicate);
    drop(source);
    Ok(Preparation {
        supported: true,
        method: "linux_F_DUPFD_CLOEXEC_once",
        configured_max_connections: connections,
        runtime_reserve_fds: RUNTIME_RESERVE_FDS,
        max_open_fd_before: Some(highest),
        target_fd: Some(target.fd),
        soft_limit: Some(soft_limit),
        limit_clamped: target.clamped,
        duplicate_fd: Some(duplicate_fd),
        duplicate_closed: true,
        source_closed: true,
        close_method: "OwnedFd/File RAII dropped once; no close retry",
        elapsed_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn prepare(connections: u64) -> io::Result<Preparation> {
    let started = Instant::now();
    Ok(Preparation {
        supported: false,
        method: "unsupported_non_linux_noop",
        configured_max_connections: connections,
        runtime_reserve_fds: RUNTIME_RESERVE_FDS,
        max_open_fd_before: None,
        target_fd: None,
        soft_limit: None,
        limit_clamped: false,
        duplicate_fd: None,
        duplicate_closed: false,
        source_closed: false,
        close_method: "no descriptors acquired",
        elapsed_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_hint_defaults_and_rejects_invalid_digits() {
        assert_eq!(parse_max_connections(None).unwrap(), 4096);
        assert_eq!(parse_max_connections(Some("1")).unwrap(), 1);
        assert_eq!(
            parse_max_connections(Some("18446744073709551615")).unwrap(),
            u64::MAX
        );
        for input in [
            "",
            "0",
            "-1",
            "+1",
            " 4096",
            "4096 ",
            "1.5",
            "18446744073709551616",
        ] {
            assert_eq!(
                parse_max_connections(Some(input)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn target_uses_connection_hint_reserve_soft_limit_and_c_int_ceiling() {
        for (highest, connections, limit, fd, clamped) in [
            (7, 4096, 1_048_575, 4167, false),
            (9, 4096, 64, 63, true),
            (100, 1, 32, 31, true),
            (7, 1, 1, 0, true),
            (u64::MAX - 2, 9, 64, 63, true),
            (7, u64::MAX, u64::MAX, i32::MAX as u64, true),
            (i32::MAX as u64 - 1, 4096, u64::MAX, i32::MAX as u64, true),
        ] {
            assert_eq!(
                target(highest, connections, limit).unwrap(),
                Target { fd, clamped }
            );
        }
        assert!(target(0, 0, 1024).is_err());
        assert!(target(0, 4096, 0).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_preparation_expands_table_and_releases_descriptors_in_child() {
        const CHILD: &str = "KU_RUST_FD_TABLE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::process::{Command, Stdio};
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "fd_table::tests::linux_preparation_expands_table_and_releases_descriptors_in_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!("FD table child exceeded10s: {output:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "FD table child failed: {output:?}");
            return;
        }
        let fd_size = || {
            std::fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("FDSize:")
                        .map(|n| n.trim().parse::<u64>().unwrap())
                })
                .unwrap()
        };
        let before_size = fd_size();
        let before_count = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let preparation = prepare(before_size).unwrap();
        assert!(
            preparation.soft_limit.unwrap() > before_size,
            "test requires room to observe a real expansion"
        );
        assert!(preparation.duplicate_closed && preparation.source_closed);
        let duplicate = preparation.duplicate_fd.unwrap();
        // No file is opened between dropping the owned duplicate and this check.
        assert_eq!(unsafe { libc::fcntl(duplicate, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        assert_eq!(
            std::fs::read_dir("/proc/self/fd").unwrap().count(),
            before_count
        );
        assert!(
            fd_size() > before_size,
            "successful dup/close must retain enlarged table capacity"
        );
    }
}
