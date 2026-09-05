use std::process::Stdio;

/// Supply a valid standard descriptor whose writes fail. Closing fd 1 itself
/// is not sufficient: Rust can reopen missing standard fds or ignore EBADF.
pub fn disconnected_stdout() -> Stdio {
    #[cfg(unix)]
    {
        use std::{net::Shutdown, os::fd::OwnedFd, os::unix::net::UnixStream};

        let (reader, writer) = UnixStream::pair().expect("create stdout fault socket pair");
        // Shutdown is shared by every duplicate, including a descriptor that
        // another test's concurrent fork briefly inherited before its exec.
        // Dropping a pipe reader alone would leave that inheritance window.
        reader
            .shutdown(Shutdown::Both)
            .expect("disconnect stdout peer");
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
#[test]
fn disconnected_stdout_peer_clone_cannot_keep_writes_alive() {
    use std::{io::Write, net::Shutdown, os::unix::net::UnixStream};

    let (reader, mut writer) = UnixStream::pair().unwrap();
    let inherited_reader = reader.try_clone().unwrap();
    reader.shutdown(Shutdown::Both).unwrap();
    drop(reader);
    let error = writer
        .write_all(b"output probe")
        .expect_err("peer is disconnected");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    drop(inherited_reader);
}
