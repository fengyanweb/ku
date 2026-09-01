//! Bounded, socket-free TLS state machine for Ku's native runtimes.

use std::io::{self, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::Arc;

use rustls::pki_types::pem::{PemObject, SectionKind};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};

#[cfg(not(panic = "unwind"))]
compile_error!("ku-native-tls requires panic=unwind so panics cannot cross the C ABI");

pub const KU_TLS_ABI_VERSION: u32 = 1;

pub const KU_TLS_STATUS_OK: u32 = 0;
pub const KU_TLS_STATUS_NULL_POINTER: u32 = 1;
pub const KU_TLS_STATUS_INVALID_ARGUMENT: u32 = 2;
pub const KU_TLS_STATUS_LIMIT_EXCEEDED: u32 = 3;
pub const KU_TLS_STATUS_INVALID_DNS_NAME: u32 = 4;
pub const KU_TLS_STATUS_INVALID_CA: u32 = 5;
pub const KU_TLS_STATUS_TLS_ERROR: u32 = 6;
pub const KU_TLS_STATUS_SESSION_FAILED: u32 = 7;
pub const KU_TLS_STATUS_TRUNCATED: u32 = 8;
pub const KU_TLS_STATUS_WOULD_BLOCK: u32 = 9;
pub const KU_TLS_STATUS_IO_ERROR: u32 = 10;
pub const KU_TLS_STATUS_PANIC: u32 = 255;

pub const KU_TLS_ROOTS_WEBPKI: u32 = 0;
pub const KU_TLS_ROOTS_CUSTOM_PEM: u32 = 1;

pub const KU_TLS_MAX_CA_PEM_BYTES: usize = 4 * 1024 * 1024;
pub const KU_TLS_MAX_CA_CERTIFICATES: usize = 1024;
pub const KU_TLS_MAX_CA_DER_BYTES: usize = 64 * 1024;
pub const KU_TLS_MAX_IO_BYTES: usize = 64 * 1024;
pub const KU_TLS_MAX_HANDSHAKE_BYTES: u64 = 1024 * 1024;
pub const KU_TLS_MAX_HANDSHAKE_ITERATIONS: u32 = 4096;
pub const KU_TLS_MAX_SERVER_NAME_BYTES: usize = 253;
pub const KU_TLS_RESUMPTION_CACHE_ENTRIES: usize = 64;

const BUILD_ID: &[u8] = b"ku-native-tls/0.1.0;abi=1;rustls=0.23.40;ring=0.17.14;\
webpki-roots=1.0.7;buffer=65536;handshake=1048576;resumption=64";

pub struct KuTlsConfig {
    config: Arc<ClientConfig>,
}

pub struct KuTlsClientSession {
    connection: ClientConnection,
    handshake_bytes: u64,
    handshake_iterations: u32,
    needs_process: bool,
    peer_closed: bool,
    local_close_sent: bool,
    transport_eof: bool,
    failed: bool,
}

fn ffi_status(operation: impl FnOnce() -> Result<(), u32>) -> u32 {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => KU_TLS_STATUS_OK,
        Ok(Err(status)) => status,
        Err(_) => KU_TLS_STATUS_PANIC,
    }
}

#[no_mangle]
pub extern "C" fn ku_tls_abi_version() -> u32 {
    KU_TLS_ABI_VERSION
}

#[no_mangle]
/// Returns a process-lifetime build identifier byte view.
///
/// # Safety
/// Both outputs must be valid, writable, non-aliasing pointers.
pub unsafe extern "C" fn ku_tls_v1_build_id(out_data: *mut *const u8, out_len: *mut usize) -> u32 {
    ffi_status(|| {
        if out_data.is_null() || out_len.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The non-null output pointers are part of the C ABI contract.
        unsafe {
            ptr::write(out_data, BUILD_ID.as_ptr());
            ptr::write(out_len, BUILD_ID.len());
        }
        Ok(())
    })
}

unsafe fn bounded_input<'a>(data: *const u8, len: usize, max_len: usize) -> Result<&'a [u8], u32> {
    if len > max_len {
        return Err(KU_TLS_STATUS_LIMIT_EXCEEDED);
    }
    if len == 0 {
        return Ok(&[]);
    }
    if data.is_null() {
        return Err(KU_TLS_STATUS_NULL_POINTER);
    }
    // SAFETY: The caller promises a readable region of `len` bytes. The length
    // is bounded above before constructing the slice.
    Ok(unsafe { slice::from_raw_parts(data, len) })
}

unsafe fn bounded_output<'a>(
    data: *mut u8,
    len: usize,
    max_len: usize,
) -> Result<&'a mut [u8], u32> {
    if len > max_len {
        return Err(KU_TLS_STATUS_LIMIT_EXCEEDED);
    }
    if len == 0 {
        return Ok(&mut []);
    }
    if data.is_null() {
        return Err(KU_TLS_STATUS_NULL_POINTER);
    }
    // SAFETY: The caller promises a writable region of `len` bytes. The length
    // is bounded above before constructing the slice.
    Ok(unsafe { slice::from_raw_parts_mut(data, len) })
}

fn strict_custom_roots(pem: &[u8]) -> Result<RootCertStore, u32> {
    if pem.is_empty() {
        return Err(KU_TLS_STATUS_INVALID_CA);
    }

    let mut roots = RootCertStore::empty();
    let mut remaining = pem;
    let mut certificate_count = 0usize;
    const BEGIN_CERTIFICATE: &[u8] = b"-----BEGIN CERTIFICATE-----";

    loop {
        let whitespace = remaining
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        remaining = &remaining[whitespace..];
        if remaining.is_empty() {
            break;
        }
        if !remaining.starts_with(BEGIN_CERTIFICATE) {
            return Err(KU_TLS_STATUS_INVALID_CA);
        }

        let before_len = remaining.len();
        let mut sections = <(SectionKind, Vec<u8>)>::pem_slice_iter(remaining);
        let Some(section) = sections.next() else {
            return Err(KU_TLS_STATUS_INVALID_CA);
        };
        let (kind, der) = section.map_err(|_| KU_TLS_STATUS_INVALID_CA)?;
        let rest = sections.remainder();
        if rest.len() >= before_len {
            return Err(KU_TLS_STATUS_INVALID_CA);
        }
        remaining = rest;

        if kind != SectionKind::Certificate {
            return Err(KU_TLS_STATUS_INVALID_CA);
        }
        certificate_count = certificate_count
            .checked_add(1)
            .ok_or(KU_TLS_STATUS_LIMIT_EXCEEDED)?;
        if certificate_count > KU_TLS_MAX_CA_CERTIFICATES {
            return Err(KU_TLS_STATUS_LIMIT_EXCEEDED);
        }
        if der.len() > KU_TLS_MAX_CA_DER_BYTES {
            return Err(KU_TLS_STATUS_LIMIT_EXCEEDED);
        }
        roots
            .add(CertificateDer::from(der))
            .map_err(|_| KU_TLS_STATUS_INVALID_CA)?;
    }

    if certificate_count == 0 {
        return Err(KU_TLS_STATUS_INVALID_CA);
    }
    Ok(roots)
}

fn make_client_config(root_mode: u32, custom_pem: &[u8]) -> Result<ClientConfig, u32> {
    let roots = match root_mode {
        KU_TLS_ROOTS_WEBPKI => {
            if !custom_pem.is_empty() {
                return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
            }
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            roots
        }
        KU_TLS_ROOTS_CUSTOM_PEM => strict_custom_roots(custom_pem)?,
        _ => return Err(KU_TLS_STATUS_INVALID_ARGUMENT),
    };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| KU_TLS_STATUS_TLS_ERROR)?;
    let mut config = builder.with_root_certificates(roots).with_no_client_auth();
    config.resumption =
        rustls::client::Resumption::in_memory_sessions(KU_TLS_RESUMPTION_CACHE_ENTRIES);
    Ok(config)
}

#[no_mangle]
/// Creates an immutable TLS client configuration.
///
/// # Safety
/// `out_config` must be writable. A non-empty PEM pointer must be readable for
/// its stated length and must not alias `out_config`.
pub unsafe extern "C" fn ku_tls_v1_config_new(
    root_mode: u32,
    custom_ca_pem: *const u8,
    custom_ca_pem_len: usize,
    out_config: *mut *mut KuTlsConfig,
) -> u32 {
    ffi_status(|| {
        if out_config.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The output pointer is non-null and owned by the caller.
        unsafe { ptr::write(out_config, ptr::null_mut()) };
        // Validate the attacker-controlled length before reading the pointer.
        let pem =
            unsafe { bounded_input(custom_ca_pem, custom_ca_pem_len, KU_TLS_MAX_CA_PEM_BYTES)? };
        if root_mode == KU_TLS_ROOTS_CUSTOM_PEM && custom_ca_pem_len == 0 {
            return Err(KU_TLS_STATUS_INVALID_CA);
        }
        if root_mode == KU_TLS_ROOTS_WEBPKI && !custom_ca_pem.is_null() {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }

        let handle = Box::new(KuTlsConfig {
            config: Arc::new(make_client_config(root_mode, pem)?),
        });
        // SAFETY: Ownership of the box is transferred to the caller.
        unsafe { ptr::write(out_config, Box::into_raw(handle)) };
        Ok(())
    })
}

#[no_mangle]
/// Drops a TLS client configuration. A null handle is accepted.
///
/// # Safety
/// A non-null handle must be live, uniquely owned, and consumed exactly once.
pub unsafe extern "C" fn ku_tls_v1_config_drop(config: *mut KuTlsConfig) -> u32 {
    ffi_status(|| {
        if !config.is_null() {
            // SAFETY: The ABI requires this pointer to be a live handle returned
            // by `ku_tls_v1_config_new`, consumed exactly once here.
            unsafe { drop(Box::from_raw(config)) };
        }
        Ok(())
    })
}

#[no_mangle]
/// Creates a socket-free TLS client session.
///
/// # Safety
/// `config` must be a live handle, `server_name` must be readable for its stated
/// length, and `out_session` must be a writable non-aliasing pointer.
pub unsafe extern "C" fn ku_tls_v1_client_new(
    config: *const KuTlsConfig,
    server_name: *const u8,
    server_name_len: usize,
    out_session: *mut *mut KuTlsClientSession,
) -> u32 {
    ffi_status(|| {
        if out_session.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The output pointer is non-null and owned by the caller.
        unsafe { ptr::write(out_session, ptr::null_mut()) };
        if config.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        let name_bytes =
            unsafe { bounded_input(server_name, server_name_len, KU_TLS_MAX_SERVER_NAME_BYTES)? };
        if name_bytes.is_empty() {
            return Err(KU_TLS_STATUS_INVALID_DNS_NAME);
        }
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| KU_TLS_STATUS_INVALID_DNS_NAME)?
            .to_owned();
        let server_name = ServerName::try_from(name).map_err(|_| KU_TLS_STATUS_INVALID_DNS_NAME)?;
        // SAFETY: The ABI requires a live immutable config handle for this call.
        let config = unsafe { &*config };
        let mut connection = ClientConnection::new(Arc::clone(&config.config), server_name)
            .map_err(|_| KU_TLS_STATUS_TLS_ERROR)?;
        connection.set_buffer_limit(Some(KU_TLS_MAX_IO_BYTES));

        let session = Box::new(KuTlsClientSession {
            connection,
            handshake_bytes: 0,
            handshake_iterations: 0,
            needs_process: false,
            peer_closed: false,
            local_close_sent: false,
            transport_eof: false,
            failed: false,
        });
        // SAFETY: Ownership of the box is transferred to the caller.
        unsafe { ptr::write(out_session, Box::into_raw(session)) };
        Ok(())
    })
}

#[no_mangle]
/// Drops a TLS client session. A null handle is accepted.
///
/// # Safety
/// A non-null handle must be live, uniquely owned, and consumed exactly once.
pub unsafe extern "C" fn ku_tls_v1_client_drop(session: *mut KuTlsClientSession) -> u32 {
    ffi_status(|| {
        if !session.is_null() {
            // SAFETY: The ABI requires this pointer to be a live handle returned
            // by `ku_tls_v1_client_new`, consumed exactly once here.
            unsafe { drop(Box::from_raw(session)) };
        }
        Ok(())
    })
}

unsafe fn session_mut<'a>(
    session: *mut KuTlsClientSession,
) -> Result<&'a mut KuTlsClientSession, u32> {
    // SAFETY: `as_mut` only creates a reference for a non-null pointer. Handle
    // provenance, uniqueness, and lifetime are requirements of the C ABI.
    unsafe { session.as_mut() }.ok_or(KU_TLS_STATUS_NULL_POINTER)
}

unsafe fn session_ref<'a>(
    session: *const KuTlsClientSession,
) -> Result<&'a KuTlsClientSession, u32> {
    // SAFETY: `as_ref` only creates a reference for a non-null pointer. Handle
    // provenance and lifetime are requirements of the C ABI.
    unsafe { session.as_ref() }.ok_or(KU_TLS_STATUS_NULL_POINTER)
}

fn set_bool(out: *mut u32, value: bool) -> Result<(), u32> {
    if out.is_null() {
        return Err(KU_TLS_STATUS_NULL_POINTER);
    }
    // SAFETY: The caller provides a writable u32 output location.
    unsafe { ptr::write(out, u32::from(value)) };
    Ok(())
}

#[no_mangle]
/// Reports whether ciphertext may be fed without violating backpressure.
///
/// # Safety
/// The session must be live and `out_wants_read` must be writable. The session
/// must not be concurrently mutated.
pub unsafe extern "C" fn ku_tls_v1_client_wants_read(
    session: *const KuTlsClientSession,
    out_wants_read: *mut u32,
) -> u32 {
    ffi_status(|| {
        set_bool(out_wants_read, false)?;
        let session = unsafe { session_ref(session)? };
        set_bool(
            out_wants_read,
            !session.failed
                && !session.transport_eof
                && !session.needs_process
                && session.connection.wants_read(),
        )
    })
}

#[no_mangle]
/// Reports whether ciphertext is ready to drain.
///
/// # Safety
/// The session must be live and `out_wants_write` must be writable. The session
/// must not be concurrently mutated.
pub unsafe extern "C" fn ku_tls_v1_client_wants_write(
    session: *const KuTlsClientSession,
    out_wants_write: *mut u32,
) -> u32 {
    ffi_status(|| {
        set_bool(out_wants_write, false)?;
        let session = unsafe { session_ref(session)? };
        // Fatal alerts queued by rustls remain drainable after a TLS error.
        set_bool(out_wants_write, session.connection.wants_write())
    })
}

#[no_mangle]
/// Reports whether the TLS handshake is incomplete.
///
/// # Safety
/// The session must be live and the output must be writable. The session must
/// not be concurrently mutated.
pub unsafe extern "C" fn ku_tls_v1_client_is_handshaking(
    session: *const KuTlsClientSession,
    out_is_handshaking: *mut u32,
) -> u32 {
    ffi_status(|| {
        set_bool(out_is_handshaking, false)?;
        let session = unsafe { session_ref(session)? };
        set_bool(out_is_handshaking, session.connection.is_handshaking())
    })
}

#[no_mangle]
/// Reports whether an authenticated peer `close_notify` was processed.
///
/// # Safety
/// The session must be live and the output must be writable. The session must
/// not be concurrently mutated.
pub unsafe extern "C" fn ku_tls_v1_client_peer_closed(
    session: *const KuTlsClientSession,
    out_peer_closed: *mut u32,
) -> u32 {
    ffi_status(|| {
        set_bool(out_peer_closed, false)?;
        let session = unsafe { session_ref(session)? };
        set_bool(out_peer_closed, session.peer_closed)
    })
}

#[no_mangle]
/// Feeds one bounded ciphertext fragment into the state machine.
///
/// # Safety
/// The session must be live and exclusively accessed. Ciphertext must be
/// readable for its stated length and `out_consumed` must be writable and
/// non-aliasing.
pub unsafe extern "C" fn ku_tls_v1_client_feed_ciphertext(
    session: *mut KuTlsClientSession,
    ciphertext: *const u8,
    ciphertext_len: usize,
    out_consumed: *mut usize,
) -> u32 {
    ffi_status(|| {
        if out_consumed.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The output pointer is non-null and owned by the caller.
        unsafe { ptr::write(out_consumed, 0) };
        let input = unsafe { bounded_input(ciphertext, ciphertext_len, KU_TLS_MAX_IO_BYTES)? };
        let session = unsafe { session_mut(session)? };
        if session.failed {
            return Err(KU_TLS_STATUS_SESSION_FAILED);
        }
        if input.is_empty() {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }
        if session.transport_eof || session.peer_closed {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }
        if session.needs_process || !session.connection.wants_read() {
            return Err(KU_TLS_STATUS_WOULD_BLOCK);
        }

        let was_handshaking = session.connection.is_handshaking();
        if was_handshaking {
            let projected = session
                .handshake_bytes
                .checked_add(input.len() as u64)
                .ok_or(KU_TLS_STATUS_LIMIT_EXCEEDED)?;
            if projected > KU_TLS_MAX_HANDSHAKE_BYTES {
                session.failed = true;
                return Err(KU_TLS_STATUS_LIMIT_EXCEEDED);
            }
        }

        let mut cursor = io::Cursor::new(input);
        let consumed =
            session
                .connection
                .read_tls(&mut cursor)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::WouldBlock => KU_TLS_STATUS_WOULD_BLOCK,
                    _ => KU_TLS_STATUS_IO_ERROR,
                })?;
        if consumed == 0 {
            session.failed = true;
            return Err(KU_TLS_STATUS_IO_ERROR);
        }
        if was_handshaking {
            session.handshake_bytes += consumed as u64;
        }
        session.needs_process = consumed != 0;
        // SAFETY: The output pointer was checked above.
        unsafe { ptr::write(out_consumed, consumed) };
        Ok(())
    })
}

#[no_mangle]
/// Processes the ciphertext supplied by the preceding feed call.
///
/// # Safety
/// The session must be live and exclusively accessed by this call.
pub unsafe extern "C" fn ku_tls_v1_client_process(session: *mut KuTlsClientSession) -> u32 {
    ffi_status(|| {
        let session = unsafe { session_mut(session)? };
        if session.failed {
            return Err(KU_TLS_STATUS_SESSION_FAILED);
        }
        if !session.needs_process {
            return Err(KU_TLS_STATUS_WOULD_BLOCK);
        }
        if session.connection.is_handshaking() {
            let next = session
                .handshake_iterations
                .checked_add(1)
                .ok_or(KU_TLS_STATUS_LIMIT_EXCEEDED)?;
            if next > KU_TLS_MAX_HANDSHAKE_ITERATIONS {
                session.failed = true;
                return Err(KU_TLS_STATUS_LIMIT_EXCEEDED);
            }
            session.handshake_iterations = next;
        }
        session.needs_process = false;
        match session.connection.process_new_packets() {
            Ok(state) => {
                session.peer_closed |= state.peer_has_closed();
                Ok(())
            }
            Err(_) => {
                session.failed = true;
                Err(KU_TLS_STATUS_TLS_ERROR)
            }
        }
    })
}

struct SliceWriter<'a> {
    output: &'a mut [u8],
    used: usize,
}

impl Write for SliceWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let available = self.output.len().saturating_sub(self.used);
        let count = available.min(data.len());
        self.output[self.used..self.used + count].copy_from_slice(&data[..count]);
        self.used += count;
        Ok(count)
    }

    fn write_vectored(&mut self, buffers: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let start = self.used;
        for buffer in buffers {
            if self.used == self.output.len() {
                break;
            }
            let _ = self.write(buffer)?;
        }
        Ok(self.used - start)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[no_mangle]
/// Drains pending ciphertext without performing socket I/O.
///
/// # Safety
/// The session must be live and exclusively accessed. Output buffers and the
/// count output must be writable for their stated sizes and non-aliasing.
pub unsafe extern "C" fn ku_tls_v1_client_drain_ciphertext(
    session: *mut KuTlsClientSession,
    output: *mut u8,
    output_capacity: usize,
    out_written: *mut usize,
) -> u32 {
    ffi_status(|| {
        if out_written.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The output pointer is non-null and owned by the caller.
        unsafe { ptr::write(out_written, 0) };
        let output = unsafe { bounded_output(output, output_capacity, KU_TLS_MAX_IO_BYTES)? };
        let session = unsafe { session_mut(session)? };
        if output.is_empty() {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }
        let mut writer = SliceWriter { output, used: 0 };
        session
            .connection
            .write_tls(&mut writer)
            .map_err(|_| KU_TLS_STATUS_IO_ERROR)?;
        if writer.used == 0 && session.connection.wants_write() {
            return Err(KU_TLS_STATUS_IO_ERROR);
        }
        // SAFETY: The output pointer was checked above.
        unsafe { ptr::write(out_written, writer.used) };
        Ok(())
    })
}

#[no_mangle]
/// Queues bounded plaintext for encryption.
///
/// # Safety
/// The session must be live and exclusively accessed. Plaintext must be
/// readable for its stated length and `out_written` must be writable and
/// non-aliasing.
pub unsafe extern "C" fn ku_tls_v1_client_write_plaintext(
    session: *mut KuTlsClientSession,
    plaintext: *const u8,
    plaintext_len: usize,
    out_written: *mut usize,
) -> u32 {
    ffi_status(|| {
        if out_written.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The output pointer is non-null and owned by the caller.
        unsafe { ptr::write(out_written, 0) };
        let input = unsafe { bounded_input(plaintext, plaintext_len, KU_TLS_MAX_IO_BYTES)? };
        let session = unsafe { session_mut(session)? };
        if session.failed {
            return Err(KU_TLS_STATUS_SESSION_FAILED);
        }
        if session.transport_eof || session.local_close_sent {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }
        if input.is_empty() {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }
        let written =
            session
                .connection
                .writer()
                .write(input)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::WouldBlock => KU_TLS_STATUS_WOULD_BLOCK,
                    _ => KU_TLS_STATUS_IO_ERROR,
                })?;
        // SAFETY: The output pointer was checked above.
        unsafe { ptr::write(out_written, written) };
        if written == 0 {
            Err(KU_TLS_STATUS_WOULD_BLOCK)
        } else {
            Ok(())
        }
    })
}

#[no_mangle]
/// Reads authenticated plaintext already buffered by rustls.
///
/// # Safety
/// The session must be live and exclusively accessed. Output buffers and the
/// count output must be writable for their stated sizes and non-aliasing.
pub unsafe extern "C" fn ku_tls_v1_client_read_plaintext(
    session: *mut KuTlsClientSession,
    output: *mut u8,
    output_capacity: usize,
    out_read: *mut usize,
) -> u32 {
    ffi_status(|| {
        if out_read.is_null() {
            return Err(KU_TLS_STATUS_NULL_POINTER);
        }
        // SAFETY: The output pointer is non-null and owned by the caller.
        unsafe { ptr::write(out_read, 0) };
        let output = unsafe { bounded_output(output, output_capacity, KU_TLS_MAX_IO_BYTES)? };
        let session = unsafe { session_mut(session)? };
        if session.failed {
            return Err(KU_TLS_STATUS_SESSION_FAILED);
        }
        if output.is_empty() {
            return Err(KU_TLS_STATUS_INVALID_ARGUMENT);
        }
        match session.connection.reader().read(output) {
            Ok(read) => {
                // SAFETY: The output pointer was checked above.
                unsafe { ptr::write(out_read, read) };
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(KU_TLS_STATUS_WOULD_BLOCK)
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                session.failed = true;
                Err(KU_TLS_STATUS_TRUNCATED)
            }
            Err(_) => Err(KU_TLS_STATUS_IO_ERROR),
        }
    })
}

#[no_mangle]
/// Queues an idempotent local `close_notify` alert.
///
/// # Safety
/// The session must be live and exclusively accessed by this call.
pub unsafe extern "C" fn ku_tls_v1_client_send_close_notify(
    session: *mut KuTlsClientSession,
) -> u32 {
    ffi_status(|| {
        let session = unsafe { session_mut(session)? };
        if session.failed {
            return Err(KU_TLS_STATUS_SESSION_FAILED);
        }
        if !session.local_close_sent {
            session.connection.send_close_notify();
            session.local_close_sent = true;
        }
        Ok(())
    })
}

#[no_mangle]
/// Reports transport EOF and rejects unauthenticated truncation.
///
/// # Safety
/// The session must be live and exclusively accessed by this call.
pub unsafe extern "C" fn ku_tls_v1_client_notify_eof(session: *mut KuTlsClientSession) -> u32 {
    ffi_status(|| {
        let session = unsafe { session_mut(session)? };
        if session.failed {
            return Err(KU_TLS_STATUS_SESSION_FAILED);
        }
        if session.needs_process {
            return Err(KU_TLS_STATUS_WOULD_BLOCK);
        }
        session.transport_eof = true;
        if session.peer_closed {
            Ok(())
        } else {
            session.failed = true;
            Err(KU_TLS_STATUS_TRUNCATED)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_webpki_session() -> (*mut KuTlsConfig, *mut KuTlsClientSession) {
        // SAFETY: All output slots and handles are live for the calls below.
        unsafe {
            let mut config = ptr::null_mut();
            assert_eq!(
                ku_tls_v1_config_new(KU_TLS_ROOTS_WEBPKI, ptr::null(), 0, &mut config),
                KU_TLS_STATUS_OK
            );
            let mut session = ptr::null_mut();
            assert_eq!(
                ku_tls_v1_client_new(
                    config,
                    b"localhost".as_ptr(),
                    b"localhost".len(),
                    &mut session,
                ),
                KU_TLS_STATUS_OK
            );
            (config, session)
        }
    }

    fn drain_initial_client_hello(session: *mut KuTlsClientSession) {
        // SAFETY: The caller supplies a live, exclusively held session.
        unsafe {
            let mut output = [0u8; 4096];
            for _ in 0..4096 {
                let mut wants_write = 0;
                assert_eq!(
                    ku_tls_v1_client_wants_write(session, &mut wants_write),
                    KU_TLS_STATUS_OK
                );
                if wants_write == 0 {
                    return;
                }
                let mut written = 0;
                assert_eq!(
                    ku_tls_v1_client_drain_ciphertext(
                        session,
                        output.as_mut_ptr(),
                        output.len(),
                        &mut written,
                    ),
                    KU_TLS_STATUS_OK
                );
                assert_ne!(written, 0);
            }
            panic!("bounded ClientHello drain did not converge");
        }
    }

    #[test]
    fn panic_fence_maps_unwind_to_stable_status() {
        assert_eq!(
            ffi_status(|| -> Result<(), u32> { panic!("test panic") }),
            KU_TLS_STATUS_PANIC
        );
    }

    #[test]
    fn opaque_handle_state_is_safe_to_move_between_serialized_threads() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        assert_send::<KuTlsConfig>();
        assert_sync::<KuTlsConfig>();
        assert_send::<KuTlsClientSession>();
    }

    #[test]
    fn config_and_session_drop_release_their_arc_references() {
        // SAFETY: This test uniquely owns both handles and consumes each once.
        unsafe {
            let (config, session) = new_webpki_session();
            let retained = Arc::clone(&(*config).config);
            assert_eq!(Arc::strong_count(&retained), 3);
            assert_eq!(ku_tls_v1_config_drop(config), KU_TLS_STATUS_OK);
            assert_eq!(Arc::strong_count(&retained), 2);
            assert_eq!(ku_tls_v1_client_drop(session), KU_TLS_STATUS_OK);
            assert_eq!(Arc::strong_count(&retained), 1);
        }
    }

    #[test]
    fn handshake_byte_and_iteration_limits_fail_the_session_closed() {
        // SAFETY: This test uniquely owns all handles and buffers used below.
        unsafe {
            let (config, session) = new_webpki_session();
            drain_initial_client_hello(session);
            (*session).handshake_bytes = KU_TLS_MAX_HANDSHAKE_BYTES;
            let mut consumed = 0;
            assert_eq!(
                ku_tls_v1_client_feed_ciphertext(session, b"x".as_ptr(), 1, &mut consumed),
                KU_TLS_STATUS_LIMIT_EXCEEDED
            );
            assert_eq!(consumed, 0);
            assert_eq!(
                ku_tls_v1_client_process(session),
                KU_TLS_STATUS_SESSION_FAILED
            );
            assert_eq!(ku_tls_v1_client_drop(session), KU_TLS_STATUS_OK);
            assert_eq!(ku_tls_v1_config_drop(config), KU_TLS_STATUS_OK);

            let (config, session) = new_webpki_session();
            drain_initial_client_hello(session);
            (*session).handshake_iterations = KU_TLS_MAX_HANDSHAKE_ITERATIONS;
            assert_eq!(
                ku_tls_v1_client_feed_ciphertext(session, b"x".as_ptr(), 1, &mut consumed),
                KU_TLS_STATUS_OK
            );
            assert_eq!(consumed, 1);
            assert_eq!(
                ku_tls_v1_client_process(session),
                KU_TLS_STATUS_LIMIT_EXCEEDED
            );
            assert_eq!(
                ku_tls_v1_client_process(session),
                KU_TLS_STATUS_SESSION_FAILED
            );
            assert_eq!(ku_tls_v1_client_drop(session), KU_TLS_STATUS_OK);
            assert_eq!(ku_tls_v1_config_drop(config), KU_TLS_STATUS_OK);
        }
    }
}
