use super::*;

struct GateRelease(Option<mpsc::SyncSender<()>>);

impl GateRelease {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.try_send(());
        }
    }
}

impl Drop for GateRelease {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug)]
struct ClientReceipt {
    handshake_complete: bool,
    end: Result<bool, io::ErrorKind>,
}

struct ClientLifetime {
    release: GateRelease,
    thread: Option<thread::JoinHandle<io::Result<ClientReceipt>>>,
}

impl Drop for ClientLifetime {
    fn drop(&mut self) {
        self.release.release();
        if let Some(thread) = self.thread.take() {
            // The real client has bounded socket I/O and a bounded gate. Never
            // leave its socket/thread behind when a later assertion unwinds.
            let _ = thread.join();
        }
    }
}

fn wait_for_close_evidence(state: &RegistryState, deadline: Instant) -> TestConnectionEvidence {
    let mut recorded = state
        .connection_evidence
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    while !recorded.entries[..recorded.len]
        .iter()
        .flatten()
        .any(|evidence| evidence.phase == ConnectionPhase::CloseFlush)
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let (next, _) = state
            .connection_evidence_ready
            .wait_timeout(recorded, remaining)
            .unwrap_or_else(|error| error.into_inner());
        recorded = next;
    }
    *recorded
}

#[test]
fn authenticated_partial_put_records_expired_dispatch_response_and_close_at_one_deadline() {
    let root = TestRoot::new("connection-deadline-evidence");
    let request_timeout = Duration::from_secs(1);
    let files = test_server_files(&root.0, &[("token-math", "math")], request_timeout);
    let data_dir = files.config.data_dir.clone();
    let certificate = files.certificate.clone();
    let mut server = TestServer::start(files.config, files.certificate);
    let package = write_test_package(
        &root.0.join("math"),
        "math",
        "1.0.0",
        &server.base_url,
        &server.public_key,
        "fn Value(): int { return 1 }\n",
    );
    let artifact = pack_package(&package).expect("pack real diagnostic upload");
    let body = fs::read(&artifact.path).expect("read real diagnostic upload");
    assert!(body.len() > 1, "fixture must withhold actual archive bytes");
    let partial = test_publish_request("token-math", &artifact, &artifact.checksum, &body[..1]);
    let state = server.state.upgrade().expect("live diagnostic server");
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (body_release_tx, body_release_rx) = mpsc::sync_channel(1);
    let mut body_release = GateRelease(Some(body_release_tx));
    *state.publish_body_gate.lock().unwrap() = Some(TestPublishBodyGate {
        entered: entered_tx,
        release: body_release_rx,
    });
    let (client_release_tx, client_release_rx) = mpsc::sync_channel(1);
    let base_url = server.base_url.clone();
    let client_certificate = certificate.clone();
    let mut client = ClientLifetime {
        release: GateRelease(Some(client_release_tx)),
        thread: Some(thread::spawn(move || {
            let mut reader = connect_registry_load_tls(
                &base_url,
                client_certificate,
                Instant::now() + TEST_HTTP_RESPONSE_TIMEOUT,
            )?;
            {
                let stream = reader.get_mut();
                while stream.conn.is_handshaking() {
                    stream.conn.complete_io(&mut stream.sock)?;
                }
            }
            let handshake_complete = !reader.get_mut().conn.is_handshaking();
            reader.get_mut().write_all(&partial)?;
            reader.get_mut().flush()?;
            // Keep this authenticated socket open with the rest of the legal
            // body unsent. Server evidence, rather than a client EOF, ends it.
            client_release_rx
                .recv_timeout(TEST_HTTP_RESPONSE_TIMEOUT)
                .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))?;
            let end = reader
                .fill_buf()
                .map(|bytes| bytes.is_empty())
                .map_err(|error| error.kind());
            Ok(ClientReceipt {
                handshake_complete,
                end,
            })
        })),
    };
    let entered = entered_rx.recv_timeout(TEST_HTTP_RESPONSE_TIMEOUT);
    let evidence = wait_for_close_evidence(&state, Instant::now() + TEST_HTTP_RESPONSE_TIMEOUT);
    body_release.release();
    client.release.release();
    let client_result = client.thread.take().map(|thread| thread.join());
    let staging_empty =
        fs::read_dir(data_dir.join(STAGING_DIR)).map(|mut entries| entries.next().is_none());
    let uncommitted = !data_dir.join(PACKAGES_DIR).join("math").exists();
    let admission_released = state
        .mutation_admission
        .active_packages
        .lock()
        .unwrap()
        .is_empty();

    // A normal real response on the same server remains the existing protocol:
    // complete HTTP response followed by an authenticated TLS close notification.
    let control = (|| -> io::Result<(u16, Option<String>, bool)> {
        let mut reader = connect_registry_load_tls(
            &server.base_url,
            certificate,
            Instant::now() + TEST_HTTP_RESPONSE_TIMEOUT,
        )?;
        write_test_get(
            reader.get_mut(),
            "/v1/packages/missing/index.toml",
            Some("close"),
        );
        let (status, headers, _) = read_test_http_response(&mut reader)?;
        let closed = reader.fill_buf()?.is_empty();
        Ok((status, headers.get("connection").cloned(), closed))
    })();
    let after_control = *state.connection_evidence.lock().unwrap();
    drop(state);
    server.stop();
    let resources_released = server.state.upgrade().is_none();
    eprintln!(
        "registry_deadline_evidence entered={entered:?} events={:?} overflow={} client={client_result:?} staging={staging_empty:?} uncommitted={uncommitted} admission_released={admission_released} control={control:?} resources_released={resources_released}",
        evidence.entries, evidence.overflow,
    );

    entered.expect("real authenticated PUT reached admitted body gate");
    assert_eq!(evidence.overflow, 0);
    assert_eq!(evidence.len, 3);
    let dispatch = evidence.entries[0].expect("dispatch deadline evidence");
    assert_eq!(dispatch.phase, ConnectionPhase::Dispatch);
    assert_eq!(
        dispatch.io_kind, None,
        "the original deadline check is not a fabricated I/O error"
    );
    assert_eq!(dispatch.status, Some(408));
    assert!(dispatch.dispatch_elapsed_ms.is_some());
    assert_eq!(dispatch.response_write_elapsed_ms, None);
    let response = evidence.entries[1].expect("response write deadline evidence");
    assert_eq!(response.phase, ConnectionPhase::ResponseWrite);
    assert_eq!(response.io_kind, Some(io::ErrorKind::TimedOut));
    assert_eq!(response.status, Some(408));
    assert_eq!(response.dispatch_elapsed_ms, dispatch.dispatch_elapsed_ms);
    assert!(response.response_write_elapsed_ms.is_some());
    let close = evidence.entries[2].expect("close flush deadline evidence");
    assert_eq!(close.phase, ConnectionPhase::CloseFlush);
    assert_eq!(close.io_kind, Some(io::ErrorKind::TimedOut));
    assert_eq!(close.status, None);
    assert_eq!(close.dispatch_elapsed_ms, response.dispatch_elapsed_ms);
    assert_eq!(
        close.response_write_elapsed_ms,
        response.response_write_elapsed_ms,
    );
    for event in [dispatch, response, close] {
        assert!(event.deadline_expired);
        assert!(event.elapsed_ms >= request_timeout.as_millis());
        assert!(event.dispatch_elapsed_ms.unwrap() <= event.elapsed_ms);
        if let Some(response_write_elapsed_ms) = event.response_write_elapsed_ms {
            assert!(response_write_elapsed_ms <= event.elapsed_ms);
        }
    }
    let client = client_result
        .expect("client lifetime retained its real thread")
        .expect("diagnostic client thread did not panic")
        .expect("real TLS handshake and partial request completed");
    assert!(client.handshake_complete);
    assert_eq!(client.end, Err(io::ErrorKind::UnexpectedEof));
    assert!(staging_empty.unwrap());
    assert!(uncommitted);
    assert!(admission_released);
    assert_eq!(control.unwrap(), (404, Some("close".into()), true));
    assert_eq!(after_control.len, evidence.len);
    assert_eq!(after_control.overflow, 0);
    assert!(resources_released);
}
