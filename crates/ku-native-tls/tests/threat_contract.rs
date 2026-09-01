use std::collections::VecDeque;
use std::io::{Cursor, Read, Write};
use std::ptr;
use std::sync::Arc;

use ku_native_tls::*;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, SupportedProtocolVersion};

struct Fixture {
    certificate_pem: String,
    certificate_der: rustls::pki_types::CertificateDer<'static>,
    private_key_der: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("generate test certificate");
        Self {
            certificate_pem: cert.pem(),
            certificate_der: cert.der().clone(),
            private_key_der: key_pair.serialize_der(),
        }
    }

    fn server(&self, version: &'static SupportedProtocolVersion) -> ServerConnection {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[version])
            .expect("test protocol is supported")
            .with_no_client_auth()
            .with_single_cert(
                vec![self.certificate_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.private_key_der.clone())),
            )
            .expect("build test server config");
        ServerConnection::new(Arc::new(config)).expect("create test server")
    }
}

struct Client {
    session: *mut KuTlsClientSession,
}

impl Client {
    fn custom(pem: &[u8], server_name: &[u8]) -> Result<Self, u32> {
        let mut config = ptr::null_mut();
        // SAFETY: Slices and output slots are live for each call.
        let status = unsafe {
            ku_tls_v1_config_new(
                KU_TLS_ROOTS_CUSTOM_PEM,
                pem.as_ptr(),
                pem.len(),
                &mut config,
            )
        };
        if status != KU_TLS_STATUS_OK {
            return Err(status);
        }
        let mut session = ptr::null_mut();
        // SAFETY: The config, name slice, and output slot are live. The config
        // is consumed exactly once after the session retains its own reference.
        let status = unsafe {
            ku_tls_v1_client_new(
                config,
                server_name.as_ptr(),
                server_name.len(),
                &mut session,
            )
        };
        // SAFETY: `config` is live and uniquely owned here.
        assert_eq!(unsafe { ku_tls_v1_config_drop(config) }, KU_TLS_STATUS_OK);
        if status != KU_TLS_STATUS_OK {
            return Err(status);
        }
        Ok(Self { session })
    }

    fn webpki(server_name: &[u8]) -> Self {
        let mut config = ptr::null_mut();
        assert_eq!(
            // SAFETY: The output slot is live and the empty input has no pointer.
            unsafe { ku_tls_v1_config_new(KU_TLS_ROOTS_WEBPKI, ptr::null(), 0, &mut config) },
            KU_TLS_STATUS_OK
        );
        let mut session = ptr::null_mut();
        assert_eq!(
            // SAFETY: The config, name slice, and output slot are live.
            unsafe {
                ku_tls_v1_client_new(
                    config,
                    server_name.as_ptr(),
                    server_name.len(),
                    &mut session,
                )
            },
            KU_TLS_STATUS_OK
        );
        // SAFETY: `config` is live and uniquely owned here.
        assert_eq!(unsafe { ku_tls_v1_config_drop(config) }, KU_TLS_STATUS_OK);
        Self { session }
    }

    fn bool_state(
        &self,
        query: unsafe extern "C" fn(*const KuTlsClientSession, *mut u32) -> u32,
    ) -> bool {
        let mut value = 0;
        // SAFETY: The session and output slot are live for this call.
        assert_eq!(unsafe { query(self.session, &mut value) }, KU_TLS_STATUS_OK);
        value != 0
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: This wrapper uniquely owns and consumes the live session.
        assert_eq!(
            unsafe { ku_tls_v1_client_drop(self.session) },
            KU_TLS_STATUS_OK
        );
        self.session = ptr::null_mut();
    }
}

fn drain_client(client: &Client, queue: &mut VecDeque<u8>, fragment: usize) {
    if !client.bool_state(ku_tls_v1_client_wants_write) {
        return;
    }
    let mut output = vec![0u8; fragment];
    let mut written = 0;
    assert_eq!(
        // SAFETY: The session and local output storage are live.
        unsafe {
            ku_tls_v1_client_drain_ciphertext(
                client.session,
                output.as_mut_ptr(),
                output.len(),
                &mut written,
            )
        },
        KU_TLS_STATUS_OK
    );
    queue.extend(&output[..written]);
}

fn drain_client_all(client: &Client, queue: &mut VecDeque<u8>, fragment: usize) {
    assert_ne!(fragment, 0);
    let max_steps = KU_TLS_MAX_IO_BYTES.div_ceil(fragment) + 128;
    for _ in 0..max_steps {
        if !client.bool_state(ku_tls_v1_client_wants_write) {
            return;
        }
        let before = queue.len();
        drain_client(client, queue, fragment);
        assert!(
            queue.len() > before,
            "pending ciphertext must make progress"
        );
    }
    panic!("bounded client drain did not converge");
}

fn drain_server(server: &mut ServerConnection, queue: &mut VecDeque<u8>) {
    if !server.wants_write() {
        return;
    }
    let mut output = Vec::new();
    server
        .write_tls(&mut output)
        .expect("drain test server TLS bytes");
    queue.extend(output);
}

fn feed_server(server: &mut ServerConnection, queue: &mut VecDeque<u8>, fragment: usize) {
    if !server.wants_read() || queue.is_empty() {
        return;
    }
    let count = fragment.min(queue.len());
    let input = queue.drain(..count).collect::<Vec<_>>();
    let consumed = server
        .read_tls(&mut Cursor::new(&input))
        .expect("feed test server TLS bytes");
    assert_eq!(consumed, input.len());
    server
        .process_new_packets()
        .expect("process test server TLS bytes");
}

fn feed_client(client: &Client, queue: &mut VecDeque<u8>, fragment: usize) -> Result<(), u32> {
    if !client.bool_state(ku_tls_v1_client_wants_read) || queue.is_empty() {
        return Ok(());
    }
    let count = fragment.min(queue.len());
    let input = queue.drain(..count).collect::<Vec<_>>();
    let mut consumed = 0;
    // SAFETY: The session, input fragment, and output slot are live.
    let status = unsafe {
        ku_tls_v1_client_feed_ciphertext(client.session, input.as_ptr(), input.len(), &mut consumed)
    };
    if status != KU_TLS_STATUS_OK {
        return Err(status);
    }
    assert_eq!(consumed, input.len());
    // SAFETY: The session is live and accessed only by this test.
    let status = unsafe { ku_tls_v1_client_process(client.session) };
    if status == KU_TLS_STATUS_OK {
        Ok(())
    } else {
        Err(status)
    }
}

fn handshake(client: &Client, server: &mut ServerConnection, fragment: usize) -> Result<(), u32> {
    let mut client_to_server = VecDeque::new();
    let mut server_to_client = VecDeque::new();
    for _ in 0..50_000 {
        drain_client(client, &mut client_to_server, fragment);
        feed_server(server, &mut client_to_server, fragment);
        drain_server(server, &mut server_to_client);
        feed_client(client, &mut server_to_client, fragment)?;
        if !client.bool_state(ku_tls_v1_client_is_handshaking)
            && !server.is_handshaking()
            && client_to_server.is_empty()
            && server_to_client.is_empty()
        {
            return Ok(());
        }
    }
    panic!("bounded in-memory handshake did not converge");
}

fn transfer_server_to_client(
    client: &Client,
    server: &mut ServerConnection,
    fragment: usize,
) -> Result<(), u32> {
    let mut queue = VecDeque::new();
    for _ in 0..50_000 {
        drain_server(server, &mut queue);
        feed_client(client, &mut queue, fragment)?;
        if !server.wants_write() && queue.is_empty() {
            return Ok(());
        }
    }
    panic!("bounded server-to-client transfer did not converge");
}

fn base64_certificate(der: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = Vec::with_capacity(der.len().div_ceil(3) * 4 + 64);
    encoded.extend_from_slice(b"-----BEGIN CERTIFICATE-----\n");
    for chunk in der.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize]);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize]);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize]
        } else {
            b'='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize]
        } else {
            b'='
        });
    }
    encoded.extend_from_slice(b"\n-----END CERTIFICATE-----\n");
    encoded
}

#[test]
fn tls12_and_tls13_handshake_and_binary_plaintext_round_trip() {
    // SAFETY: All ABI handles and buffers below remain live and non-aliasing.
    unsafe {
        let fixture = Fixture::new();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let client = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
                .expect("create custom-root client");
            let mut server = fixture.server(version);
            handshake(&client, &mut server, 97).expect("complete verified handshake");

            let request = b"hello\0from-ku";
            let mut written = 0;
            assert_eq!(
                ku_tls_v1_client_write_plaintext(
                    client.session,
                    request.as_ptr(),
                    request.len(),
                    &mut written,
                ),
                KU_TLS_STATUS_OK
            );
            assert_eq!(written, request.len());
            let mut encrypted = VecDeque::new();
            drain_client_all(&client, &mut encrypted, 31);
            let max_server_feed_steps = encrypted.len().div_ceil(17) + 1;
            for _ in 0..max_server_feed_steps {
                if encrypted.is_empty() {
                    break;
                }
                let before = encrypted.len();
                feed_server(&mut server, &mut encrypted, 17);
                assert!(encrypted.len() < before, "server feed must make progress");
            }
            assert!(encrypted.is_empty(), "bounded server feed did not converge");
            let mut received = vec![0; request.len()];
            assert_eq!(
                server.reader().read(&mut received).expect("read request"),
                request.len()
            );
            assert_eq!(received, request);

            let response = b"world\0from-rustls";
            assert_eq!(
                server.writer().write(response).expect("write response"),
                response.len()
            );
            transfer_server_to_client(&client, &mut server, 23).expect("transfer response");
            let mut received = vec![0; response.len()];
            let mut read = 0;
            assert_eq!(
                ku_tls_v1_client_read_plaintext(
                    client.session,
                    received.as_mut_ptr(),
                    received.len(),
                    &mut read,
                ),
                KU_TLS_STATUS_OK
            );
            assert_eq!(read, response.len());
            assert_eq!(received, response);
        }
    }
}

#[test]
fn one_byte_fragmentation_is_bounded_and_completes() {
    let fixture = Fixture::new();
    let client = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
        .expect("create custom-root client");
    let mut server = fixture.server(&rustls::version::TLS13);
    handshake(&client, &mut server, 1).expect("complete fragmented handshake");
}

#[test]
fn wrong_hostname_and_untrusted_certificate_fail_closed() {
    let fixture = Fixture::new();

    let wrong_name = Client::custom(fixture.certificate_pem.as_bytes(), b"example.invalid")
        .expect("create wrong-name client");
    let mut server = fixture.server(&rustls::version::TLS13);
    assert_eq!(
        handshake(&wrong_name, &mut server, 256),
        Err(KU_TLS_STATUS_TLS_ERROR)
    );

    let untrusted = Client::webpki(b"localhost");
    let mut server = fixture.server(&rustls::version::TLS13);
    assert_eq!(
        handshake(&untrusted, &mut server, 256),
        Err(KU_TLS_STATUS_TLS_ERROR)
    );
}

#[test]
fn malformed_and_oversized_ca_inputs_are_rejected() {
    // SAFETY: All input slices and output slots are live for each ABI call.
    unsafe {
        for malformed in [
            b"not pem".as_slice(),
            b"-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n".as_slice(),
            b"-----BEGIN CERTIFICATE-----\n%%%%\n-----END CERTIFICATE-----\n".as_slice(),
            b"garbage\n-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n".as_slice(),
        ] {
            let mut config = ptr::null_mut();
            assert_eq!(
                ku_tls_v1_config_new(
                    KU_TLS_ROOTS_CUSTOM_PEM,
                    malformed.as_ptr(),
                    malformed.len(),
                    &mut config,
                ),
                KU_TLS_STATUS_INVALID_CA
            );
            assert!(config.is_null());
        }

        let fixture = Fixture::new();
        for mixed in [
            format!("{}garbage", fixture.certificate_pem),
            format!(
                "{}-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
                fixture.certificate_pem
            ),
        ] {
            let mut config = ptr::null_mut();
            assert_eq!(
                ku_tls_v1_config_new(
                    KU_TLS_ROOTS_CUSTOM_PEM,
                    mixed.as_ptr(),
                    mixed.len(),
                    &mut config,
                ),
                KU_TLS_STATUS_INVALID_CA
            );
            assert!(config.is_null());
        }

        let too_many = fixture
            .certificate_pem
            .repeat(KU_TLS_MAX_CA_CERTIFICATES + 1);
        let mut config = ptr::null_mut();
        assert_eq!(
            ku_tls_v1_config_new(
                KU_TLS_ROOTS_CUSTOM_PEM,
                too_many.as_ptr(),
                too_many.len(),
                &mut config,
            ),
            KU_TLS_STATUS_LIMIT_EXCEEDED
        );
        assert!(config.is_null());

        let oversized_der = base64_certificate(&vec![0u8; KU_TLS_MAX_CA_DER_BYTES + 1]);
        assert_eq!(
            ku_tls_v1_config_new(
                KU_TLS_ROOTS_CUSTOM_PEM,
                oversized_der.as_ptr(),
                oversized_der.len(),
                &mut config,
            ),
            KU_TLS_STATUS_LIMIT_EXCEEDED
        );
        assert!(config.is_null());
    }
}

#[test]
fn malformed_tls_record_poisoning_is_terminal_but_alert_is_drainable() {
    // SAFETY: The session is live and exclusively accessed by this test.
    unsafe {
        let fixture = Fixture::new();
        let client = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
            .expect("create custom-root client");
        let discarded_hello_capacity = 4096;
        let mut discarded = VecDeque::new();
        drain_client_all(&client, &mut discarded, discarded_hello_capacity);
        assert!(!discarded.is_empty());

        let malformed = [0xff, 0x03, 0x03, 0x00, 0x00];
        let mut consumed = 0;
        assert_eq!(
            ku_tls_v1_client_feed_ciphertext(
                client.session,
                malformed.as_ptr(),
                malformed.len(),
                &mut consumed,
            ),
            KU_TLS_STATUS_OK
        );
        assert_eq!(consumed, malformed.len());
        assert_eq!(
            ku_tls_v1_client_process(client.session),
            KU_TLS_STATUS_TLS_ERROR
        );
        assert_eq!(
            ku_tls_v1_client_process(client.session),
            KU_TLS_STATUS_SESSION_FAILED
        );

        if client.bool_state(ku_tls_v1_client_wants_write) {
            let mut alert = [0u8; 256];
            let mut written = 0;
            assert_eq!(
                ku_tls_v1_client_drain_ciphertext(
                    client.session,
                    alert.as_mut_ptr(),
                    alert.len(),
                    &mut written,
                ),
                KU_TLS_STATUS_OK
            );
            assert_ne!(written, 0);
        }
    }
}

#[test]
fn io_limits_reject_lengths_before_dereferencing_and_enforce_backpressure() {
    // SAFETY: The deliberately invalid input address is rejected by its length
    // before dereference; all other handles and buffers are live.
    unsafe {
        let fixture = Fixture::new();
        let client = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
            .expect("create custom-root client");

        let mut read = usize::MAX;
        assert_eq!(
            ku_tls_v1_client_read_plaintext(client.session, ptr::null_mut(), 0, &mut read,),
            KU_TLS_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(read, 0);

        let mut count = usize::MAX;
        assert_eq!(
            ku_tls_v1_client_write_plaintext(client.session, ptr::null(), 0, &mut count),
            KU_TLS_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(count, 0);

        count = usize::MAX;
        assert_eq!(
            ku_tls_v1_client_write_plaintext(
                client.session,
                ptr::dangling::<u8>(),
                KU_TLS_MAX_IO_BYTES + 1,
                &mut count,
            ),
            KU_TLS_STATUS_LIMIT_EXCEEDED
        );
        assert_eq!(count, 0);

        let mut zero_written = usize::MAX;
        assert_eq!(
            ku_tls_v1_client_drain_ciphertext(
                client.session,
                ptr::null_mut(),
                0,
                &mut zero_written,
            ),
            KU_TLS_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(zero_written, 0);

        let mut hello = [0u8; 1];
        let mut written = 0;
        assert_eq!(
            ku_tls_v1_client_drain_ciphertext(
                client.session,
                hello.as_mut_ptr(),
                hello.len(),
                &mut written,
            ),
            KU_TLS_STATUS_OK
        );
        assert_eq!(written, 1);

        let input = [22u8];
        let mut consumed = 99;
        assert_eq!(
            ku_tls_v1_client_feed_ciphertext(
                client.session,
                input.as_ptr(),
                input.len(),
                &mut consumed,
            ),
            KU_TLS_STATUS_WOULD_BLOCK
        );
        assert_eq!(consumed, 0);

        let mut total = written;
        for _ in 0..4096 {
            if !client.bool_state(ku_tls_v1_client_wants_write) {
                break;
            }
            written = 0;
            assert_eq!(
                ku_tls_v1_client_drain_ciphertext(
                    client.session,
                    hello.as_mut_ptr(),
                    hello.len(),
                    &mut written,
                ),
                KU_TLS_STATUS_OK
            );
            assert_eq!(written, 1, "pending ciphertext must make progress");
            total += written;
        }
        assert!(!client.bool_state(ku_tls_v1_client_wants_write));
        assert!(total > 1);

        consumed = 99;
        assert_eq!(
            ku_tls_v1_client_feed_ciphertext(client.session, ptr::null(), 0, &mut consumed,),
            KU_TLS_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(consumed, 0);

        consumed = 0;
        assert_eq!(
            ku_tls_v1_client_feed_ciphertext(
                client.session,
                input.as_ptr(),
                input.len(),
                &mut consumed,
            ),
            KU_TLS_STATUS_OK
        );
        assert_eq!(consumed, 1);
        consumed = 99;
        assert_eq!(
            ku_tls_v1_client_feed_ciphertext(
                client.session,
                input.as_ptr(),
                input.len(),
                &mut consumed,
            ),
            KU_TLS_STATUS_WOULD_BLOCK
        );
        assert_eq!(consumed, 0);
        assert_eq!(ku_tls_v1_client_process(client.session), KU_TLS_STATUS_OK);
    }
}

#[test]
fn transport_eof_requires_authenticated_close_notify() {
    // SAFETY: Both sessions are live and exclusively accessed by this test.
    unsafe {
        let fixture = Fixture::new();

        let truncated = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
            .expect("create custom-root client");
        let mut server = fixture.server(&rustls::version::TLS13);
        handshake(&truncated, &mut server, 128).expect("complete handshake");
        assert_eq!(
            ku_tls_v1_client_notify_eof(truncated.session),
            KU_TLS_STATUS_TRUNCATED
        );

        let closed = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
            .expect("create custom-root client");
        let mut server = fixture.server(&rustls::version::TLS13);
        handshake(&closed, &mut server, 128).expect("complete handshake");
        server.send_close_notify();
        transfer_server_to_client(&closed, &mut server, 19).expect("transfer close notify");
        assert!(closed.bool_state(ku_tls_v1_client_peer_closed));
        let mut output = [0u8; 1];
        let mut read = usize::MAX;
        assert_eq!(
            ku_tls_v1_client_read_plaintext(
                closed.session,
                output.as_mut_ptr(),
                output.len(),
                &mut read,
            ),
            KU_TLS_STATUS_OK
        );
        assert_eq!(
            read, 0,
            "clean TLS EOF must be distinguishable via peer_closed"
        );
        assert_eq!(
            ku_tls_v1_client_notify_eof(closed.session),
            KU_TLS_STATUS_OK
        );
        assert_eq!(
            ku_tls_v1_client_send_close_notify(closed.session),
            KU_TLS_STATUS_OK
        );
    }
}

#[test]
fn local_close_notify_is_idempotent_and_observed_by_peer() {
    // SAFETY: The session and every referenced buffer remain live for each call.
    unsafe {
        let fixture = Fixture::new();
        let client = Client::custom(fixture.certificate_pem.as_bytes(), b"localhost")
            .expect("create custom-root client");
        let mut server = fixture.server(&rustls::version::TLS13);
        handshake(&client, &mut server, 128).expect("complete handshake");

        assert_eq!(
            ku_tls_v1_client_send_close_notify(client.session),
            KU_TLS_STATUS_OK
        );
        assert_eq!(
            ku_tls_v1_client_send_close_notify(client.session),
            KU_TLS_STATUS_OK
        );
        let mut encrypted = VecDeque::new();
        drain_client_all(&client, &mut encrypted, 11);
        let encrypted = encrypted.into_iter().collect::<Vec<_>>();
        assert_eq!(
            server
                .read_tls(&mut Cursor::new(&encrypted))
                .expect("feed close notify"),
            encrypted.len()
        );
        let state = server.process_new_packets().expect("process close notify");
        assert!(state.peer_has_closed());

        let mut written = 0;
        assert_eq!(
            ku_tls_v1_client_write_plaintext(
                client.session,
                b"late".as_ptr(),
                b"late".len(),
                &mut written,
            ),
            KU_TLS_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(written, 0);
    }
}
