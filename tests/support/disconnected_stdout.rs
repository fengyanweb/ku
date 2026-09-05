use std::process::Stdio;

/// Supply a valid standard descriptor whose writes fail. Closing fd 1 itself
/// is not sufficient: Rust can reopen missing standard fds or ignore EBADF.
pub fn disconnected_stdout() -> Stdio {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;

        let (reader, writer) = stdout_fault_socket_pair();
        drop(reader);
        Stdio::from(OwnedFd::from(writer))
    }
    #[cfg(not(unix))]
    {
        let (reader, writer) = std::io::pipe().expect("create stdout fault pipe");
        drop(reader);
        Stdio::from(writer)
    }
}

#[cfg(unix)]
fn stdout_fault_socket_pair() -> (
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    use std::{net::Shutdown, os::unix::net::UnixStream};

    let (reader, writer) = UnixStream::pair().expect("create stdout fault socket pair");
    // Disable this endpoint's writes, not just its peer's reads: on macOS a
    // peer with a surviving duplicate can still accept writes after SHUT_RD.
    // SHUT_WR also applies to copies inherited by concurrent fork/exec calls.
    writer
        .shutdown(Shutdown::Write)
        .expect("disable stdout socket writes");
    (reader, writer)
}

#[cfg(unix)]
#[test]
fn disconnected_stdout_peer_clone_cannot_keep_writes_alive() {
    use std::io::Write;

    let (reader, mut writer) = stdout_fault_socket_pair();
    let inherited_reader = reader.try_clone().unwrap();
    let mut inherited_writer = writer.try_clone().unwrap();
    drop(reader);
    let error = writer
        .write_all(b"output probe")
        .expect_err("writer is disabled even while a peer duplicate survives");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    let error = inherited_writer
        .write_all(b"duplicate output probe")
        .expect_err("duplicate writer is also disabled");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    drop(inherited_reader);
}
