use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, TrySendError},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use ed25519_dalek::{Signer, SigningKey};
use rustls::pki_types::{
    pem::{PemObject, SectionKind},
    CertificateDer, PrivateKeyDer,
};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub use crate::registry_admin::REGISTRY_CREDENTIALS_FILE_ENV;

use crate::{
    error::{KuError, KuResult},
    package::{
        parse_package_version, parse_registry_index, parse_version_requirement,
        validate_package_name, validate_registry_package_dependencies,
        validate_registry_publish_archive, validate_sha256_checksum, PackageDependency,
        MAX_PUBLISH_TOKEN_BYTES, MAX_REGISTRY_DEPENDENCIES, MAX_REGISTRY_DOWNLOAD_BYTES,
        MAX_REGISTRY_INDEX_BYTES, MAX_REGISTRY_INDEX_VERSIONS, MAX_REGISTRY_LINE_BYTES,
        PACKAGE_CONTENT_TYPE,
    },
    registry_admin::{self, parse_credential_records},
    span::Span,
};

pub const REGISTRY_BIND_ENV: &str = "KU_REGISTRY_BIND";
pub const REGISTRY_DATA_DIR_ENV: &str = "KU_REGISTRY_DATA_DIR";
pub const REGISTRY_SIGNING_KEY_FILE_ENV: &str = "KU_REGISTRY_SIGNING_KEY_FILE";
pub const REGISTRY_TLS_CERT_FILE_ENV: &str = "KU_REGISTRY_TLS_CERT_FILE";
pub const REGISTRY_TLS_KEY_FILE_ENV: &str = "KU_REGISTRY_TLS_KEY_FILE";
pub const REGISTRY_WORKERS_ENV: &str = "KU_REGISTRY_WORKERS";
pub const REGISTRY_QUEUE_CAPACITY_ENV: &str = "KU_REGISTRY_QUEUE_CAPACITY";
pub const REGISTRY_REQUEST_TIMEOUT_MS_ENV: &str = "KU_REGISTRY_REQUEST_TIMEOUT_MS";

const DEFAULT_BIND: &str = "127.0.0.1:8443";
const DEFAULT_WORKERS: usize = 16;
const DEFAULT_QUEUE_CAPACITY: usize = 32;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 15_000;
const MAX_WORKERS: usize = 64;
const MAX_QUEUE_CAPACITY: usize = 256;
const MAX_REQUEST_TIMEOUT_MS: u64 = 60_000;
const ACCEPT_POLL_MS: u64 = 5;
const LOCK_POLL_MS: u64 = 10;
const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
const MAX_REQUEST_LINE_BYTES: usize = 2 * 1024;
const MAX_REQUEST_HEADERS: usize = 64;
const MAX_REQUESTS_PER_CONNECTION: usize = 8;
const REGISTRY_TLS_INPUT_BUFFER_BYTES: usize = 8 * 1024;
const MAX_BUFFERED_REJECT_BODY_BYTES: usize = 8 * 1024;
const KEEP_ALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_SECRET_FILE_BYTES: u64 = 16 * 1024;
const MAX_CREDENTIALS_FILE_BYTES: u64 = registry_admin::MAX_CREDENTIAL_FILE_BYTES;
const MAX_TLS_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ENTRY_METADATA_BYTES: u64 = 256 * 1024;
const MAX_INDEX_ITEMS: usize = 65_536;
const MAX_ACTIVE_MUTATIONS: usize = 4;
const MAX_ARTIFACT_VERIFICATION_CACHE_ENTRIES: usize = 1_024;
const MAX_INDEX_GENERATION_CLEANUP: usize = 16;
const MAX_REGISTRY_PACKAGES: usize = 4_096;
const STARTUP_AUDIT_TIMEOUT: Duration = Duration::from_secs(300);
const STARTUP_PACKAGE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const STAGING_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_STAGING_SCAN_ENTRIES: usize = 4_096;
const MAX_STAGING_TREE_ENTRIES: usize = 5_000;
const MAX_STAGING_TREE_BYTES: u64 = 160_000_000;
const INDEX_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
const ARTIFACT_CONTENT_TYPE: &str = PACKAGE_CONTENT_TYPE;
const ENTRY_METADATA_FILE: &str = "entry.toml";
const VERSIONS_DIR: &str = "versions";
const INDEX_GENERATIONS_DIR: &str = ".indexes";
const INDEX_CURRENT_FILE: &str = ".index.current";
const PUBLISH_PENDING_FILE: &str = ".publish.pending";
const LOCKS_DIR: &str = ".locks";
const STAGING_DIR: &str = ".staging";
const YANKS_DIR: &str = ".yanks";
const PACKAGES_DIR: &str = "packages";
const INSTANCE_LOCK_FILE: &str = ".instance.lock";

static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct RegistryServerConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub credentials_file: PathBuf,
    pub signing_key_file: PathBuf,
    pub tls_cert_file: PathBuf,
    pub tls_key_file: PathBuf,
    pub workers: usize,
    pub queue_capacity: usize,
    pub request_timeout: Duration,
}

impl RegistryServerConfig {
    pub fn from_env() -> KuResult<Self> {
        let bind = env::var(REGISTRY_BIND_ENV)
            .unwrap_or_else(|_| DEFAULT_BIND.to_string())
            .parse::<SocketAddr>()
            .map_err(|_| {
                server_config_error(
                    "invalid_bind",
                    format!(
                        "{REGISTRY_BIND_ENV} must be an IP socket address such as {DEFAULT_BIND}"
                    ),
                )
            })?;
        let data_dir = required_path_env(REGISTRY_DATA_DIR_ENV)?;
        let credentials_file = required_path_env(REGISTRY_CREDENTIALS_FILE_ENV)?;
        let signing_key_file = required_path_env(REGISTRY_SIGNING_KEY_FILE_ENV)?;
        let tls_cert_file = required_path_env(REGISTRY_TLS_CERT_FILE_ENV)?;
        let tls_key_file = required_path_env(REGISTRY_TLS_KEY_FILE_ENV)?;
        let workers = bounded_usize_env(REGISTRY_WORKERS_ENV, DEFAULT_WORKERS, 1, MAX_WORKERS)?;
        let queue_capacity = bounded_usize_env(
            REGISTRY_QUEUE_CAPACITY_ENV,
            DEFAULT_QUEUE_CAPACITY,
            1,
            MAX_QUEUE_CAPACITY,
        )?;
        let timeout_ms = bounded_u64_env(
            REGISTRY_REQUEST_TIMEOUT_MS_ENV,
            DEFAULT_REQUEST_TIMEOUT_MS,
            100,
            MAX_REQUEST_TIMEOUT_MS,
        )?;
        Ok(Self {
            bind,
            data_dir,
            credentials_file,
            signing_key_file,
            tls_cert_file,
            tls_key_file,
            workers,
            queue_capacity,
            request_timeout: Duration::from_millis(timeout_ms),
        })
    }
}

pub struct RegistryServer {
    listener: TcpListener,
    state: Arc<RegistryState>,
    workers: usize,
    queue_capacity: usize,
    #[cfg(test)]
    accepted_connections: Arc<AtomicUsize>,
}

impl RegistryServer {
    pub fn bind(config: RegistryServerConfig) -> KuResult<Self> {
        validate_server_config(&config)?;
        let storage = RegistryStorage::open(&config.data_dir)?;
        let instance_lock = RegistryInstanceLock::acquire(&storage.root)?;
        cleanup_stale_staging(&storage.staging)?;
        let credentials = read_credentials(&config.credentials_file)?;
        let signing_key = read_signing_key(&config.signing_key_file)?;
        let tls = load_tls_config(&config.tls_cert_file, &config.tls_key_file)?;
        let state = Arc::new(RegistryState {
            storage,
            credentials,
            signing_key,
            tls: Arc::new(tls),
            request_timeout: config.request_timeout,
            mutation_admission: MutationAdmission::new(),
            artifact_verifications: Arc::new(ArtifactVerificationCache::new(
                MAX_ARTIFACT_VERIFICATION_CACHE_ENTRIES,
            )),
            _instance_lock: instance_lock,
        });
        audit_registry_storage(&state)?;
        let listener = TcpListener::bind(config.bind).map_err(|err| {
            server_error(
                "registry_bind_failed",
                format!("failed to bind registry listener '{}': {err}", config.bind),
            )
        })?;
        listener.set_nonblocking(true).map_err(|err| {
            server_error(
                "registry_bind_failed",
                format!("failed to configure registry listener: {err}"),
            )
        })?;
        Ok(Self {
            listener,
            state,
            workers: config.workers,
            queue_capacity: config.queue_capacity,
            #[cfg(test)]
            accepted_connections: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn local_addr(&self) -> KuResult<SocketAddr> {
        self.listener.local_addr().map_err(|err| {
            server_error(
                "registry_listener_failed",
                format!("failed to inspect registry listener address: {err}"),
            )
        })
    }

    pub fn public_key(&self) -> String {
        format!(
            "ed25519-{}",
            encode_hex(self.state.signing_key.verifying_key().as_bytes())
        )
    }

    #[cfg(test)]
    fn accepted_connection_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.accepted_connections)
    }

    pub fn serve(self) -> KuResult<()> {
        self.serve_until(Arc::new(AtomicBool::new(false)))
    }

    pub fn serve_until(self, shutdown: Arc<AtomicBool>) -> KuResult<()> {
        let (sender, receiver) = mpsc::sync_channel::<QueuedConnection>(self.queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut handles = Vec::with_capacity(self.workers);
        for _ in 0..self.workers {
            let receiver = Arc::clone(&receiver);
            let state = Arc::clone(&self.state);
            let shutdown = Arc::clone(&shutdown);
            handles.push(thread::spawn(move || loop {
                let queued = {
                    let receiver = match receiver.lock() {
                        Ok(receiver) => receiver,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    match receiver.recv() {
                        Ok(queued) => queued,
                        Err(_) => return,
                    }
                };
                if shutdown.load(Ordering::Acquire) {
                    drop(queued);
                    continue;
                }
                if Instant::now() >= queued.deadline {
                    continue;
                }
                handle_connection(queued.stream, &state, queued.deadline);
            }));
        }

        let mut accept_error = None;
        while !shutdown.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    #[cfg(test)]
                    self.accepted_connections.fetch_add(1, Ordering::Relaxed);
                    if let Err(err) = stream.set_nonblocking(false) {
                        eprintln!("ku-registry could not configure an accepted socket: {err}");
                        drop(stream);
                        continue;
                    }
                    match sender.try_send(QueuedConnection {
                        stream,
                        deadline: Instant::now() + self.state.request_timeout,
                    }) {
                        Ok(()) => {}
                        Err(TrySendError::Full(queued)) => drop(queued),
                        Err(TrySendError::Disconnected(_)) => {
                            accept_error = Some(server_error(
                                "registry_worker_failed",
                                "all registry workers stopped unexpectedly",
                            ));
                            break;
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(ACCEPT_POLL_MS));
                }
                Err(err) => {
                    accept_error = Some(server_error(
                        "registry_accept_failed",
                        format!("registry accept failed: {err}"),
                    ));
                    break;
                }
            }
        }
        drop(sender);
        for handle in handles {
            if handle.join().is_err() && accept_error.is_none() {
                accept_error = Some(server_error(
                    "registry_worker_failed",
                    "a registry worker terminated unexpectedly",
                ));
            }
        }
        match accept_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

struct RegistryState {
    storage: RegistryStorage,
    credentials: Vec<PublishCredential>,
    signing_key: SigningKey,
    tls: Arc<ServerConfig>,
    request_timeout: Duration,
    mutation_admission: MutationAdmission,
    artifact_verifications: Arc<ArtifactVerificationCache>,
    _instance_lock: RegistryInstanceLock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArtifactFingerprint {
    identity_high: u64,
    identity_low: u64,
    length: u64,
    modified: Option<SystemTime>,
    changed_seconds: i64,
    changed_nanos: i64,
}

enum ArtifactVerificationEntry {
    Verifying {
        fingerprint: ArtifactFingerprint,
    },
    Verified {
        fingerprint: ArtifactFingerprint,
        used_at: u64,
    },
    Failed {
        fingerprint: ArtifactFingerprint,
        message: String,
        used_at: u64,
    },
}

struct ArtifactVerificationCacheInner {
    entries: HashMap<PathBuf, ArtifactVerificationEntry>,
    clock: u64,
}

struct ArtifactVerificationCache {
    capacity: usize,
    inner: Mutex<ArtifactVerificationCacheInner>,
    changed: Condvar,
    #[cfg(test)]
    hash_count: AtomicUsize,
    #[cfg(test)]
    hash_delay_ms: AtomicU64,
    #[cfg(test)]
    fail_next_verification: AtomicBool,
}

impl ArtifactVerificationCache {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            inner: Mutex::new(ArtifactVerificationCacheInner {
                entries: HashMap::with_capacity(capacity.min(64)),
                clock: 0,
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            hash_count: AtomicUsize::new(0),
            #[cfg(test)]
            hash_delay_ms: AtomicU64::new(0),
            #[cfg(test)]
            fail_next_verification: AtomicBool::new(false),
        }
    }
}

struct QueuedConnection {
    stream: TcpStream,
    deadline: Instant,
}

#[derive(Clone)]
struct RegistryStorage {
    root: PathBuf,
    packages: PathBuf,
    locks: PathBuf,
    staging: PathBuf,
}

impl RegistryStorage {
    fn open(root: &Path) -> KuResult<Self> {
        ensure_real_directory(root)?;
        let root = fs::canonicalize(root).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to resolve registry data directory: {err}"),
            )
        })?;
        let packages = ensure_real_child_directory(&root, PACKAGES_DIR)?;
        let locks = ensure_real_child_directory(&root, LOCKS_DIR)?;
        let staging = ensure_real_child_directory(&root, STAGING_DIR)?;
        Ok(Self {
            root,
            packages,
            locks,
            staging,
        })
    }
}

struct RegistryInstanceLock {
    file: fs::File,
}

impl RegistryInstanceLock {
    fn acquire(root: &Path) -> KuResult<Self> {
        let path = root.join(INSTANCE_LOCK_FILE);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(server_error(
                    "unsafe_registry_storage",
                    "registry instance lock must be a real regular file",
                ));
            }
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                server_error(
                    "registry_instance_lock_failed",
                    format!("failed to open registry instance lock: {err}"),
                )
            })?;
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            server_error(
                "registry_instance_lock_failed",
                format!("failed to inspect registry instance lock: {err}"),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(server_error(
                "unsafe_registry_storage",
                "registry instance lock must remain a real regular file",
            ));
        }
        let canonical = fs::canonicalize(&path).map_err(|err| {
            server_error(
                "registry_instance_lock_failed",
                format!("failed to resolve registry instance lock: {err}"),
            )
        })?;
        if canonical.parent() != Some(root) {
            return Err(server_error(
                "unsafe_registry_storage",
                "registry instance lock escaped its data directory",
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(fs::TryLockError::WouldBlock) => Err(server_error(
                "registry_instance_locked",
                "another ku-registry instance already owns this data directory",
            )),
            Err(fs::TryLockError::Error(err)) => Err(server_error(
                "registry_instance_lock_failed",
                format!("failed to acquire registry instance lock: {err}"),
            )),
        }
    }
}

impl Drop for RegistryInstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn audit_registry_storage(state: &RegistryState) -> KuResult<()> {
    let deadline = Instant::now() + STARTUP_AUDIT_TIMEOUT;
    let mut packages_seen = 0usize;
    let entries = fs::read_dir(&state.storage.packages).map_err(|err| {
        server_error(
            "registry_storage_failed",
            format!("failed to scan registry packages during startup: {err}"),
        )
    })?;
    for entry in entries {
        if Instant::now() >= deadline {
            return Err(server_error(
                "registry_startup_audit_timeout",
                "registry startup audit exceeded its 300 second hard deadline",
            ));
        }
        packages_seen += 1;
        if packages_seen > MAX_REGISTRY_PACKAGES {
            return Err(server_error(
                "registry_storage_limit",
                "registry package count exceeds the v1 storage limit",
            ));
        }
        let entry = entry.map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to read registry package during startup: {err}"),
            )
        })?;
        let package_name = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                server_error(
                    "unsafe_registry_storage",
                    "registry package storage name is not UTF-8",
                )
            })?;
        validate_package_name(&package_name, Span::default()).map_err(|_| {
            server_error(
                "unsafe_registry_storage",
                "registry package storage contains an invalid package name",
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to inspect registry package during startup: {err}"),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(server_error(
                "unsafe_registry_storage",
                "registry package storage must contain only real package directories",
            ));
        }
        let package_root = fs::canonicalize(entry.path()).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to resolve registry package during startup: {err}"),
            )
        })?;
        if package_root.parent() != Some(state.storage.packages.as_path()) {
            return Err(server_error(
                "unsafe_registry_storage",
                "registry package storage escaped its data directory",
            ));
        }
        let _lock = PackagePublishLock::acquire(&state.storage.locks, &package_name, deadline)
            .map_err(|failure| startup_request_failure("acquire package audit lock", failure))?;
        cleanup_package_temporary_files(&package_root)
            .map_err(|failure| startup_request_failure("clean package staging", failure))?;
        let versions_root = ensure_request_child_directory(&package_root, VERSIONS_DIR)
            .map_err(|failure| startup_request_failure("open package versions", failure))?;
        ensure_request_child_directory(&package_root, YANKS_DIR)
            .map_err(|failure| startup_request_failure("open package yanks", failure))?;
        ensure_request_child_directory(&package_root, INDEX_GENERATIONS_DIR)
            .map_err(|failure| startup_request_failure("open signed indexes", failure))?;
        let yanked = load_yanked_versions(&package_root, &versions_root, deadline)
            .map_err(|failure| startup_request_failure("audit package yanks", failure))?;
        let cached = if !pending_marker_exists(&package_root)
            .map_err(|failure| startup_request_failure("inspect recovery marker", failure))?
        {
            load_cached_signed_index(&package_root, &state.signing_key)
                .ok()
                .flatten()
        } else {
            None
        };
        if let Some(cached) = cached {
            let visible = entries_from_signed_index(&cached, &package_name)
                .map_err(|failure| startup_request_failure("audit signed index yanks", failure))?;
            let exposes_yanked = visible.iter().any(|entry| yanked.contains(&entry.version));
            if exposes_yanked {
                discard_broken_cached_index(&package_root).map_err(|failure| {
                    startup_request_failure("discard stale pre-yank index", failure)
                })?;
            } else {
                audit_clean_package_index_storage(
                    &versions_root,
                    &package_name,
                    &visible,
                    &yanked,
                    deadline,
                )
                .map_err(|failure| {
                    startup_request_failure(
                        "verify signed index against physical package versions",
                        failure,
                    )
                })?;
                if let Err(failure) =
                    cleanup_old_index_generations(&package_root, &cached.generation)
                {
                    log_maintenance_failure(
                        "clean stale signed index generations at startup",
                        &failure,
                    );
                }
                continue;
            }
        }
        let recovery_deadline = (Instant::now() + STARTUP_PACKAGE_RECOVERY_TIMEOUT).min(deadline);
        ensure_signed_index_locked(state, &package_root, &package_name, recovery_deadline)
            .map_err(|failure| startup_request_failure("recover package index", failure))?;
    }
    Ok(())
}

fn startup_request_failure(action: &str, failure: RequestFailure) -> KuError {
    server_error(
        "registry_startup_audit_failed",
        format!(
            "failed to {action}: {}",
            failure
                .internal
                .unwrap_or(failure.public_message.to_string())
                .trim()
        ),
    )
}

struct RequestHead {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    content_length: u64,
    keep_alive: bool,
}

enum ResponseBody {
    Empty,
    Bytes(Vec<u8>),
    File(fs::File, u64),
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: ResponseBody,
    artifact_checksum: Option<String>,
}

impl HttpResponse {
    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: ResponseBody::Empty,
            artifact_checksum: None,
        }
    }

    fn message(status: u16, message: &'static str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: ResponseBody::Bytes(message.as_bytes().to_vec()),
            artifact_checksum: None,
        }
    }

    fn bytes(status: u16, content_type: &'static str, bytes: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body: ResponseBody::Bytes(bytes),
            artifact_checksum: None,
        }
    }
}

#[derive(Debug)]
struct RequestFailure {
    status: u16,
    public_message: &'static str,
    internal: Option<String>,
}

impl RequestFailure {
    fn public(status: u16, public_message: &'static str) -> Self {
        Self {
            status,
            public_message,
            internal: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            public_message: "internal registry error\n",
            internal: Some(message.into()),
        }
    }

    fn response(&self) -> HttpResponse {
        HttpResponse::message(self.status, self.public_message)
    }
}

fn handle_connection(stream: TcpStream, state: &RegistryState, deadline: Instant) {
    let connection = match ServerConnection::new(Arc::clone(&state.tls)) {
        Ok(connection) => connection,
        Err(_) => return,
    };
    let stream = DeadlineTcpStream::new(stream, deadline);
    let tls = StreamOwned::new(connection, stream);
    let mut reader = BufReader::with_capacity(REGISTRY_TLS_INPUT_BUFFER_BYTES, tls);
    for request_number in 1..=MAX_REQUESTS_PER_CONNECTION {
        if Instant::now() >= deadline {
            break;
        }
        if request_number > 1 {
            let idle_deadline = (Instant::now() + KEEP_ALIVE_IDLE_TIMEOUT).min(deadline);
            reader.get_mut().sock.set_deadline(idle_deadline);
        }
        match reader.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(_) => {}
        }
        let request = read_request_head(&mut reader);
        reader.get_mut().sock.set_deadline(deadline);
        let request = match request {
            Ok(request) => request,
            Err(failure) => {
                log_request_failure(&failure);
                let _ = write_http_response(reader.get_mut(), failure.response(), deadline, 0);
                break;
            }
        };
        let request_keep_alive = request.keep_alive;
        let response = match dispatch_request(state, request, &mut reader, deadline) {
            Ok(response) => response,
            Err(failure) => {
                log_request_failure(&failure);
                let _ = write_http_response(reader.get_mut(), failure.response(), deadline, 0);
                break;
            }
        };
        let remaining_requests = if request_keep_alive
            && request_number < MAX_REQUESTS_PER_CONNECTION
            && Instant::now() < deadline
        {
            MAX_REQUESTS_PER_CONNECTION - request_number
        } else {
            0
        };
        if write_http_response(reader.get_mut(), response, deadline, remaining_requests).is_err()
            || remaining_requests == 0
        {
            break;
        }
    }
    reader.get_mut().sock.set_deadline(deadline);
    reader.get_mut().conn.send_close_notify();
    let _ = reader.get_mut().flush();
}

fn log_request_failure(failure: &RequestFailure) {
    if let Some(internal) = &failure.internal {
        eprintln!("ku-registry request failed: {internal}");
    }
}

struct DeadlineTcpStream {
    inner: TcpStream,
    deadline: Instant,
}

impl DeadlineTcpStream {
    fn new(inner: TcpStream, deadline: Instant) -> Self {
        Self { inner, deadline }
    }

    fn remaining(&self) -> io::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "registry request deadline exceeded",
            ))
        } else {
            Ok(remaining)
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }
}

impl Read for DeadlineTcpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.inner.set_read_timeout(Some(remaining))?;
        self.inner.read(buffer)
    }
}

impl Write for DeadlineTcpStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.inner.set_write_timeout(Some(remaining))?;
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        let remaining = self.remaining()?;
        self.inner.set_write_timeout(Some(remaining))?;
        self.inner.flush()
    }
}

fn dispatch_request(
    state: &RegistryState,
    request: RequestHead,
    reader: &mut BufReader<StreamOwned<ServerConnection, DeadlineTcpStream>>,
    deadline: Instant,
) -> Result<HttpResponse, RequestFailure> {
    ensure_request_deadline(deadline)?;
    if request.method == "PUT" {
        if let Some((name, version)) = parse_yank_path(&request.path) {
            if let Err(failure) = validate_yank_request_head(state, &request, &name, &version) {
                discard_complete_buffered_rejected_body(reader, request.content_length);
                return Err(failure);
            }
            let mutation_slot = match state.mutation_admission.try_acquire(&name) {
                Ok(slot) => slot,
                Err(failure) => {
                    discard_complete_buffered_rejected_body(reader, request.content_length);
                    return Err(failure);
                }
            };
            return yank_request(state, &name, &version, mutation_slot, deadline);
        }
        let Some((name, version)) = parse_publish_path(&request.path) else {
            return Err(RequestFailure::public(404, "not found\n"));
        };
        let expected_checksum =
            match validate_publish_request_head(state, &request, &name, &version) {
                Ok(checksum) => checksum,
                Err(failure) => {
                    discard_complete_buffered_rejected_body(reader, request.content_length);
                    return Err(failure);
                }
            };
        let publish_slot = match state.mutation_admission.try_acquire(&name) {
            Ok(slot) => slot,
            Err(failure) => {
                discard_complete_buffered_rejected_body(reader, request.content_length);
                return Err(failure);
            }
        };
        return publish_request(
            state,
            &request,
            reader,
            ValidatedPublish {
                package_name: &name,
                version: &version,
                expected_checksum,
                _slot: publish_slot,
            },
            deadline,
        );
    }
    if request.method != "GET" {
        return Err(RequestFailure::public(405, "method not allowed\n"));
    }
    if request.content_length != 0 {
        return Err(RequestFailure::public(
            400,
            "GET request body is not allowed\n",
        ));
    }
    if let Some((name, signature)) = parse_index_path(&request.path) {
        let index = load_or_recover_signed_index(state, &name, deadline)?;
        let Some(index) = index else {
            return Err(RequestFailure::public(404, "not found\n"));
        };
        return Ok(if signature {
            HttpResponse::bytes(200, INDEX_CONTENT_TYPE, index.signature)
        } else {
            HttpResponse::bytes(200, INDEX_CONTENT_TYPE, index.bytes)
        });
    }
    if let Some(reference) = parse_artifact_path(&request.path) {
        return open_artifact_response(state, &reference, deadline);
    }
    Err(RequestFailure::public(404, "not found\n"))
}

/// Discard a rejected PUT body only when the complete, small body was already
/// decrypted by the header read. This performs no socket read and therefore
/// cannot turn an unauthenticated slow upload into worker wait time. Consuming
/// already-buffered bytes avoids a TCP reset on Windows, allowing well-behaved
/// clients to receive the structured rejection reliably.
fn discard_complete_buffered_rejected_body(
    reader: &mut BufReader<StreamOwned<ServerConnection, DeadlineTcpStream>>,
    content_length: u64,
) {
    let Ok(content_length) = usize::try_from(content_length) else {
        return;
    };
    if content_length > MAX_BUFFERED_REJECT_BODY_BYTES || reader.buffer().len() < content_length {
        return;
    }
    reader.consume(content_length);
}

fn read_request_head(reader: &mut impl BufRead) -> Result<RequestHead, RequestFailure> {
    let mut total = 0usize;
    let request_line = read_crlf_line(reader, MAX_REQUEST_LINE_BYTES, &mut total)?;
    let request_line = std::str::from_utf8(&request_line)
        .map_err(|_| RequestFailure::public(400, "invalid request\n"))?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(RequestFailure::public(400, "invalid request line\n"));
    };
    if !matches!(method, "GET" | "PUT") || version != "HTTP/1.1" || !valid_origin_form_path(path) {
        return Err(RequestFailure::public(400, "invalid request line\n"));
    }
    let mut headers = HashMap::<String, String>::new();
    for _ in 0..=MAX_REQUEST_HEADERS {
        let line = read_crlf_line(reader, MAX_REGISTRY_LINE_BYTES, &mut total)?;
        if line.is_empty() {
            if !headers.contains_key("host") {
                return Err(RequestFailure::public(400, "host header required\n"));
            }
            if headers.contains_key("transfer-encoding") {
                return Err(RequestFailure::public(
                    400,
                    "transfer encoding is not supported\n",
                ));
            }
            if headers.contains_key("expect") {
                return Err(RequestFailure::public(
                    417,
                    "expectation is not supported\n",
                ));
            }
            let content_length = match headers.get("content-length") {
                Some(value) => parse_content_length(value)?,
                None if method == "PUT" => {
                    return Err(RequestFailure::public(411, "content length required\n"));
                }
                None => 0,
            };
            let keep_alive = request_keep_alive(headers.get("connection").map(String::as_str))?;
            return Ok(RequestHead {
                method: method.to_string(),
                path: path.to_string(),
                headers,
                content_length,
                keep_alive,
            });
        }
        if line
            .first()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        {
            return Err(RequestFailure::public(
                400,
                "folded headers are not allowed\n",
            ));
        }
        if !line
            .iter()
            .all(|byte| byte.is_ascii() && (*byte == b'\t' || *byte >= b' '))
        {
            return Err(RequestFailure::public(400, "invalid request header\n"));
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| RequestFailure::public(400, "invalid request header\n"))?;
        let Some((name, value)) = text.split_once(':') else {
            return Err(RequestFailure::public(400, "invalid request header\n"));
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(RequestFailure::public(400, "invalid request header\n"));
        }
        let name = name.to_ascii_lowercase();
        if headers.insert(name, value.trim().to_string()).is_some() {
            return Err(RequestFailure::public(400, "duplicate request header\n"));
        }
    }
    Err(RequestFailure::public(431, "too many request headers\n"))
}

fn request_keep_alive(connection: Option<&str>) -> Result<bool, RequestFailure> {
    let mut close = false;
    if let Some(value) = connection {
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() || !token.bytes().all(is_http_token_byte) {
                return Err(RequestFailure::public(400, "invalid connection header\n"));
            }
            close |= token.eq_ignore_ascii_case("close");
        }
    }
    Ok(!close)
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn read_crlf_line(
    reader: &mut impl BufRead,
    max_line_bytes: usize,
    total: &mut usize,
) -> Result<Vec<u8>, RequestFailure> {
    let mut line = Vec::with_capacity(128);
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| RequestFailure::public(400, "failed to read request\n"))?;
        if available.is_empty() {
            return Err(RequestFailure::public(400, "truncated request headers\n"));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > max_line_bytes.saturating_add(2)
            || total.saturating_add(take) > MAX_REQUEST_HEADER_BYTES
        {
            return Err(RequestFailure::public(431, "request headers too large\n"));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        *total += take;
        if line.ends_with(b"\n") {
            if !line.ends_with(b"\r\n") {
                return Err(RequestFailure::public(400, "headers require CRLF\n"));
            }
            line.truncate(line.len() - 2);
            return Ok(line);
        }
    }
}

fn valid_origin_form_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= MAX_REQUEST_LINE_BYTES
        && path.is_ascii()
        && !path.contains('%')
        && !path.contains('\\')
        && !path.contains('?')
        && !path.contains('#')
        && !path.contains("//")
        && !path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
}

fn parse_content_length(value: &str) -> Result<u64, RequestFailure> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RequestFailure::public(400, "invalid content length\n"));
    }
    value
        .parse::<u64>()
        .map_err(|_| RequestFailure::public(413, "request body too large\n"))
}

fn validate_publish_request_head(
    state: &RegistryState,
    request: &RequestHead,
    package_name: &str,
    version: &str,
) -> Result<String, RequestFailure> {
    match authorize_publish(
        &state.credentials,
        request.headers.get("authorization"),
        package_name,
    ) {
        PublishAuthorization::Allowed => {}
        PublishAuthorization::Invalid => {
            return Err(RequestFailure::public(401, "invalid publish credentials\n"));
        }
        PublishAuthorization::Forbidden => {
            return Err(RequestFailure::public(403, "publish forbidden\n"));
        }
    }
    if request.headers.get("content-type").map(String::as_str) != Some(PACKAGE_CONTENT_TYPE) {
        return Err(RequestFailure::public(
            415,
            "unsupported package content type\n",
        ));
    }
    if request.content_length == 0 || request.content_length > MAX_REGISTRY_DOWNLOAD_BYTES {
        return Err(RequestFailure::public(413, "package archive too large\n"));
    }
    let expected_checksum = request
        .headers
        .get("x-ku-checksum")
        .ok_or_else(|| RequestFailure::public(400, "missing package checksum\n"))?;
    validate_sha256_checksum(expected_checksum, Span::default())
        .map_err(|_| RequestFailure::public(400, "invalid package checksum\n"))?;
    let expected_idempotency = format!("{package_name}-{version}-{expected_checksum}");
    if request.headers.get("idempotency-key") != Some(&expected_idempotency) {
        return Err(RequestFailure::public(400, "invalid idempotency key\n"));
    }
    Ok(expected_checksum.to_string())
}

fn validate_yank_request_head(
    state: &RegistryState,
    request: &RequestHead,
    package_name: &str,
    version: &str,
) -> Result<(), RequestFailure> {
    match authorize_publish(
        &state.credentials,
        request.headers.get("authorization"),
        package_name,
    ) {
        PublishAuthorization::Allowed => {}
        PublishAuthorization::Invalid => {
            return Err(RequestFailure::public(401, "invalid publish credentials\n"));
        }
        PublishAuthorization::Forbidden => {
            return Err(RequestFailure::public(403, "publish forbidden\n"));
        }
    }
    if request.content_length != 0 {
        return Err(RequestFailure::public(
            400,
            "yank request body is not allowed\n",
        ));
    }
    let expected_idempotency = format!("yank-{package_name}-{version}");
    if request.headers.get("idempotency-key") != Some(&expected_idempotency) {
        return Err(RequestFailure::public(400, "invalid idempotency key\n"));
    }
    Ok(())
}

fn yank_request(
    state: &RegistryState,
    package_name: &str,
    version: &str,
    _mutation_slot: MutationGuard<'_>,
    deadline: Instant,
) -> Result<HttpResponse, RequestFailure> {
    ensure_request_deadline(deadline)?;
    let _lock = PackagePublishLock::try_acquire(&state.storage.locks, package_name, deadline)?;
    ensure_request_deadline(deadline)?;
    let package_root = existing_package_storage_root(&state.storage, package_name)?
        .ok_or_else(|| RequestFailure::public(404, "package version not found\n"))?;
    let versions_root = existing_real_child_directory(&package_root, VERSIONS_DIR)?;
    let version_root = versions_root.join(version);
    let version_metadata = match fs::symlink_metadata(&version_root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(RequestFailure::public(404, "package version not found\n"));
        }
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect package version for yank: {err}"
            )));
        }
    };
    if version_metadata.file_type().is_symlink() || !version_metadata.is_dir() {
        return Err(RequestFailure::internal(
            "package version selected for yank is not a real directory",
        ));
    }
    read_entry_metadata(&version_root, package_name, version)?;
    let yanks_root = ensure_request_child_directory(&package_root, YANKS_DIR)?;
    let yanked = load_yanked_versions(&package_root, &versions_root, deadline)?;
    let current = ensure_signed_index_locked(state, &package_root, package_name, deadline)?
        .ok_or_else(|| RequestFailure::internal("published package has no signed index"))?;
    let mut visible = entries_from_signed_index(&current, package_name)?;
    let index_exposes_version = visible.iter().any(|entry| entry.version == version);
    if yanked.contains(version) && !index_exposes_version {
        return Ok(HttpResponse::empty(200));
    }
    if !yanked.contains(version) && !index_exposes_version {
        return Err(RequestFailure::internal(
            "signed index omitted a published version without a yank marker",
        ));
    }
    visible.retain(|entry| entry.version != version);
    let signed = generate_signed_index(&state.signing_key, package_name, visible)?;
    persist_signed_generation(&package_root, &signed)?;
    write_pending_marker(&package_root, version)?;
    create_yank_marker(&yanks_root, version)?;
    switch_signed_index(&package_root, &signed.generation)?;
    if let Err(err) = remove_pending_marker(&package_root) {
        log_maintenance_failure("clear yank recovery marker", &err);
    }
    if let Err(err) = cleanup_old_index_generations(&package_root, &signed.generation) {
        log_maintenance_failure("clean old signed index generations after yank", &err);
    }
    Ok(HttpResponse::empty(200))
}

fn publish_request(
    state: &RegistryState,
    request: &RequestHead,
    reader: &mut impl Read,
    publish: ValidatedPublish<'_>,
    deadline: Instant,
) -> Result<HttpResponse, RequestFailure> {
    ensure_request_deadline(deadline)?;
    let package_name = publish.package_name;
    let version = publish.version;
    let expected_checksum = publish.expected_checksum;

    let staging_path = create_staging_directory(&state.storage.staging)?;
    let mut staging = TemporaryDirectory::new(staging_path.clone());
    let upload_path = staging_path.join("upload.tar.zst");
    let mut upload = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&upload_path)
        .map_err(|err| {
            RequestFailure::internal(format!("failed to create upload staging: {err}"))
        })?;
    let mut hasher = Sha256::new();
    let mut remaining = request.content_length;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        ensure_request_deadline(deadline)?;
        let read_len = remaining.min(buffer.len() as u64) as usize;
        let count = reader.read(&mut buffer[..read_len]).map_err(|err| {
            if err.kind() == io::ErrorKind::TimedOut {
                RequestFailure::public(408, "request timed out\n")
            } else {
                RequestFailure::public(400, "failed to read package body\n")
            }
        })?;
        if count == 0 {
            return Err(RequestFailure::public(400, "truncated package body\n"));
        }
        hasher.update(&buffer[..count]);
        upload.write_all(&buffer[..count]).map_err(|err| {
            RequestFailure::internal(format!("failed to write upload staging: {err}"))
        })?;
        remaining -= count as u64;
    }
    upload
        .sync_all()
        .map_err(|err| RequestFailure::internal(format!("failed to sync upload staging: {err}")))?;
    drop(upload);
    let actual_checksum = format!("sha256-{:x}", hasher.finalize());
    if !constant_time_equal(actual_checksum.as_bytes(), expected_checksum.as_bytes()) {
        return Err(RequestFailure::public(400, "package checksum mismatch\n"));
    }

    ensure_request_deadline(deadline)?;
    let verification_path = staging_path.join("verify");
    let mut dependencies = validate_registry_publish_archive(
        &upload_path,
        &verification_path,
        package_name,
        version,
        deadline,
        Span::default(),
    )
    .map_err(|err| {
        if err.code.as_deref() == Some("package_archive_timeout") {
            RequestFailure::public(408, "request timed out\n")
        } else {
            RequestFailure {
                status: 400,
                public_message: "invalid package archive\n",
                internal: Some(format!(
                    "package validation rejected {package_name}@{version}: {err}"
                )),
            }
        }
    })?;
    dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    fs::remove_dir_all(&verification_path).map_err(|err| {
        RequestFailure::internal(format!("failed to remove verified upload tree: {err}"))
    })?;

    let artifact_name = artifact_storage_name(&actual_checksum)?;
    let artifact_path = staging_path.join(&artifact_name);
    fs::rename(&upload_path, &artifact_path).map_err(|err| {
        RequestFailure::internal(format!("failed to finalize staged artifact: {err}"))
    })?;
    let metadata = EntryMetadata {
        name: package_name.to_string(),
        version: version.to_string(),
        checksum: actual_checksum.clone(),
        dependencies,
    };
    write_entry_metadata(&staging_path.join(ENTRY_METADATA_FILE), &metadata)?;
    sync_directory(&staging_path)
        .map_err(|err| RequestFailure::internal(format!("failed to sync staged version: {err}")))?;

    ensure_request_deadline(deadline)?;
    let _lock = PackagePublishLock::try_acquire(&state.storage.locks, package_name, deadline)?;
    ensure_request_deadline(deadline)?;
    let package_root = package_storage_root(&state.storage, package_name)?;
    let versions_root = ensure_request_child_directory(&package_root, VERSIONS_DIR)?;
    ensure_request_child_directory(&package_root, YANKS_DIR)?;
    ensure_request_child_directory(&package_root, INDEX_GENERATIONS_DIR)?;
    let final_path = versions_root.join(version);
    match fs::symlink_metadata(&final_path) {
        Ok(existing) => {
            if existing.file_type().is_symlink() || !existing.is_dir() {
                return Err(RequestFailure::internal(
                    "published version path is not a real directory",
                ));
            }
            let existing = read_entry_metadata(&final_path, package_name, version)?;
            if existing.checksum != actual_checksum {
                return Ok(HttpResponse::empty(409));
            }
            if existing != metadata {
                return Err(RequestFailure::internal(
                    "published version metadata does not match its immutable artifact",
                ));
            }
            verify_committed_artifact(&final_path, &existing, deadline)?;
            sync_directory(&final_path).map_err(|err| {
                RequestFailure::internal(format!(
                    "failed to re-sync the idempotent package version: {err}"
                ))
            })?;
            sync_directory(&versions_root).map_err(|err| {
                RequestFailure::internal(format!(
                    "failed to re-sync the package versions directory: {err}"
                ))
            })?;
            ensure_signed_index_locked(state, &package_root, package_name, deadline)?;
            return Ok(HttpResponse::empty(200));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect published version: {err}"
            )));
        }
    }
    let current = ensure_signed_index_locked(state, &package_root, package_name, deadline)?;
    let mut candidate_entries = match current {
        Some(signed) => entries_from_signed_index(&signed, package_name)?,
        None => Vec::new(),
    };
    let yanked = load_yanked_versions(&package_root, &versions_root, deadline)?;
    if candidate_entries
        .iter()
        .any(|entry| yanked.contains(&entry.version))
    {
        return Err(RequestFailure::internal(
            "signed index exposes a package version with a yank marker",
        ));
    }
    ensure_physical_version_capacity(candidate_entries.len(), yanked.len())?;
    candidate_entries.push(metadata.clone());
    let signed = generate_signed_index(&state.signing_key, package_name, candidate_entries)
        .map_err(|failure| {
            if failure.status == 500 {
                RequestFailure::public(409, "package index limit reached\n")
            } else {
                failure
            }
        })?;
    persist_signed_generation(&package_root, &signed)?;
    write_pending_marker(&package_root, version)?;
    fs::rename(&staging_path, &final_path).map_err(|err| {
        let _ = remove_pending_marker(&package_root);
        RequestFailure::internal(format!(
            "failed to atomically commit package version: {err}"
        ))
    })?;
    staging.keep();
    sync_directory(&versions_root).map_err(|err| {
        RequestFailure::internal(format!("failed to sync package version directory: {err}"))
    })?;
    switch_signed_index(&package_root, &signed.generation)?;
    // The atomic pointer switch is the externally visible commit point. Any
    // maintenance after it is recoverable and must not turn a committed
    // publication into an ambiguous error response.
    if let Err(err) = remove_pending_marker(&package_root) {
        log_maintenance_failure("clear publish recovery marker", &err);
    }
    if let Err(err) = cleanup_old_index_generations(&package_root, &signed.generation) {
        log_maintenance_failure("clean old signed index generations", &err);
    }
    Ok(HttpResponse::empty(201))
}

fn ensure_physical_version_capacity(
    visible_versions: usize,
    yanked_versions: usize,
) -> Result<(), RequestFailure> {
    let physical_versions = physical_version_count(visible_versions, yanked_versions)?;
    if physical_versions >= MAX_REGISTRY_INDEX_VERSIONS {
        return Err(RequestFailure::public(
            409,
            "package version limit reached\n",
        ));
    }
    Ok(())
}

fn validate_physical_version_count(
    visible_versions: usize,
    yanked_versions: usize,
) -> Result<(), RequestFailure> {
    let physical_versions = physical_version_count(visible_versions, yanked_versions)?;
    if physical_versions > MAX_REGISTRY_INDEX_VERSIONS {
        return Err(RequestFailure::internal(
            "physical package version count exceeds the registry limit",
        ));
    }
    Ok(())
}

fn physical_version_count(
    visible_versions: usize,
    yanked_versions: usize,
) -> Result<usize, RequestFailure> {
    visible_versions
        .checked_add(yanked_versions)
        .ok_or_else(|| RequestFailure::internal("physical package version count overflowed"))
}

struct ValidatedPublish<'a> {
    package_name: &'a str,
    version: &'a str,
    expected_checksum: String,
    _slot: MutationGuard<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryMetadata {
    name: String,
    version: String,
    checksum: String,
    dependencies: Vec<PackageDependency>,
}

#[derive(Clone)]
struct SignedIndex {
    bytes: Vec<u8>,
    signature: Vec<u8>,
    generation: String,
}

fn load_or_recover_signed_index(
    state: &RegistryState,
    package_name: &str,
    deadline: Instant,
) -> Result<Option<SignedIndex>, RequestFailure> {
    validate_package_name(package_name, Span::default())
        .map_err(|_| RequestFailure::public(404, "not found\n"))?;
    let package_root = state.storage.packages.join(package_name);
    let metadata = match fs::symlink_metadata(&package_root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect package index storage: {err}"
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "package index storage is not a real directory",
        ));
    }
    if !pending_marker_exists(&package_root)? {
        if let Ok(Some(cached)) = load_cached_signed_index(&package_root, &state.signing_key) {
            return Ok(Some(cached));
        }
    }
    let _lock = PackagePublishLock::acquire(&state.storage.locks, package_name, deadline)?;
    ensure_signed_index_locked(state, &package_root, package_name, deadline)
}

fn ensure_signed_index_locked(
    state: &RegistryState,
    package_root: &Path,
    package_name: &str,
    deadline: Instant,
) -> Result<Option<SignedIndex>, RequestFailure> {
    let pending_version = read_pending_marker(package_root)?;
    if pending_version.is_none() {
        match load_cached_signed_index(package_root, &state.signing_key) {
            Ok(Some(cached)) => return Ok(Some(cached)),
            Ok(None) => {}
            Err(err) => {
                log_maintenance_failure("rebuild damaged signed index cache", &err);
                discard_broken_cached_index(package_root)?;
            }
        }
    }
    let versions_root = existing_real_child_directory(package_root, VERSIONS_DIR)?;
    let mut versions =
        load_committed_metadata_for_recovery(&versions_root, package_name, deadline)?;
    if versions.is_empty() {
        discard_broken_cached_index(package_root)?;
        remove_pending_marker(package_root)?;
        cleanup_old_index_generations(package_root, "")?;
        return Ok(None);
    }
    if let Some(pending_version) = pending_version {
        if let Some(pending) = versions
            .iter()
            .find(|metadata| metadata.version == pending_version)
        {
            verify_committed_artifact(&versions_root.join(&pending.version), pending, deadline)?;
        }
    }
    let yanked = load_yanked_versions(package_root, &versions_root, deadline)?;
    versions.retain(|metadata| !yanked.contains(&metadata.version));
    let signed = generate_signed_index(&state.signing_key, package_name, versions)?;
    persist_signed_generation(package_root, &signed)?;
    switch_signed_index(package_root, &signed.generation)?;
    if let Err(err) = remove_pending_marker(package_root) {
        log_maintenance_failure("clear recovered publish marker", &err);
    }
    if let Err(err) = cleanup_old_index_generations(package_root, &signed.generation) {
        log_maintenance_failure("clean recovered signed index generations", &err);
    }
    Ok(Some(signed))
}

fn pending_marker_exists(package_root: &Path) -> Result<bool, RequestFailure> {
    read_pending_marker(package_root).map(|version| version.is_some())
}

fn read_pending_marker(package_root: &Path) -> Result<Option<String>, RequestFailure> {
    let path = package_root.join(PUBLISH_PENDING_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 128 {
                return Err(RequestFailure::internal(
                    "publish recovery marker is not a bounded regular file",
                ));
            }
            let bytes = read_request_file(&path, 128, "publish recovery marker")?;
            let version = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|value| value.strip_suffix('\n'))
                .filter(|value| !value.is_empty() && !value.contains(['\r', '\n']))
                .ok_or_else(|| {
                    RequestFailure::internal("publish recovery marker has invalid syntax")
                })?;
            parse_package_version(version, Span::default()).map_err(|_| {
                RequestFailure::internal("publish recovery marker has an invalid version")
            })?;
            Ok(Some(version.to_string()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(RequestFailure::internal(format!(
            "failed to inspect publish recovery marker: {err}"
        ))),
    }
}

// Recovery always validates the complete bounded metadata graph. Artifact
// bytes are checked for the pending version below and for every version when
// served; historical local corruption cannot be signed into different metadata.
fn load_committed_metadata_for_recovery(
    versions_root: &Path,
    package_name: &str,
    deadline: Instant,
) -> Result<Vec<EntryMetadata>, RequestFailure> {
    let mut versions = Vec::new();
    let mut index_items = 0usize;
    for entry in fs::read_dir(versions_root).map_err(|err| {
        RequestFailure::internal(format!("failed to read package index storage: {err}"))
    })? {
        ensure_request_deadline(deadline)?;
        if versions.len() >= MAX_REGISTRY_INDEX_VERSIONS {
            return Err(RequestFailure::internal(
                "package index version limit exceeded",
            ));
        }
        let entry = entry.map_err(|err| {
            RequestFailure::internal(format!("failed to read package version entry: {err}"))
        })?;
        let version = entry
            .file_name()
            .to_str()
            .ok_or_else(|| RequestFailure::internal("package version path is not UTF-8"))?
            .to_string();
        parse_package_version(&version, Span::default())
            .map_err(|_| RequestFailure::internal("package version storage name is invalid"))?;
        let entry_metadata = fs::symlink_metadata(entry.path()).map_err(|err| {
            RequestFailure::internal(format!("failed to inspect package version entry: {err}"))
        })?;
        if entry_metadata.file_type().is_symlink() || !entry_metadata.is_dir() {
            return Err(RequestFailure::internal(
                "package version entry is not a real directory",
            ));
        }
        let metadata = read_entry_metadata(&entry.path(), package_name, &version)?;
        index_items = index_items
            .saturating_add(1)
            .saturating_add(metadata.dependencies.len());
        if index_items > MAX_INDEX_ITEMS {
            return Err(RequestFailure::internal(
                "package index item limit exceeded",
            ));
        }
        versions.push(metadata);
    }
    Ok(versions)
}

fn audit_clean_package_index_storage(
    versions_root: &Path,
    package_name: &str,
    visible: &[EntryMetadata],
    yanked: &HashSet<String>,
    deadline: Instant,
) -> Result<(), RequestFailure> {
    validate_physical_version_count(visible.len(), yanked.len())?;
    let physical = load_committed_metadata_for_recovery(versions_root, package_name, deadline)?;
    let mut physical_by_version = physical
        .into_iter()
        .map(|metadata| (metadata.version.clone(), metadata))
        .collect::<HashMap<_, _>>();
    let mut visible_versions = HashSet::with_capacity(visible.len());

    for indexed in visible {
        if !visible_versions.insert(indexed.version.as_str()) {
            return Err(RequestFailure::internal(
                "signed package index contains a duplicate version",
            ));
        }
        let Some(stored) = physical_by_version.remove(&indexed.version) else {
            return Err(RequestFailure::internal(format!(
                "signed package index references missing physical version {}",
                indexed.version
            )));
        };
        if stored != *indexed {
            return Err(RequestFailure::internal(format!(
                "signed package index metadata does not match physical version {}",
                indexed.version
            )));
        }
    }

    for version in yanked {
        if physical_by_version.remove(version).is_none() {
            return Err(RequestFailure::internal(format!(
                "package yank marker references missing physical metadata for version {version}"
            )));
        }
    }

    if let Some(version) = physical_by_version.keys().min() {
        return Err(RequestFailure::internal(format!(
            "physical package version {version} is absent from the signed index and yank storage"
        )));
    }
    Ok(())
}

fn load_yanked_versions(
    package_root: &Path,
    versions_root: &Path,
    deadline: Instant,
) -> Result<HashSet<String>, RequestFailure> {
    let yanks_root = existing_real_child_directory(package_root, YANKS_DIR)?;
    let mut yanked = HashSet::new();
    for entry in fs::read_dir(&yanks_root).map_err(|err| {
        RequestFailure::internal(format!("failed to read package yank storage: {err}"))
    })? {
        ensure_request_deadline(deadline)?;
        if yanked.len() >= MAX_REGISTRY_INDEX_VERSIONS {
            return Err(RequestFailure::internal(
                "package yank marker limit exceeded",
            ));
        }
        let entry = entry.map_err(|err| {
            RequestFailure::internal(format!("failed to read package yank marker: {err}"))
        })?;
        let version = entry
            .file_name()
            .to_str()
            .ok_or_else(|| RequestFailure::internal("package yank marker is not UTF-8"))?
            .to_string();
        parse_package_version(&version, Span::default())
            .map_err(|_| RequestFailure::internal("package yank marker version is invalid"))?;
        validate_yank_marker_directory(&entry.path(), &yanks_root)?;
        let version_path = versions_root.join(&version);
        let version_metadata = fs::symlink_metadata(&version_path).map_err(|err| {
            RequestFailure::internal(format!(
                "package yank marker references a missing version: {err}"
            ))
        })?;
        if version_metadata.file_type().is_symlink() || !version_metadata.is_dir() {
            return Err(RequestFailure::internal(
                "package yank marker does not reference a real version directory",
            ));
        }
        if !yanked.insert(version) {
            return Err(RequestFailure::internal(
                "package yank storage contains a duplicate version",
            ));
        }
    }
    Ok(yanked)
}

fn validate_yank_marker_directory(path: &Path, yanks_root: &Path) -> Result<(), RequestFailure> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        RequestFailure::internal(format!("failed to inspect package yank marker: {err}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "package yank marker must be a real empty directory",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|err| {
        RequestFailure::internal(format!("failed to resolve package yank marker: {err}"))
    })?;
    if canonical.parent() != Some(yanks_root) {
        return Err(RequestFailure::internal(
            "package yank marker escaped its storage directory",
        ));
    }
    if fs::read_dir(&canonical)
        .map_err(|err| {
            RequestFailure::internal(format!("failed to inspect package yank marker: {err}"))
        })?
        .next()
        .is_some()
    {
        return Err(RequestFailure::internal(
            "package yank marker directory must be empty",
        ));
    }
    Ok(())
}

fn create_yank_marker(yanks_root: &Path, version: &str) -> Result<(), RequestFailure> {
    parse_package_version(version, Span::default())
        .map_err(|_| RequestFailure::internal("package yank marker version is invalid"))?;
    let marker = yanks_root.join(version);
    match fs::create_dir(&marker) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to create package yank marker: {err}"
            )));
        }
    }
    validate_yank_marker_directory(&marker, yanks_root)?;
    sync_directory(&marker).map_err(|err| {
        RequestFailure::internal(format!("failed to sync package yank marker: {err}"))
    })?;
    sync_directory(yanks_root).map_err(|err| {
        RequestFailure::internal(format!("failed to sync package yank storage: {err}"))
    })?;
    sync_directory(yanks_root.parent().unwrap_or_else(|| Path::new("."))).map_err(|err| {
        RequestFailure::internal(format!("failed to sync package storage after yank: {err}"))
    })
}

fn generate_signed_index(
    signing_key: &SigningKey,
    package_name: &str,
    mut versions: Vec<EntryMetadata>,
) -> Result<SignedIndex, RequestFailure> {
    if versions.len() > MAX_REGISTRY_INDEX_VERSIONS {
        return Err(RequestFailure::internal(
            "package index version limit exceeded",
        ));
    }
    let item_count = versions.iter().fold(0usize, |count, entry| {
        count.saturating_add(1 + entry.dependencies.len())
    });
    if item_count > MAX_INDEX_ITEMS {
        return Err(RequestFailure::internal(
            "package index item limit exceeded",
        ));
    }
    versions.sort_by_key(|entry| {
        std::cmp::Reverse(
            parse_package_version(&entry.version, Span::default())
                .expect("stored registry version was validated"),
        )
    });
    let mut index = format!("name = \"{package_name}\"\n").into_bytes();
    for entry in versions {
        let checksum_hex = entry
            .checksum
            .strip_prefix("sha256-")
            .expect("stored checksum was validated");
        append_index_bytes(&mut index, b"\n[[version]]\n")?;
        append_index_bytes(
            &mut index,
            format!("version = \"{}\"\n", entry.version).as_bytes(),
        )?;
        append_index_bytes(
            &mut index,
            format!(
                "url = \"../../artifacts/{package_name}-{}-sha256-{checksum_hex}.tar.zst\"\n",
                entry.version
            )
            .as_bytes(),
        )?;
        append_index_bytes(
            &mut index,
            format!("checksum = \"{}\"\n", entry.checksum).as_bytes(),
        )?;
        for dependency in entry.dependencies {
            append_index_bytes(
                &mut index,
                format!("dep.{} = \"{}\"\n", dependency.name, dependency.version).as_bytes(),
            )?;
        }
    }
    let signature = signing_key.sign(&index);
    let signature = format!("ed25519-{}\n", encode_hex(&signature.to_bytes())).into_bytes();
    let mut generation_hasher = Sha256::new();
    generation_hasher.update(signing_key.verifying_key().as_bytes());
    generation_hasher.update(&index);
    let generation = encode_hex(&generation_hasher.finalize());
    Ok(SignedIndex {
        bytes: index,
        signature,
        generation,
    })
}

fn entries_from_signed_index(
    signed: &SignedIndex,
    expected_name: &str,
) -> Result<Vec<EntryMetadata>, RequestFailure> {
    let source = std::str::from_utf8(&signed.bytes)
        .map_err(|_| RequestFailure::internal("cached signed index is not UTF-8"))?;
    let index_url = format!("https://registry.invalid/v1/packages/{expected_name}/index.toml");
    let parsed = parse_registry_index(source, &index_url, Span::default()).map_err(|err| {
        RequestFailure::internal(format!("cached signed index is invalid: {err}"))
    })?;
    if parsed.name != expected_name {
        return Err(RequestFailure::internal(
            "cached signed index identity does not match its package",
        ));
    }
    Ok(parsed
        .versions
        .into_iter()
        .map(|version| EntryMetadata {
            name: expected_name.to_string(),
            version: version.version,
            checksum: version.checksum,
            dependencies: version.dependencies,
        })
        .collect())
}

fn append_index_bytes(target: &mut Vec<u8>, value: &[u8]) -> Result<(), RequestFailure> {
    if value.len() > MAX_REGISTRY_LINE_BYTES
        || target.len().saturating_add(value.len()) > MAX_REGISTRY_INDEX_BYTES as usize
    {
        return Err(RequestFailure::internal(
            "generated package index exceeds protocol limits",
        ));
    }
    target.extend_from_slice(value);
    Ok(())
}

fn persist_signed_generation(
    package_root: &Path,
    signed: &SignedIndex,
) -> Result<(), RequestFailure> {
    let indexes = ensure_request_child_directory(package_root, INDEX_GENERATIONS_DIR)?;
    let generation_path = indexes.join(&signed.generation);
    match fs::symlink_metadata(&generation_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RequestFailure::internal(
                    "signed index generation is not a real directory",
                ));
            }
            let existing_index = read_request_file(
                &generation_path.join("index.toml"),
                MAX_REGISTRY_INDEX_BYTES,
                "signed index",
            )?;
            let existing_signature = read_request_file(
                &generation_path.join("index.toml.sig"),
                512,
                "signed index signature",
            )?;
            if !constant_time_equal(&existing_index, &signed.bytes)
                || !constant_time_equal(&existing_signature, &signed.signature)
            {
                return Err(RequestFailure::internal(
                    "signed index generation conflicts with existing storage",
                ));
            }
            sync_directory(&generation_path).map_err(|err| {
                RequestFailure::internal(format!(
                    "failed to re-sync signed index generation: {err}"
                ))
            })?;
            sync_directory(&indexes).map_err(|err| {
                RequestFailure::internal(format!("failed to re-sync signed index storage: {err}"))
            })?;
            return Ok(());
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect signed index generation: {err}"
            )));
        }
    }
    let temporary_path = indexes.join(format!(".{}.{}.tmp", signed.generation, staging_nonce()));
    fs::create_dir(&temporary_path).map_err(|err| {
        RequestFailure::internal(format!("failed to create signed index staging: {err}"))
    })?;
    let mut temporary = TemporaryDirectory::new(temporary_path.clone());
    write_new_synced_file(&temporary_path.join("index.toml"), &signed.bytes)?;
    write_new_synced_file(&temporary_path.join("index.toml.sig"), &signed.signature)?;
    sync_directory(&temporary_path).map_err(|err| {
        RequestFailure::internal(format!("failed to sync signed index generation: {err}"))
    })?;
    fs::rename(&temporary_path, &generation_path).map_err(|err| {
        RequestFailure::internal(format!("failed to install signed index generation: {err}"))
    })?;
    temporary.keep();
    sync_directory(&indexes).map_err(|err| {
        RequestFailure::internal(format!("failed to sync signed index storage: {err}"))
    })
}

fn load_cached_signed_index(
    package_root: &Path,
    signing_key: &SigningKey,
) -> Result<Option<SignedIndex>, RequestFailure> {
    let current_path = package_root.join(INDEX_CURRENT_FILE);
    let pointer = match fs::symlink_metadata(&current_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 128 {
                return Err(RequestFailure::internal(
                    "signed index pointer is not a bounded regular file",
                ));
            }
            read_request_file(&current_path, 128, "signed index pointer")?
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect signed index pointer: {err}"
            )));
        }
    };
    let generation = std::str::from_utf8(&pointer)
        .ok()
        .and_then(|value| value.strip_suffix('\n'))
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| RequestFailure::internal("signed index pointer is invalid"))?
        .to_ascii_lowercase();
    let indexes = existing_real_child_directory(package_root, INDEX_GENERATIONS_DIR)?;
    let generation_path = indexes.join(&generation);
    let metadata = fs::symlink_metadata(&generation_path).map_err(|err| {
        RequestFailure::internal(format!("failed to inspect signed index generation: {err}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "signed index generation is not a real directory",
        ));
    }
    let bytes = read_request_file(
        &generation_path.join("index.toml"),
        MAX_REGISTRY_INDEX_BYTES,
        "signed index",
    )?;
    let mut generation_hasher = Sha256::new();
    generation_hasher.update(signing_key.verifying_key().as_bytes());
    generation_hasher.update(&bytes);
    if encode_hex(&generation_hasher.finalize()) != generation {
        return Err(RequestFailure::internal(
            "signed index generation digest is invalid",
        ));
    }
    let signature = read_request_file(
        &generation_path.join("index.toml.sig"),
        512,
        "signed index signature",
    )?;
    let expected = format!(
        "ed25519-{}\n",
        encode_hex(&signing_key.sign(&bytes).to_bytes())
    )
    .into_bytes();
    if !constant_time_equal(&signature, &expected) {
        return Err(RequestFailure::internal(
            "signed index signature storage is invalid",
        ));
    }
    Ok(Some(SignedIndex {
        bytes,
        signature,
        generation,
    }))
}

fn discard_broken_cached_index(package_root: &Path) -> Result<(), RequestFailure> {
    let pointer_path = package_root.join(INDEX_CURRENT_FILE);
    let pointer = match fs::symlink_metadata(&pointer_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 128 {
                return Err(RequestFailure::internal(
                    "damaged signed index pointer is not a bounded regular file",
                ));
            }
            Some(read_request_file(
                &pointer_path,
                128,
                "signed index pointer",
            )?)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect damaged signed index pointer: {err}"
            )));
        }
    };
    if let Some(pointer) = pointer {
        let generation = std::str::from_utf8(&pointer)
            .ok()
            .and_then(|value| value.strip_suffix('\n'))
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase);
        fs::remove_file(&pointer_path).map_err(|err| {
            RequestFailure::internal(format!("failed to remove damaged index pointer: {err}"))
        })?;
        if let Some(generation) = generation {
            let generation_path = package_root.join(INDEX_GENERATIONS_DIR).join(generation);
            match fs::symlink_metadata(&generation_path) {
                Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
                    if !bounded_plain_tree(&generation_path)
                        .map_err(|err| RequestFailure::internal(err.to_string()))?
                    {
                        return Err(RequestFailure::internal(
                            "damaged signed index generation exceeds cleanup limits",
                        ));
                    }
                    fs::remove_dir_all(&generation_path).map_err(|err| {
                        RequestFailure::internal(format!(
                            "failed to remove damaged signed index generation: {err}"
                        ))
                    })?;
                }
                Ok(_) => {
                    return Err(RequestFailure::internal(
                        "damaged signed index generation is not a real directory",
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(RequestFailure::internal(format!(
                        "failed to inspect damaged signed index generation: {err}"
                    )));
                }
            }
        }
        sync_directory(package_root).map_err(|err| {
            RequestFailure::internal(format!("failed to sync repaired index pointer: {err}"))
        })?;
    }
    Ok(())
}

fn switch_signed_index(package_root: &Path, generation: &str) -> Result<(), RequestFailure> {
    if generation.len() != 64 || !generation.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RequestFailure::internal(
            "signed index generation is invalid",
        ));
    }
    write_file_atomically(
        &package_root.join(INDEX_CURRENT_FILE),
        format!("{generation}\n").as_bytes(),
    )
}

fn write_pending_marker(package_root: &Path, version: &str) -> Result<(), RequestFailure> {
    parse_package_version(version, Span::default())
        .map_err(|_| RequestFailure::internal("pending package version is invalid"))?;
    write_file_atomically(
        &package_root.join(PUBLISH_PENDING_FILE),
        format!("{version}\n").as_bytes(),
    )
}

fn remove_pending_marker(package_root: &Path) -> Result<(), RequestFailure> {
    let path = package_root.join(PUBLISH_PENDING_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(RequestFailure::internal(
                    "publish recovery marker is not a real file",
                ));
            }
            fs::remove_file(&path).map_err(|err| {
                RequestFailure::internal(format!("failed to clear publish recovery marker: {err}"))
            })?;
            sync_directory(package_root).map_err(|err| {
                RequestFailure::internal(format!("failed to sync publish recovery marker: {err}"))
            })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(RequestFailure::internal(format!(
            "failed to inspect publish recovery marker: {err}"
        ))),
    }
}

fn cleanup_old_index_generations(
    package_root: &Path,
    current_generation: &str,
) -> Result<(), RequestFailure> {
    let indexes = existing_real_child_directory(package_root, INDEX_GENERATIONS_DIR)?;
    let entries = fs::read_dir(&indexes).map_err(|err| {
        RequestFailure::internal(format!("failed to scan signed index generations: {err}"))
    })?;
    let mut scanned = 0usize;
    let mut removed = 0usize;
    for entry in entries {
        scanned += 1;
        if scanned > MAX_REGISTRY_INDEX_VERSIONS.saturating_add(128) {
            return Err(RequestFailure::internal(
                "signed index storage entry limit exceeded during old generation cleanup",
            ));
        }
        let entry = entry.map_err(|err| {
            RequestFailure::internal(format!("failed to read signed index generation: {err}"))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.eq_ignore_ascii_case(current_generation)
            || name.len() != 64
            || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        if removed >= MAX_INDEX_GENERATION_CLEANUP {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            RequestFailure::internal(format!("failed to inspect signed index generation: {err}"))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if !bounded_plain_tree(&path).map_err(|err| RequestFailure::internal(err.to_string()))? {
            return Err(RequestFailure::internal(
                "old signed index generation exceeds cleanup safety limits",
            ));
        }
        fs::remove_dir_all(&path).map_err(|err| {
            RequestFailure::internal(format!(
                "failed to remove old signed index generation: {err}"
            ))
        })?;
        removed += 1;
    }
    if removed != 0 {
        sync_directory(&indexes).map_err(|err| {
            RequestFailure::internal(format!(
                "failed to sync signed index generation cleanup: {err}"
            ))
        })?;
    }
    Ok(())
}

fn cleanup_package_temporary_files(package_root: &Path) -> Result<(), RequestFailure> {
    let mut root_entries = 0usize;
    for entry in fs::read_dir(package_root).map_err(|err| {
        RequestFailure::internal(format!("failed to scan package temporary files: {err}"))
    })? {
        root_entries += 1;
        if root_entries > 128 {
            return Err(RequestFailure::internal(
                "package root entry limit exceeded during temporary cleanup",
            ));
        }
        let entry = entry.map_err(|err| {
            RequestFailure::internal(format!("failed to read package temporary file: {err}"))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_atomic_file_staging_name(&name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|err| {
            RequestFailure::internal(format!("failed to inspect package temporary file: {err}"))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        fs::remove_file(entry.path()).map_err(|err| {
            RequestFailure::internal(format!("failed to remove package temporary file: {err}"))
        })?;
    }

    let indexes = package_root.join(INDEX_GENERATIONS_DIR);
    let index_metadata = match fs::symlink_metadata(&indexes) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect signed index temporary storage: {err}"
            )));
        }
    };
    if index_metadata.file_type().is_symlink() || !index_metadata.is_dir() {
        return Err(RequestFailure::internal(
            "signed index temporary storage is not a real directory",
        ));
    }
    let mut index_entries = 0usize;
    for entry in fs::read_dir(&indexes).map_err(|err| {
        RequestFailure::internal(format!(
            "failed to scan signed index temporary storage: {err}"
        ))
    })? {
        index_entries += 1;
        if index_entries > MAX_REGISTRY_INDEX_VERSIONS.saturating_add(128) {
            return Err(RequestFailure::internal(
                "signed index storage entry limit exceeded during temporary cleanup",
            ));
        }
        let entry = entry.map_err(|err| {
            RequestFailure::internal(format!(
                "failed to read signed index temporary entry: {err}"
            ))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_index_directory_staging_name(&name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|err| {
            RequestFailure::internal(format!(
                "failed to inspect signed index temporary entry: {err}"
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if !bounded_plain_tree(&entry.path())
            .map_err(|err| RequestFailure::internal(err.to_string()))?
        {
            continue;
        }
        fs::remove_dir_all(entry.path()).map_err(|err| {
            RequestFailure::internal(format!(
                "failed to remove signed index temporary entry: {err}"
            ))
        })?;
    }
    Ok(())
}

fn valid_atomic_file_staging_name(name: &str) -> bool {
    [".index.", ".publish."].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(".tmp"))
            .is_some_and(valid_staging_nonce)
    })
}

fn valid_index_directory_staging_name(name: &str) -> bool {
    let Some(value) = name
        .strip_prefix('.')
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((generation, nonce)) = value.split_once('.') else {
        return false;
    };
    generation.len() == 64
        && generation.bytes().all(|byte| byte.is_ascii_hexdigit())
        && valid_staging_nonce(nonce)
}

fn valid_staging_nonce(value: &str) -> bool {
    let mut parts = value.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(process), Some(counter), Some(nanos), None)
            if process.parse::<u32>().is_ok()
                && counter.parse::<u64>().is_ok()
                && nanos.parse::<u128>().is_ok()
    )
}

fn write_file_atomically(path: &Path, bytes: &[u8]) -> Result<(), RequestFailure> {
    let temporary_path = path.with_extension(format!("{}.tmp", staging_nonce()));
    let mut temporary = TemporaryFile::new(temporary_path.clone());
    write_new_synced_file(&temporary_path, bytes)?;
    replace_file_atomically(&temporary_path, path).map_err(|err| {
        RequestFailure::internal(format!(
            "failed to install registry metadata atomically: {err}"
        ))
    })?;
    temporary.keep();
    sync_directory(path.parent().unwrap_or_else(|| Path::new("."))).map_err(|err| {
        RequestFailure::internal(format!("failed to sync registry metadata directory: {err}"))
    })
}

fn write_new_synced_file(path: &Path, bytes: &[u8]) -> Result<(), RequestFailure> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| {
            RequestFailure::internal(format!("failed to create registry file: {err}"))
        })?;
    file.write_all(bytes)
        .map_err(|err| RequestFailure::internal(format!("failed to write registry file: {err}")))?;
    file.sync_all()
        .map_err(|err| RequestFailure::internal(format!("failed to sync registry file: {err}")))
}

fn read_request_file(path: &Path, max_bytes: u64, kind: &str) -> Result<Vec<u8>, RequestFailure> {
    read_bounded_regular_file(path, max_bytes, kind)
        .map_err(|err| RequestFailure::internal(err.to_string()))
}

struct TemporaryFile {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: true,
        }
    }

    fn keep(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn staging_nonce() -> String {
    let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{id}-{nanos}", std::process::id())
}

fn ensure_request_child_directory(root: &Path, child: &str) -> Result<PathBuf, RequestFailure> {
    let path = root.join(child);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to create registry storage directory: {err}"
            )));
        }
    }
    existing_real_child_directory(root, child)
}

fn existing_real_child_directory(root: &Path, child: &str) -> Result<PathBuf, RequestFailure> {
    let path = root.join(child);
    let metadata = fs::symlink_metadata(&path).map_err(|err| {
        RequestFailure::internal(format!(
            "failed to inspect registry storage directory: {err}"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "registry storage path is not a real directory",
        ));
    }
    let canonical = fs::canonicalize(&path).map_err(|err| {
        RequestFailure::internal(format!(
            "failed to resolve registry storage directory: {err}"
        ))
    })?;
    if canonical.parent() != Some(root) {
        return Err(RequestFailure::internal(
            "registry storage path escaped its parent",
        ));
    }
    Ok(canonical)
}

#[cfg(not(windows))]
fn replace_file_atomically(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file_atomically(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let mut source_wide = source.as_os_str().encode_wide().collect::<Vec<_>>();
    source_wide.push(0);
    let mut target_wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    target_wide.push(0);
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct ArtifactReference {
    name: String,
    version: String,
    checksum: String,
}

fn open_artifact_response(
    state: &RegistryState,
    reference: &ArtifactReference,
    deadline: Instant,
) -> Result<HttpResponse, RequestFailure> {
    let version_root = state
        .storage
        .packages
        .join(&reference.name)
        .join(VERSIONS_DIR)
        .join(&reference.version);
    let metadata = match fs::symlink_metadata(&version_root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(RequestFailure::public(404, "not found\n"));
        }
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect artifact storage: {err}"
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "artifact version storage is not a real directory",
        ));
    }
    let entry = read_entry_metadata(&version_root, &reference.name, &reference.version)?;
    if entry.checksum != reference.checksum {
        return Err(RequestFailure::public(404, "not found\n"));
    }
    let (file, length) = open_cached_verified_artifact(
        &state.artifact_verifications,
        &version_root,
        &entry,
        deadline,
    )?;
    Ok(HttpResponse {
        status: 200,
        content_type: ARTIFACT_CONTENT_TYPE,
        body: ResponseBody::File(file, length),
        artifact_checksum: Some(entry.checksum),
    })
}

fn parse_publish_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/v1/packages/")?;
    let (name, version) = rest.split_once('/')?;
    if version.contains('/')
        || validate_package_name(name, Span::default()).is_err()
        || parse_package_version(version, Span::default()).is_err()
    {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

fn parse_yank_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/v1/packages/")?;
    let (identity, action) = rest.rsplit_once('/')?;
    if action != "yank" {
        return None;
    }
    let (name, version) = identity.split_once('/')?;
    if version.contains('/')
        || validate_package_name(name, Span::default()).is_err()
        || parse_package_version(version, Span::default()).is_err()
    {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

fn parse_index_path(path: &str) -> Option<(String, bool)> {
    let rest = path.strip_prefix("/v1/packages/")?;
    let (name, leaf) = rest.split_once('/')?;
    let signature = match leaf {
        "index.toml" => false,
        "index.toml.sig" => true,
        _ => return None,
    };
    if validate_package_name(name, Span::default()).is_err() {
        return None;
    }
    Some((name.to_string(), signature))
}

fn parse_artifact_path(path: &str) -> Option<ArtifactReference> {
    let value = path
        .strip_prefix("/v1/artifacts/")?
        .strip_suffix(".tar.zst")?;
    let (identity, digest) = value.rsplit_once("-sha256-")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (name, version) = identity.rsplit_once('-')?;
    if validate_package_name(name, Span::default()).is_err()
        || parse_package_version(version, Span::default()).is_err()
    {
        return None;
    }
    Some(ArtifactReference {
        name: name.to_string(),
        version: version.to_string(),
        checksum: format!("sha256-{}", digest.to_ascii_lowercase()),
    })
}

#[derive(Debug, Clone)]
struct PublishCredential {
    token_hash: [u8; 32],
    package_name: String,
}

enum PublishAuthorization {
    Allowed,
    Invalid,
    Forbidden,
}

fn authorize_publish(
    credentials: &[PublishCredential],
    authorization: Option<&String>,
    package_name: &str,
) -> PublishAuthorization {
    let Some(token) = authorization.and_then(|value| value.strip_prefix("Bearer ")) else {
        return PublishAuthorization::Invalid;
    };
    if token.is_empty()
        || token.len() > MAX_PUBLISH_TOKEN_BYTES
        || !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return PublishAuthorization::Invalid;
    }
    let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let mut recognized = 0u8;
    let mut allowed = 0u8;
    for credential in credentials {
        let hash_matches = constant_time_equal(&presented, &credential.token_hash) as u8;
        recognized |= hash_matches;
        allowed |= hash_matches & u8::from(credential.package_name == package_name);
    }
    if allowed != 0 {
        PublishAuthorization::Allowed
    } else if recognized != 0 {
        PublishAuthorization::Forbidden
    } else {
        PublishAuthorization::Invalid
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn read_credentials(path: &Path) -> KuResult<Vec<PublishCredential>> {
    let bytes = read_bounded_regular_file(path, MAX_CREDENTIALS_FILE_BYTES, "credentials")?;
    let credentials = parse_credential_records(&bytes)?;
    if credentials.is_empty() {
        return Err(server_config_error(
            "invalid_registry_credentials",
            "registry credentials file must contain at least one active token-to-owned-package authorization",
        ));
    }
    Ok(credentials
        .into_iter()
        .map(|credential| PublishCredential {
            token_hash: credential.token_hash,
            package_name: credential.package_name,
        })
        .collect())
}

fn write_entry_metadata(path: &Path, metadata: &EntryMetadata) -> Result<(), RequestFailure> {
    let mut source = format!(
        "name = \"{}\"\nversion = \"{}\"\nchecksum = \"{}\"\n",
        metadata.name, metadata.version, metadata.checksum
    );
    for dependency in &metadata.dependencies {
        source.push_str(&format!(
            "dep.{} = \"{}\"\n",
            dependency.name, dependency.version
        ));
    }
    if source.len() > MAX_ENTRY_METADATA_BYTES as usize {
        return Err(RequestFailure::internal(
            "package metadata exceeds storage limit",
        ));
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| {
            RequestFailure::internal(format!("failed to create package metadata: {err}"))
        })?;
    file.write_all(source.as_bytes()).map_err(|err| {
        RequestFailure::internal(format!("failed to write package metadata: {err}"))
    })?;
    file.sync_all()
        .map_err(|err| RequestFailure::internal(format!("failed to sync package metadata: {err}")))
}

fn read_entry_metadata(
    version_root: &Path,
    expected_name: &str,
    expected_version: &str,
) -> Result<EntryMetadata, RequestFailure> {
    let path = version_root.join(ENTRY_METADATA_FILE);
    let bytes = read_bounded_regular_file(&path, MAX_ENTRY_METADATA_BYTES, "package metadata")
        .map_err(|err| RequestFailure::internal(err.to_string()))?;
    let source = std::str::from_utf8(&bytes)
        .map_err(|_| RequestFailure::internal("package metadata is not UTF-8"))?;
    let mut fields = HashMap::new();
    let mut dependencies = Vec::new();
    let mut dependency_names = HashSet::new();
    for raw_line in source.lines() {
        if raw_line.is_empty() || raw_line.len() > MAX_REGISTRY_LINE_BYTES {
            return Err(RequestFailure::internal("package metadata line is invalid"));
        }
        let Some((key, raw_value)) = raw_line.split_once(" = ") else {
            return Err(RequestFailure::internal(
                "package metadata syntax is invalid",
            ));
        };
        let value = parse_storage_string(raw_value)
            .ok_or_else(|| RequestFailure::internal("package metadata value is invalid"))?;
        if let Some(name) = key.strip_prefix("dep.") {
            validate_package_name(name, Span::default())
                .map_err(|_| RequestFailure::internal("stored dependency name is invalid"))?;
            parse_version_requirement(value, Span::default()).map_err(|_| {
                RequestFailure::internal("stored dependency requirement is invalid")
            })?;
            if dependencies.len() >= MAX_REGISTRY_DEPENDENCIES
                || !dependency_names.insert(name.to_string())
            {
                return Err(RequestFailure::internal(
                    "stored package dependencies are duplicated or exceed limits",
                ));
            }
            dependencies.push(PackageDependency {
                name: name.to_string(),
                version: value.to_string(),
                source: None,
                checksum: None,
            });
        } else if !matches!(key, "name" | "version" | "checksum")
            || fields.insert(key.to_string(), value.to_string()).is_some()
        {
            return Err(RequestFailure::internal(
                "stored package metadata has an unknown or duplicate field",
            ));
        }
    }
    let name = fields
        .remove("name")
        .ok_or_else(|| RequestFailure::internal("stored package metadata is missing name"))?;
    let version = fields
        .remove("version")
        .ok_or_else(|| RequestFailure::internal("stored package metadata is missing version"))?;
    let checksum = fields
        .remove("checksum")
        .ok_or_else(|| RequestFailure::internal("stored package metadata is missing checksum"))?;
    if !fields.is_empty() || name != expected_name || version != expected_version {
        return Err(RequestFailure::internal(
            "stored package metadata identity does not match its path",
        ));
    }
    validate_sha256_checksum(&checksum, Span::default())
        .map_err(|_| RequestFailure::internal("stored package checksum is invalid"))?;
    dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    validate_registry_package_dependencies(&dependencies)
        .map_err(|_| RequestFailure::internal("stored package dependencies are invalid"))?;
    Ok(EntryMetadata {
        name,
        version,
        checksum,
        dependencies,
    })
}

fn parse_storage_string(value: &str) -> Option<&str> {
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    (!value.contains(['"', '\\', '\r', '\n'])).then_some(value)
}

fn artifact_storage_name(checksum: &str) -> Result<String, RequestFailure> {
    validate_sha256_checksum(checksum, Span::default())
        .map_err(|_| RequestFailure::internal("artifact checksum is invalid"))?;
    Ok(format!("package-{checksum}.tar.zst"))
}

fn verify_committed_artifact(
    version_root: &Path,
    metadata: &EntryMetadata,
    deadline: Instant,
) -> Result<(), RequestFailure> {
    open_verified_committed_artifact(version_root, metadata, deadline).map(|_| ())
}

fn open_cached_verified_artifact(
    cache: &ArtifactVerificationCache,
    version_root: &Path,
    metadata: &EntryMetadata,
    deadline: Instant,
) -> Result<(fs::File, u64), RequestFailure> {
    let artifact_path = version_root.join(artifact_storage_name(&metadata.checksum)?);
    'retry: loop {
        ensure_request_deadline(deadline)?;
        let (mut file, fingerprint) = open_bounded_artifact(&artifact_path)?;
        let mut inner = cache
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        inner.clock = inner.clock.saturating_add(1);
        let used_at = inner.clock;

        if let Some(entry) = inner.entries.get_mut(&artifact_path) {
            match entry {
                ArtifactVerificationEntry::Verified {
                    fingerprint: cached,
                    used_at: cached_used_at,
                } if cached == &fingerprint => {
                    *cached_used_at = used_at;
                    return Ok((file, fingerprint.length));
                }
                ArtifactVerificationEntry::Failed {
                    fingerprint: cached,
                    message,
                    used_at: cached_used_at,
                } if cached == &fingerprint => {
                    *cached_used_at = used_at;
                    return Err(RequestFailure::internal(message.clone()));
                }
                ArtifactVerificationEntry::Verifying {
                    fingerprint: cached,
                } if cached == &fingerprint => {
                    drop(file);
                    inner = wait_for_artifact_verification(cache, inner, deadline)?;
                    drop(inner);
                    continue;
                }
                ArtifactVerificationEntry::Verifying { .. } => {
                    // A replacement arrived while the old inode is still being
                    // verified. Let that bounded leader finish before retrying;
                    // two leaders for one path would defeat single-flight.
                    drop(file);
                    inner = wait_for_artifact_verification(cache, inner, deadline)?;
                    drop(inner);
                    continue;
                }
                ArtifactVerificationEntry::Verified { .. }
                | ArtifactVerificationEntry::Failed { .. } => {
                    inner.entries.remove(&artifact_path);
                }
            }
        }

        while inner.entries.len() >= cache.capacity {
            let eviction = inner
                .entries
                .iter()
                .filter_map(|(path, entry)| match entry {
                    ArtifactVerificationEntry::Verified { used_at, .. }
                    | ArtifactVerificationEntry::Failed { used_at, .. } => {
                        Some((*used_at, path.clone()))
                    }
                    ArtifactVerificationEntry::Verifying { .. } => None,
                })
                .min_by(|left, right| left.cmp(right));
            if let Some((_, path)) = eviction {
                inner.entries.remove(&path);
            } else {
                drop(file);
                inner = wait_for_artifact_verification(cache, inner, deadline)?;
                drop(inner);
                continue 'retry;
            }
        }
        if inner.entries.len() >= cache.capacity {
            continue 'retry;
        }
        inner.entries.insert(
            artifact_path.clone(),
            ArtifactVerificationEntry::Verifying {
                fingerprint: fingerprint.clone(),
            },
        );
        drop(inner);

        #[cfg(test)]
        {
            cache.hash_count.fetch_add(1, Ordering::Relaxed);
            let delay = cache.hash_delay_ms.load(Ordering::Relaxed);
            if delay > 0 {
                thread::sleep(Duration::from_millis(delay));
            }
        }
        let verified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            if cache.fail_next_verification.swap(false, Ordering::Relaxed) {
                return Err(ArtifactVerificationFailure::Retryable(
                    RequestFailure::internal("injected transient artifact read failure"),
                ));
            }
            verify_opened_artifact(&mut file, &fingerprint, &metadata.checksum, deadline)
        }));
        let mut inner = cache
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let still_leader = matches!(
            inner.entries.get(&artifact_path),
            Some(ArtifactVerificationEntry::Verifying { fingerprint: cached })
                if cached == &fingerprint
        );
        if still_leader {
            inner.clock = inner.clock.saturating_add(1);
            let used_at = inner.clock;
            match &verified {
                Ok(Ok(())) => {
                    inner.entries.insert(
                        artifact_path.clone(),
                        ArtifactVerificationEntry::Verified {
                            fingerprint: fingerprint.clone(),
                            used_at,
                        },
                    );
                }
                Ok(Err(ArtifactVerificationFailure::ChecksumMismatch)) => {
                    inner.entries.insert(
                        artifact_path.clone(),
                        ArtifactVerificationEntry::Failed {
                            fingerprint: fingerprint.clone(),
                            message: ARTIFACT_CHECKSUM_MISMATCH.to_string(),
                            used_at,
                        },
                    );
                }
                Ok(Err(_)) | Err(_) => {
                    inner.entries.remove(&artifact_path);
                }
            }
        }
        drop(inner);
        cache.changed.notify_all();
        match verified {
            Ok(Ok(())) => return Ok((file, fingerprint.length)),
            Ok(Err(failure)) => return Err(failure.into_request_failure()),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

fn wait_for_artifact_verification<'a>(
    cache: &ArtifactVerificationCache,
    inner: std::sync::MutexGuard<'a, ArtifactVerificationCacheInner>,
    deadline: Instant,
) -> Result<std::sync::MutexGuard<'a, ArtifactVerificationCacheInner>, RequestFailure> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| RequestFailure::public(408, "request timeout\n"))?;
    let (inner, timeout) = cache
        .changed
        .wait_timeout(inner, remaining)
        .unwrap_or_else(|poison| poison.into_inner());
    if timeout.timed_out() || Instant::now() >= deadline {
        Err(RequestFailure::public(408, "request timeout\n"))
    } else {
        Ok(inner)
    }
}

fn open_bounded_artifact(
    artifact_path: &Path,
) -> Result<(fs::File, ArtifactFingerprint), RequestFailure> {
    let path_metadata = fs::symlink_metadata(artifact_path).map_err(|err| {
        RequestFailure::internal(format!("failed to inspect committed artifact: {err}"))
    })?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.len() == 0
        || path_metadata.len() > MAX_REGISTRY_DOWNLOAD_BYTES
    {
        return Err(RequestFailure::internal(
            "committed artifact is not a bounded regular file",
        ));
    }
    let file = open_artifact_nofollow(artifact_path).map_err(|err| {
        RequestFailure::internal(format!("failed to open committed artifact: {err}"))
    })?;
    let fingerprint = artifact_fingerprint(&file)?;
    if fingerprint.length != path_metadata.len() {
        return Err(RequestFailure::internal(
            "committed artifact changed while it was opened",
        ));
    }
    Ok((file, fingerprint))
}

#[cfg(unix)]
fn open_artifact_nofollow(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_artifact_nofollow(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

const ARTIFACT_CHECKSUM_MISMATCH: &str =
    "committed artifact checksum does not match package metadata";

enum ArtifactVerificationFailure {
    ChecksumMismatch,
    Retryable(RequestFailure),
}

impl ArtifactVerificationFailure {
    fn into_request_failure(self) -> RequestFailure {
        match self {
            Self::ChecksumMismatch => RequestFailure::internal(ARTIFACT_CHECKSUM_MISMATCH),
            Self::Retryable(failure) => failure,
        }
    }
}

impl From<RequestFailure> for ArtifactVerificationFailure {
    fn from(failure: RequestFailure) -> Self {
        Self::Retryable(failure)
    }
}

fn verify_opened_artifact(
    file: &mut fs::File,
    initial: &ArtifactFingerprint,
    expected_checksum: &str,
    deadline: Instant,
) -> Result<(), ArtifactVerificationFailure> {
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_request_deadline(deadline)?;
        let count = file.read(&mut buffer).map_err(|err| {
            RequestFailure::internal(format!("failed to verify committed artifact: {err}"))
        })?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > MAX_REGISTRY_DOWNLOAD_BYTES || total > initial.length {
            return Err(RequestFailure::internal(
                "committed artifact changed while it was verified",
            )
            .into());
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("sha256-{:x}", hasher.finalize());
    let after = artifact_fingerprint(file)?;
    if total != initial.length || &after != initial {
        return Err(
            RequestFailure::internal("committed artifact changed while it was verified").into(),
        );
    }
    // Only a complete read of an unchanged file proves deterministic corruption.
    // I/O errors and identity changes must leave the next request free to retry.
    if !constant_time_equal(actual.as_bytes(), expected_checksum.as_bytes()) {
        return Err(ArtifactVerificationFailure::ChecksumMismatch);
    }
    file.seek(SeekFrom::Start(0)).map_err(|err| {
        RequestFailure::internal(format!("failed to rewind verified artifact: {err}"))
    })?;
    Ok(())
}

#[cfg(unix)]
fn artifact_fingerprint(file: &fs::File) -> Result<ArtifactFingerprint, RequestFailure> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|err| {
        RequestFailure::internal(format!("failed to inspect opened artifact: {err}"))
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_REGISTRY_DOWNLOAD_BYTES {
        return Err(RequestFailure::internal(
            "committed artifact is not a bounded regular file",
        ));
    }
    Ok(ArtifactFingerprint {
        identity_high: metadata.dev(),
        identity_low: metadata.ino(),
        length: metadata.len(),
        modified: metadata.modified().ok(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn artifact_fingerprint(file: &fs::File) -> Result<ArtifactFingerprint, RequestFailure> {
    use std::{
        mem::MaybeUninit,
        os::windows::io::{AsRawHandle, RawHandle},
    };

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: RawHandle, information: *mut FileInformation) -> i32;
    }

    let metadata = file.metadata().map_err(|err| {
        RequestFailure::internal(format!("failed to inspect opened artifact: {err}"))
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_REGISTRY_DOWNLOAD_BYTES {
        return Err(RequestFailure::internal(
            "committed artifact is not a bounded regular file",
        ));
    }
    let mut information = MaybeUninit::<FileInformation>::uninit();
    // SAFETY: Windows initializes the complete structure on nonzero return and
    // the borrowed file owns a valid handle for the duration of this call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(RequestFailure::internal(format!(
            "failed to identify opened artifact: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: the preceding call returned success.
    let information = unsafe { information.assume_init() };
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if information.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(RequestFailure::internal(
            "committed artifact must not be a Windows reparse point",
        ));
    }
    let last_write = (u64::from(information.last_write_time.high) << 32)
        | u64::from(information.last_write_time.low);
    Ok(ArtifactFingerprint {
        identity_high: u64::from(information.volume_serial_number),
        identity_low: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
        length: metadata.len(),
        modified: metadata.modified().ok(),
        changed_seconds: last_write as i64,
        changed_nanos: 0,
    })
}

fn open_verified_committed_artifact(
    version_root: &Path,
    metadata: &EntryMetadata,
    deadline: Instant,
) -> Result<(fs::File, u64), RequestFailure> {
    let artifact_path = version_root.join(artifact_storage_name(&metadata.checksum)?);
    let file_metadata = fs::symlink_metadata(&artifact_path).map_err(|err| {
        RequestFailure::internal(format!("failed to inspect committed artifact: {err}"))
    })?;
    if file_metadata.file_type().is_symlink()
        || !file_metadata.is_file()
        || file_metadata.len() == 0
        || file_metadata.len() > MAX_REGISTRY_DOWNLOAD_BYTES
    {
        return Err(RequestFailure::internal(
            "committed artifact is not a bounded regular file",
        ));
    }
    let mut file = fs::File::open(&artifact_path).map_err(|err| {
        RequestFailure::internal(format!("failed to open committed artifact: {err}"))
    })?;
    let opened_metadata = file.metadata().map_err(|err| {
        RequestFailure::internal(format!("failed to inspect opened artifact: {err}"))
    })?;
    if !opened_metadata.is_file() || opened_metadata.len() != file_metadata.len() {
        return Err(RequestFailure::internal(
            "committed artifact changed while it was opened",
        ));
    }
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_request_deadline(deadline)?;
        let count = file.read(&mut buffer).map_err(|err| {
            RequestFailure::internal(format!("failed to verify committed artifact: {err}"))
        })?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > MAX_REGISTRY_DOWNLOAD_BYTES || total > file_metadata.len() {
            return Err(RequestFailure::internal(
                "committed artifact changed while it was verified",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("sha256-{:x}", hasher.finalize());
    if total != file_metadata.len()
        || !constant_time_equal(actual.as_bytes(), metadata.checksum.as_bytes())
    {
        return Err(RequestFailure::internal(
            "committed artifact checksum does not match package metadata",
        ));
    }
    file.seek(SeekFrom::Start(0)).map_err(|err| {
        RequestFailure::internal(format!("failed to rewind verified artifact: {err}"))
    })?;
    let reopened_metadata = file.metadata().map_err(|err| {
        RequestFailure::internal(format!("failed to re-inspect verified artifact: {err}"))
    })?;
    if reopened_metadata.len() != file_metadata.len() {
        return Err(RequestFailure::internal(
            "committed artifact changed after checksum verification",
        ));
    }
    Ok((file, file_metadata.len()))
}

fn write_http_response(
    writer: &mut impl Write,
    response: HttpResponse,
    deadline: Instant,
    remaining_requests: usize,
) -> io::Result<()> {
    let content_length = match &response.body {
        ResponseBody::Empty => 0,
        ResponseBody::Bytes(bytes) => bytes.len() as u64,
        ResponseBody::File(_, length) => *length,
    };
    ensure_io_deadline(deadline)?;
    write!(
        writer,
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: {}\r\n",
        response.status,
        status_reason(response.status),
        content_length,
        response.content_type,
        if remaining_requests > 0 {
            "keep-alive"
        } else {
            "close"
        }
    )?;
    if remaining_requests > 0 {
        write!(
            writer,
            "Keep-Alive: timeout={}, max={}\r\n",
            KEEP_ALIVE_IDLE_TIMEOUT.as_secs(),
            remaining_requests
        )?;
    }
    if let Some(checksum) = response.artifact_checksum.as_deref() {
        write!(
            writer,
            "ETag: \"{}\"\r\nCache-Control: public, max-age=31536000, immutable\r\n",
            checksum
        )?;
    }
    writer.write_all(b"\r\n")?;
    match response.body {
        ResponseBody::Empty => {}
        ResponseBody::Bytes(bytes) => writer.write_all(&bytes)?,
        ResponseBody::File(mut file, expected) => {
            let mut remaining = expected;
            let mut buffer = [0u8; 64 * 1024];
            while remaining > 0 {
                ensure_io_deadline(deadline)?;
                let read_len = remaining.min(buffer.len() as u64) as usize;
                let count = file.read(&mut buffer[..read_len])?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "registry artifact changed while it was served",
                    ));
                }
                writer.write_all(&buffer[..count])?;
                remaining -= count as u64;
            }
        }
    }
    writer.flush()
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn ensure_request_deadline(deadline: Instant) -> Result<(), RequestFailure> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(RequestFailure::public(408, "request timed out\n"))
    }
}

fn log_maintenance_failure(action: &str, failure: &RequestFailure) {
    if let Some(internal) = &failure.internal {
        eprintln!("ku-registry could not {action}: {internal}");
    } else {
        eprintln!("ku-registry could not {action}: status {}", failure.status);
    }
}

fn ensure_io_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "registry request deadline exceeded",
        ))
    }
}

fn create_staging_directory(staging_root: &Path) -> Result<PathBuf, RequestFailure> {
    for _ in 0..16 {
        let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = staging_root.join(format!("upload-{}-{id}-{nanos}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(RequestFailure::internal(format!(
                    "failed to create registry staging directory: {err}"
                )));
            }
        }
    }
    Err(RequestFailure::internal(
        "failed to allocate a unique registry staging directory",
    ))
}

struct TemporaryDirectory {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TemporaryDirectory {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: true,
        }
    }

    fn keep(&mut self) {
        self.remove_on_drop = false;
    }
}

struct MutationAdmission {
    active_packages: Mutex<HashSet<String>>,
}

impl MutationAdmission {
    fn new() -> Self {
        Self {
            active_packages: Mutex::new(HashSet::with_capacity(MAX_ACTIVE_MUTATIONS)),
        }
    }

    fn try_acquire<'a>(
        &'a self,
        package_name: &'a str,
    ) -> Result<MutationGuard<'a>, RequestFailure> {
        // Names have already passed request path validation. The same bounded
        // set enforces both per-package exclusion and the global mutation cap;
        // its mutex is never held across file, archive, or network operations.
        let mut active = self
            .active_packages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.contains(package_name) {
            return Err(RequestFailure::public(
                429,
                "package mutation already active\n",
            ));
        }
        if active.len() >= MAX_ACTIVE_MUTATIONS {
            return Err(RequestFailure::public(429, "too many active mutations\n"));
        }
        active.insert(package_name.to_string());
        Ok(MutationGuard {
            admission: self,
            package_name,
        })
    }
}

struct MutationGuard<'a> {
    admission: &'a MutationAdmission,
    package_name: &'a str,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        self.admission
            .active_packages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(self.package_name);
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct PackagePublishLock {
    file: fs::File,
}

impl PackagePublishLock {
    fn open(lock_root: &Path, package_name: &str) -> Result<fs::File, RequestFailure> {
        validate_package_name(package_name, Span::default())
            .map_err(|_| RequestFailure::public(404, "not found\n"))?;
        let path = lock_root.join(format!("{package_name}.lock"));
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(RequestFailure::internal(
                    "package publish lock is not a real file",
                ));
            }
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                RequestFailure::internal(format!("failed to open package publish lock: {err}"))
            })?;
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            RequestFailure::internal(format!("failed to inspect package publish lock: {err}"))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(RequestFailure::internal(
                "package publish lock is not a real file",
            ));
        }
        Ok(file)
    }

    fn try_acquire(
        lock_root: &Path,
        package_name: &str,
        deadline: Instant,
    ) -> Result<Self, RequestFailure> {
        ensure_request_deadline(deadline)?;
        let file = Self::open(lock_root, package_name)?;
        ensure_request_deadline(deadline)?;
        let result = file.try_lock();
        ensure_request_deadline(deadline)?;
        match result {
            Ok(()) => Ok(Self { file }),
            Err(fs::TryLockError::WouldBlock) => Err(RequestFailure::public(
                429,
                "package mutation already active\n",
            )),
            Err(fs::TryLockError::Error(err)) => Err(RequestFailure::internal(format!(
                "failed to acquire package publish lock: {err}"
            ))),
        }
    }

    fn acquire(
        lock_root: &Path,
        package_name: &str,
        deadline: Instant,
    ) -> Result<Self, RequestFailure> {
        let check_deadline = || {
            if Instant::now() >= deadline {
                Err(RequestFailure::public(503, "publish lock timed out\n"))
            } else {
                Ok(())
            }
        };
        check_deadline()?;
        let file = Self::open(lock_root, package_name)?;
        loop {
            check_deadline()?;
            let result = file.try_lock();
            check_deadline()?;
            match result {
                Ok(()) => return Ok(Self { file }),
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    thread::sleep(remaining.min(Duration::from_millis(LOCK_POLL_MS)));
                }
                Err(fs::TryLockError::Error(err)) => {
                    return Err(RequestFailure::internal(format!(
                        "failed to acquire package publish lock: {err}"
                    )));
                }
            }
        }
    }
}

impl Drop for PackagePublishLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn package_storage_root(
    storage: &RegistryStorage,
    package_name: &str,
) -> Result<PathBuf, RequestFailure> {
    validate_package_name(package_name, Span::default())
        .map_err(|_| RequestFailure::public(404, "not found\n"))?;
    let path = storage.packages.join(package_name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to create package storage: {err}"
            )));
        }
    }
    let metadata = fs::symlink_metadata(&path).map_err(|err| {
        RequestFailure::internal(format!("failed to inspect package storage: {err}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "package storage is not a real directory",
        ));
    }
    let canonical = fs::canonicalize(&path).map_err(|err| {
        RequestFailure::internal(format!("failed to resolve package storage: {err}"))
    })?;
    if canonical.parent() != Some(storage.packages.as_path()) {
        return Err(RequestFailure::internal(
            "package storage escaped the registry data root",
        ));
    }
    // On Unix, persisting a newly created directory requires syncing the
    // parent directory entry, not only later files inside the child. Re-sync
    // on the existing path as well so a retry after an interrupted first
    // publish closes the same durability gap. Windows directory fsync is not
    // exposed portably by std and `sync_directory` is deliberately best effort.
    sync_directory(&storage.packages).map_err(|err| {
        RequestFailure::internal(format!("failed to sync package storage parent: {err}"))
    })?;
    Ok(canonical)
}

fn existing_package_storage_root(
    storage: &RegistryStorage,
    package_name: &str,
) -> Result<Option<PathBuf>, RequestFailure> {
    validate_package_name(package_name, Span::default())
        .map_err(|_| RequestFailure::public(404, "not found\n"))?;
    let path = storage.packages.join(package_name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(RequestFailure::internal(format!(
                "failed to inspect package storage: {err}"
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestFailure::internal(
            "package storage is not a real directory",
        ));
    }
    let canonical = fs::canonicalize(&path).map_err(|err| {
        RequestFailure::internal(format!("failed to resolve package storage: {err}"))
    })?;
    if canonical.parent() != Some(storage.packages.as_path()) {
        return Err(RequestFailure::internal(
            "package storage escaped the registry data root",
        ));
    }
    Ok(Some(canonical))
}

fn ensure_real_directory(path: &Path) -> KuResult<()> {
    match fs::create_dir_all(path) {
        Ok(()) => {}
        Err(err) => {
            return Err(server_error(
                "registry_storage_failed",
                format!(
                    "failed to create registry directory '{}': {err}",
                    path.display()
                ),
            ));
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        server_error(
            "registry_storage_failed",
            format!(
                "failed to inspect registry directory '{}': {err}",
                path.display()
            ),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(server_error(
            "unsafe_registry_storage",
            format!(
                "registry path '{}' must be a real directory",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn ensure_real_child_directory(root: &Path, child: &str) -> KuResult<PathBuf> {
    let path = root.join(child);
    ensure_real_directory(&path)?;
    let canonical = fs::canonicalize(&path).map_err(|err| {
        server_error(
            "registry_storage_failed",
            format!("failed to resolve registry storage directory: {err}"),
        )
    })?;
    if canonical.parent() != Some(root) {
        return Err(server_error(
            "unsafe_registry_storage",
            "registry storage directory escaped its data root",
        ));
    }
    Ok(canonical)
}

fn cleanup_stale_staging(staging_root: &Path) -> KuResult<()> {
    let canonical_root = fs::canonicalize(staging_root).map_err(|err| {
        server_error(
            "registry_storage_failed",
            format!("failed to resolve registry staging directory: {err}"),
        )
    })?;
    let entries = fs::read_dir(staging_root).map_err(|err| {
        server_error(
            "registry_storage_failed",
            format!("failed to read registry staging directory: {err}"),
        )
    })?;
    let mut entries_seen = 0usize;
    for entry in entries {
        entries_seen += 1;
        if entries_seen > MAX_STAGING_SCAN_ENTRIES {
            return Err(server_error(
                "registry_storage_limit",
                "registry staging directory exceeds its startup scan limit",
            ));
        }
        let entry = entry.map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to read registry staging entry: {err}"),
            )
        })?;
        let name = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                server_error(
                    "unsafe_registry_storage",
                    "registry staging contains a non-UTF-8 entry",
                )
            })?;
        if !valid_staging_name(&name) {
            return Err(server_error(
                "unsafe_registry_storage",
                "registry staging contains an unexpected entry",
            ));
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(server_error(
                    "registry_storage_failed",
                    format!("failed to inspect registry staging entry: {err}"),
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(server_error(
                "unsafe_registry_storage",
                "registry staging entries must be real directories",
            ));
        }
        if !metadata
            .modified()
            .and_then(|modified| modified.elapsed().map_err(io::Error::other))
            .is_ok_and(|age| age >= STAGING_STALE_AFTER)
        {
            continue;
        }
        if !bounded_plain_tree(&path)? {
            return Err(server_error(
                "unsafe_registry_storage",
                "stale registry staging tree exceeds its cleanup safety limits",
            ));
        }
        let canonical = fs::canonicalize(&path).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to resolve registry staging entry: {err}"),
            )
        })?;
        if canonical.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        let current = fs::symlink_metadata(&path).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to re-inspect registry staging entry: {err}"),
            )
        })?;
        if current.file_type().is_symlink() || !current.is_dir() {
            continue;
        }
        fs::remove_dir_all(&path).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to remove stale registry staging entry: {err}"),
            )
        })?;
    }
    Ok(())
}

fn valid_staging_name(name: &str) -> bool {
    let Some(value) = name.strip_prefix("upload-") else {
        return false;
    };
    let mut parts = value.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(process), Some(counter), Some(nanos), None)
            if process.parse::<u32>().is_ok()
                && counter.parse::<u64>().is_ok()
                && nanos.parse::<u128>().is_ok()
    )
}

fn bounded_plain_tree(root: &Path) -> KuResult<bool> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some((directory, depth)) = stack.pop() {
        if depth > 34 {
            return Ok(false);
        }
        for entry in fs::read_dir(&directory).map_err(|err| {
            server_error(
                "registry_storage_failed",
                format!("failed to inspect stale staging tree: {err}"),
            )
        })? {
            let entry = entry.map_err(|err| {
                server_error(
                    "registry_storage_failed",
                    format!("failed to inspect stale staging entry: {err}"),
                )
            })?;
            entries += 1;
            if entries > MAX_STAGING_TREE_ENTRIES {
                return Ok(false);
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|err| {
                server_error(
                    "registry_storage_failed",
                    format!("failed to inspect stale staging metadata: {err}"),
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            if metadata.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
                if bytes > MAX_STAGING_TREE_BYTES {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn validate_server_config(config: &RegistryServerConfig) -> KuResult<()> {
    if config.workers == 0
        || config.workers > MAX_WORKERS
        || config.queue_capacity == 0
        || config.queue_capacity > MAX_QUEUE_CAPACITY
        || config.request_timeout < Duration::from_millis(100)
        || config.request_timeout > Duration::from_millis(MAX_REQUEST_TIMEOUT_MS)
    {
        return Err(server_config_error(
            "invalid_registry_config",
            "registry workers, queue capacity, or request timeout is outside its hard limit",
        ));
    }
    Ok(())
}

fn required_path_env(name: &str) -> KuResult<PathBuf> {
    env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        server_config_error(
            "missing_registry_config",
            format!("required registry environment variable {name} is not set"),
        )
    })
}

fn bounded_usize_env(
    name: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> KuResult<usize> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value.to_str().ok_or_else(|| {
        server_config_error(
            "invalid_registry_config",
            format!("{name} must be a decimal integer"),
        )
    })?;
    let parsed = value.parse::<usize>().map_err(|_| {
        server_config_error(
            "invalid_registry_config",
            format!("{name} must be a decimal integer"),
        )
    })?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(server_config_error(
            "invalid_registry_config",
            format!("{name} must be between {minimum} and {maximum}"),
        ));
    }
    Ok(parsed)
}

fn bounded_u64_env(name: &str, default: u64, minimum: u64, maximum: u64) -> KuResult<u64> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value.to_str().ok_or_else(|| {
        server_config_error(
            "invalid_registry_config",
            format!("{name} must be a decimal integer"),
        )
    })?;
    let parsed = value.parse::<u64>().map_err(|_| {
        server_config_error(
            "invalid_registry_config",
            format!("{name} must be a decimal integer"),
        )
    })?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(server_config_error(
            "invalid_registry_config",
            format!("{name} must be between {minimum} and {maximum}"),
        ));
    }
    Ok(parsed)
}

fn read_signing_key(path: &Path) -> KuResult<SigningKey> {
    let bytes = read_bounded_regular_file(path, MAX_SECRET_FILE_BYTES, "signing key")?;
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| {
            server_config_error(
                "invalid_registry_signing_key",
                "registry signing key file must be valid UTF-8",
            )
        })?
        .trim_end_matches(['\r', '\n']);
    let Some(seed) = value.strip_prefix("ed25519-") else {
        return Err(server_config_error(
            "invalid_registry_signing_key",
            "registry signing key must be ed25519- followed by 64 hex digits",
        ));
    };
    let seed = decode_hex_array::<32>(seed).map_err(|_| {
        server_config_error(
            "invalid_registry_signing_key",
            "registry signing key must be ed25519- followed by 64 hex digits",
        )
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

fn load_tls_config(cert_path: &Path, key_path: &Path) -> KuResult<ServerConfig> {
    let cert_pem = read_bounded_regular_file(cert_path, MAX_TLS_FILE_BYTES, "TLS certificate")?;
    let key_pem = Zeroizing::new(read_bounded_regular_file(
        key_path,
        MAX_TLS_FILE_BYTES,
        "TLS private key",
    )?);
    let certificates = parse_tls_certificates(&cert_pem)?;
    if certificates.is_empty() {
        return Err(server_config_error(
            "invalid_registry_tls",
            "registry TLS certificate file contains no certificates",
        ));
    }
    let key = parse_tls_private_key(&key_pem)?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|_| {
            server_config_error(
                "invalid_registry_tls",
                "registry TLS certificate and private key do not match",
            )
        })
}

fn parse_tls_certificates(pem: &[u8]) -> KuResult<Vec<CertificateDer<'static>>> {
    const CERTIFICATE_BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    let mut remaining = pem;
    let mut certificates = Vec::new();

    loop {
        remaining = trim_ascii_whitespace_start(remaining);
        if remaining.is_empty() {
            return Ok(certificates);
        }
        if !remaining.starts_with(CERTIFICATE_BEGIN) {
            return Err(invalid_tls_pem(
                "registry TLS certificate file must contain only PEM certificates",
            ));
        }
        let ((kind, der), rest) = parse_one_pem_section(remaining)
            .map_err(|()| invalid_tls_pem("registry TLS certificate file is not valid PEM"))?;
        if kind != SectionKind::Certificate {
            return Err(invalid_tls_pem(
                "registry TLS certificate file must contain only PEM certificates",
            ));
        }
        certificates.push(CertificateDer::from(der));
        remaining = rest;
    }
}

fn parse_tls_private_key(pem: &[u8]) -> KuResult<PrivateKeyDer<'static>> {
    const PRIVATE_KEY_BEGINS: [&[u8]; 3] = [
        b"-----BEGIN PRIVATE KEY-----",
        b"-----BEGIN RSA PRIVATE KEY-----",
        b"-----BEGIN EC PRIVATE KEY-----",
    ];
    let remaining = trim_ascii_whitespace_start(pem);
    if remaining.is_empty() {
        return Err(invalid_tls_pem(
            "registry TLS private key file contains no private key",
        ));
    }
    if !PRIVATE_KEY_BEGINS
        .iter()
        .any(|begin| remaining.starts_with(begin))
    {
        return Err(invalid_tls_pem(
            "registry TLS private key file must contain exactly one supported PEM private key",
        ));
    }
    let ((kind, der), rest) = parse_one_pem_section(remaining)
        .map_err(|()| invalid_tls_pem("registry TLS private key file is not valid PEM"))?;
    if !trim_ascii_whitespace_start(rest).is_empty() {
        return Err(invalid_tls_pem(
            "registry TLS private key file must contain exactly one supported PEM private key",
        ));
    }
    PrivateKeyDer::from_pem(kind, der).ok_or_else(|| {
        invalid_tls_pem(
            "registry TLS private key file must contain exactly one supported PEM private key",
        )
    })
}

type ParsedPemSection<'a> = ((SectionKind, Vec<u8>), &'a [u8]);

fn parse_one_pem_section(input: &[u8]) -> Result<ParsedPemSection<'_>, ()> {
    let before = input.len();
    let mut sections = <(SectionKind, Vec<u8>)>::pem_slice_iter(input);
    let section = sections.next().ok_or(())?.map_err(|_| ())?;
    let rest = sections.remainder();
    if rest.len() >= before {
        return Err(());
    }
    Ok((section, rest))
}

fn trim_ascii_whitespace_start(input: &[u8]) -> &[u8] {
    let whitespace = input
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    &input[whitespace..]
}

fn invalid_tls_pem(message: &'static str) -> KuError {
    server_config_error("invalid_registry_tls", message)
}

fn open_bounded_regular_file(path: &Path, max_bytes: u64, kind: &str) -> KuResult<fs::File> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        server_config_error(
            "invalid_registry_file",
            format!("failed to inspect registry {kind} file: {err}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Err(server_config_error(
            "invalid_registry_file",
            format!("registry {kind} must be a regular file no larger than {max_bytes} bytes"),
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|err| {
        server_config_error(
            "invalid_registry_file",
            format!("failed to open registry {kind} file: {err}"),
        )
    })?;
    let opened = file.metadata().map_err(|err| {
        server_config_error(
            "invalid_registry_file",
            format!("failed to inspect opened registry {kind} file: {err}"),
        )
    })?;
    #[cfg(windows)]
    let opened_is_reparse = {
        use std::os::windows::fs::MetadataExt;
        opened.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    };
    #[cfg(not(windows))]
    let opened_is_reparse = false;
    if opened_is_reparse || !opened.is_file() || opened.len() > max_bytes {
        return Err(server_config_error(
            "invalid_registry_file",
            format!("registry {kind} must remain a regular file no larger than {max_bytes} bytes"),
        ));
    }
    Ok(file)
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64, kind: &str) -> KuResult<Vec<u8>> {
    let file = open_bounded_regular_file(path, max_bytes, kind)?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| {
            server_config_error(
                "invalid_registry_file",
                format!("failed to read registry {kind} file: {err}"),
            )
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(server_config_error(
            "invalid_registry_file",
            format!("registry {kind} file exceeds {max_bytes} bytes"),
        ));
    }
    Ok(bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex_array<const N: usize>(value: &str) -> Result<[u8; N], ()> {
    if value.len() != N * 2 {
        return Err(());
    }
    let mut output = [0u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (decode_hex_digit(pair[0])? << 4) | decode_hex_digit(pair[1])?;
    }
    Ok(output)
}

fn decode_hex_digit(value: u8) -> Result<u8, ()> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn server_config_error(code: &str, message: impl Into<String>) -> KuError {
    KuError::package(code, message.into(), Span::default())
}

fn server_error(code: &str, message: impl Into<String>) -> KuError {
    KuError::package(code, message.into(), Span::default())
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/support/native_pg_harness.rs"]
mod native_test_harness;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run_cli;
    use crate::package::{
        discover_from_dir, pack_package, publish_package, resolve_remote_dependencies_with_mode,
        write_lock, yank_package, DependencyResolveMode, PackageArtifact,
        TestRegistryHttpAgentGuard,
    };
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::{pki_types::ServerName, ClientConnection};
    use std::{net::Shutdown, process::Command, sync::Barrier};
    use ureq::rustls::{ClientConfig, RootCertStore};

    use super::native_test_harness::{compile_harness, run_bounded, RUN_LIMITS, RUN_TIMEOUT};

    const TEST_HTTP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);
    const MAX_TEST_HTTP_RESPONSE_HEADER_BYTES: usize = MAX_REQUEST_HEADER_BYTES;
    const MAX_TEST_HTTP_RESPONSE_HEADERS: usize = MAX_REQUEST_HEADERS;
    const MAX_TEST_HTTP_RESPONSE_BODY_BYTES: usize = MAX_REGISTRY_DOWNLOAD_BYTES as usize;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = env::temp_dir().join(format!("ku-registry-{label}-{}", staging_nonce()));
            fs::create_dir_all(&path).expect("create registry test root");
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TestServerFiles {
        config: RegistryServerConfig,
        certificate: rustls::pki_types::CertificateDer<'static>,
    }

    struct TestServer {
        base_url: String,
        public_key: String,
        agent: ureq::Agent,
        state: std::sync::Weak<RegistryState>,
        accepted_connections: Arc<AtomicUsize>,
        artifact_verifications: Arc<ArtifactVerificationCache>,
        shutdown: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<KuResult<()>>>,
    }

    impl TestServer {
        fn start(
            config: RegistryServerConfig,
            certificate: rustls::pki_types::CertificateDer<'static>,
        ) -> Self {
            let server = RegistryServer::bind(config).expect("bind real registry server");
            let address = server.local_addr().expect("read registry address");
            let public_key = server.public_key();
            let accepted_connections = server.accepted_connection_counter();
            let artifact_verifications = Arc::clone(&server.state.artifact_verifications);
            let state = Arc::downgrade(&server.state);
            let mut roots = RootCertStore::empty();
            roots
                .add(certificate)
                .expect("trust registry test certificate");
            let client_config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let agent = ureq::AgentBuilder::new()
                .tls_config(Arc::new(client_config))
                .timeout_connect(Duration::from_secs(2))
                .timeout(Duration::from_secs(8))
                .timeout_read(Duration::from_secs(8))
                .redirects(0)
                .build();
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_shutdown = Arc::clone(&shutdown);
            let thread = thread::spawn(move || server.serve_until(thread_shutdown));
            Self {
                base_url: format!("https://localhost:{}/v1/", address.port()),
                public_key,
                agent,
                state,
                accepted_connections,
                artifact_verifications,
                shutdown,
                thread: Some(thread),
            }
        }

        fn stop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !thread.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                assert!(
                    thread.is_finished(),
                    "registry server did not stop within 10 seconds"
                );
                thread
                    .join()
                    .expect("registry server thread panicked")
                    .expect("registry server stopped with an error");
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn test_server_files(root: &Path, acl: &[(&str, &str)], timeout: Duration) -> TestServerFiles {
        let data_dir = root.join("data");
        fs::create_dir_all(&data_dir).expect("create registry data directory");
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("generate registry TLS certificate");
        let certificate = cert.der().clone();
        let cert_path = root.join("tls-cert.pem");
        let key_path = root.join("tls-key.pem");
        let signing_path = root.join("signing.key");
        let credentials_path = root.join("credentials.txt");
        fs::write(&cert_path, cert.pem()).expect("write registry TLS certificate");
        fs::write(&key_path, key_pair.serialize_pem()).expect("write registry TLS key");
        fs::write(&signing_path, format!("ed25519-{}\n", "11".repeat(32)))
            .expect("write registry signing seed");
        let credentials = acl
            .iter()
            .map(|(token, package)| {
                format!(
                    "sha256-{} {package}\n",
                    encode_hex(&Sha256::digest(token.as_bytes()))
                )
            })
            .collect::<String>();
        fs::write(&credentials_path, credentials).expect("write registry credentials");
        TestServerFiles {
            config: RegistryServerConfig {
                bind: "127.0.0.1:0".parse().expect("parse test bind"),
                data_dir,
                credentials_file: credentials_path,
                signing_key_file: signing_path,
                tls_cert_file: cert_path,
                tls_key_file: key_path,
                workers: 4,
                queue_capacity: 8,
                request_timeout: timeout,
            },
            certificate,
        }
    }

    #[test]
    fn empty_credentials_file_remains_an_invalid_server_configuration() {
        let root = TestRoot::new("empty-credentials");
        let path = root.0.join("credentials.txt");
        fs::write(&path, b"# no active credentials\n").expect("write empty credential ACL");
        let error = read_credentials(&path).expect_err("server must require one credential");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_credentials"));
        assert!(error.message.contains("at least one"));
    }

    #[test]
    fn registry_tls_pem_loader_accepts_only_a_certificate_chain_and_one_private_key() {
        let root = TestRoot::new("strict-tls-pem");
        let files = test_server_files(&root.0, &[("token", "demo")], Duration::from_secs(1));
        let cert_path = &files.config.tls_cert_file;
        let key_path = &files.config.tls_key_file;
        let certificate = fs::read(cert_path).expect("read valid TLS certificate PEM");
        let private_key = fs::read(key_path).expect("read valid TLS private key PEM");

        load_tls_config(cert_path, key_path).expect("accept one certificate and one private key");
        let certificate_chain = [certificate.as_slice(), b"\r\n", certificate.as_slice()].concat();
        fs::write(cert_path, &certificate_chain).expect("write TLS certificate chain");
        load_tls_config(cert_path, key_path).expect("accept a certificate-only chain");

        let rejected = [
            (Vec::new(), private_key.clone(), "empty certificate input"),
            (
                [certificate.as_slice(), b"garbage"].concat(),
                private_key.clone(),
                "certificate trailing garbage",
            ),
            (
                [certificate.as_slice(), private_key.as_slice()].concat(),
                private_key.clone(),
                "mixed certificate PEM",
            ),
            (certificate.clone(), Vec::new(), "empty private key input"),
            (
                certificate.clone(),
                [private_key.as_slice(), private_key.as_slice()].concat(),
                "multiple private keys",
            ),
            (
                certificate.clone(),
                [private_key.as_slice(), b"garbage"].concat(),
                "private key trailing garbage",
            ),
            (
                certificate.clone(),
                [certificate.as_slice(), private_key.as_slice()].concat(),
                "mixed private key PEM",
            ),
        ];
        for (certificate_input, key_input, label) in rejected {
            fs::write(cert_path, certificate_input).expect("write rejected TLS certificate case");
            fs::write(key_path, key_input).expect("write rejected TLS private key case");
            let error = load_tls_config(cert_path, key_path)
                .expect_err("strict TLS PEM loader must reject malformed structure");
            assert_eq!(
                error.code.as_deref(),
                Some("invalid_registry_tls"),
                "{label}"
            );
        }
    }

    #[test]
    fn bounded_registry_file_reader_rejects_nonregular_oversized_and_symlink_paths() {
        let root = TestRoot::new("bounded-file-reader");
        let regular = root.0.join("regular.txt");
        fs::write(&regular, b"1234").expect("write bounded regular file");
        assert_eq!(
            read_bounded_regular_file(&regular, 4, "test").expect("read bounded regular file"),
            b"1234"
        );
        assert_eq!(
            read_bounded_regular_file(&regular, u64::MAX, "test")
                .expect("maximum byte limit must not overflow"),
            b"1234"
        );

        let oversized = root.0.join("oversized.txt");
        fs::write(&oversized, b"12345").expect("write oversized file");
        let error = read_bounded_regular_file(&oversized, 4, "test")
            .expect_err("oversized registry file must be rejected");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_file"));

        let directory = root.0.join("directory");
        fs::create_dir(&directory).expect("create non-file registry path");
        let error = read_bounded_regular_file(&directory, 4, "test")
            .expect_err("directory registry path must be rejected");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_file"));

        let symlink = root.0.join("regular-link.txt");
        #[cfg(unix)]
        let symlink_result = std::os::unix::fs::symlink(&regular, &symlink);
        #[cfg(windows)]
        let symlink_result = std::os::windows::fs::symlink_file(&regular, &symlink);
        #[cfg(not(any(unix, windows)))]
        let symlink_result = Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlink probe is unavailable on this platform",
        ));
        match symlink_result {
            Ok(()) => {
                let error = read_bounded_regular_file(&symlink, 4, "test")
                    .expect_err("registry file symlink must be rejected without following it");
                assert_eq!(error.code.as_deref(), Some("invalid_registry_file"));
            }
            Err(error) => eprintln!(
                "skipping registry symlink probe because this host cannot create one: {error}"
            ),
        }
    }

    fn test_entry_metadata(name: &str, version: &str, checksum_digit: char) -> EntryMetadata {
        EntryMetadata {
            name: name.to_string(),
            version: version.to_string(),
            checksum: format!("sha256-{}", checksum_digit.to_string().repeat(64)),
            dependencies: Vec::new(),
        }
    }

    fn install_clean_test_package_storage(
        config: &RegistryServerConfig,
        package_name: &str,
        physical: &[EntryMetadata],
        visible: Vec<EntryMetadata>,
        yanked: &[&str],
    ) {
        let storage = RegistryStorage::open(&config.data_dir)
            .expect("initialize clean test registry storage");
        let package_root = package_storage_root(&storage, package_name)
            .expect("create clean test package storage");
        let versions_root = ensure_request_child_directory(&package_root, VERSIONS_DIR)
            .expect("create clean test version storage");
        let yanks_root = ensure_request_child_directory(&package_root, YANKS_DIR)
            .expect("create clean test yank storage");
        ensure_request_child_directory(&package_root, INDEX_GENERATIONS_DIR)
            .expect("create clean test index storage");
        for metadata in physical {
            let version_root = versions_root.join(&metadata.version);
            fs::create_dir(&version_root).expect("create clean test physical version");
            write_entry_metadata(&version_root.join(ENTRY_METADATA_FILE), metadata)
                .expect("write clean test physical metadata");
        }
        for version in yanked {
            create_yank_marker(&yanks_root, version).expect("create clean test yank marker");
        }
        let signed =
            generate_signed_index(&SigningKey::from_bytes(&[0x11; 32]), package_name, visible)
                .expect("generate clean test signed index");
        persist_signed_generation(&package_root, &signed).expect("persist clean test signed index");
        switch_signed_index(&package_root, &signed.generation)
            .expect("select clean test signed index");
    }

    fn write_test_package(
        root: &Path,
        name: &str,
        version: &str,
        base_url: &str,
        public_key: &str,
        body: &str,
    ) -> crate::package::PackageContext {
        fs::create_dir_all(root.join("src")).expect("create test package source");
        fs::write(
            root.join("ku.mod"),
            format!(
                "name = \"{name}\"\nversion = \"{version}\"\nregistry.url = \"{base_url}\"\nregistry.public_key = \"{public_key}\"\n"
            ),
        )
        .expect("write test package manifest");
        fs::write(root.join("src/main.ku"), body).expect("write test package source");
        discover_from_dir(root)
            .expect("discover test package")
            .expect("test package exists")
    }

    fn response_status(result: Result<ureq::Response, ureq::Error>) -> u16 {
        match result {
            Ok(response) => response.status(),
            Err(ureq::Error::Status(status, _)) => status,
            Err(err) => panic!("registry request transport failed: {err}"),
        }
    }

    fn connect_test_tls(
        base_url: &str,
        certificate: rustls::pki_types::CertificateDer<'static>,
    ) -> BufReader<StreamOwned<ClientConnection, TcpStream>> {
        let mut roots = RootCertStore::empty();
        roots
            .add(certificate)
            .expect("trust registry test certificate");
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connection = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("localhost").expect("valid registry test server name"),
        )
        .expect("create registry test TLS client");
        let stream =
            TcpStream::connect(registry_test_address(base_url)).expect("connect registry test TLS");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set registry test read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .expect("set registry test write timeout");
        BufReader::new(StreamOwned::new(connection, stream))
    }

    trait TestHttpResponseReader: BufRead {
        fn set_response_read_timeout(&mut self, timeout: Duration) -> io::Result<()>;
    }

    impl TestHttpResponseReader for BufReader<StreamOwned<ClientConnection, TcpStream>> {
        fn set_response_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.get_mut().sock.set_read_timeout(Some(timeout))
        }
    }

    impl TestHttpResponseReader for BufReader<TcpStream> {
        fn set_response_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.get_mut().set_read_timeout(Some(timeout))
        }
    }

    impl TestHttpResponseReader for BufReader<std::io::Cursor<Vec<u8>>> {
        fn set_response_read_timeout(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    fn test_http_timeout_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "registry test response exceeded its absolute deadline",
        )
    }

    fn test_http_remaining(deadline: Instant) -> io::Result<Duration> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(test_http_timeout_error)
    }

    fn map_test_http_read_error(error: io::Error, deadline: Instant) -> io::Error {
        if Instant::now() >= deadline
            || matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )
        {
            test_http_timeout_error()
        } else {
            error
        }
    }

    fn read_test_http_line_until(
        reader: &mut impl TestHttpResponseReader,
        max_line_bytes: usize,
        total: &mut usize,
        deadline: Instant,
    ) -> io::Result<Vec<u8>> {
        let mut line = Vec::with_capacity(128);
        loop {
            reader.set_response_read_timeout(test_http_remaining(deadline)?)?;
            let available = reader
                .fill_buf()
                .map_err(|error| map_test_http_read_error(error, deadline))?;
            if available.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated registry test response headers",
                ));
            }
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            let next_line_size = line.len().checked_add(take).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response line size overflowed",
                )
            })?;
            let next_total = total.checked_add(take).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response header size overflowed",
                )
            })?;
            if next_line_size > max_line_bytes.saturating_add(2)
                || next_total > MAX_TEST_HTTP_RESPONSE_HEADER_BYTES
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response headers exceeded their budget",
                ));
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            *total = next_total;
            if line.ends_with(b"\n") {
                if !line.ends_with(b"\r\n") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "registry test response headers require CRLF",
                    ));
                }
                line.truncate(line.len() - 2);
                test_http_remaining(deadline)?;
                return Ok(line);
            }
        }
    }

    fn parse_test_content_length(value: &str) -> io::Result<usize> {
        let value = value.trim();
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test content length is not canonical decimal",
            ));
        }
        value.parse::<usize>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test content length overflowed",
            )
        })
    }

    fn read_test_http_body_until(
        reader: &mut impl TestHttpResponseReader,
        body: &mut [u8],
        deadline: Instant,
    ) -> io::Result<()> {
        let mut offset = 0usize;
        while offset < body.len() {
            reader.set_response_read_timeout(test_http_remaining(deadline)?)?;
            let available = reader
                .fill_buf()
                .map_err(|error| map_test_http_read_error(error, deadline))?;
            if available.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated registry test response body",
                ));
            }
            let take = available.len().min(body.len() - offset);
            body[offset..offset + take].copy_from_slice(&available[..take]);
            reader.consume(take);
            offset += take;
        }
        test_http_remaining(deadline).map(|_| ())
    }

    fn read_test_http_response_with_timeout(
        reader: &mut impl TestHttpResponseReader,
        timeout: Duration,
    ) -> io::Result<(u16, HashMap<String, String>, Vec<u8>)> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "registry test response timeout overflowed",
            )
        })?;
        let mut total = 0usize;
        let status_line =
            read_test_http_line_until(reader, MAX_REQUEST_LINE_BYTES, &mut total, deadline)?;
        let status_line = std::str::from_utf8(&status_line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("registry test response status is not UTF-8: {error}"),
            )
        })?;
        let mut status_parts = status_line.splitn(3, ' ');
        if status_parts.next() != Some("HTTP/1.1") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test response has an unsupported HTTP version",
            ));
        }
        let status_text = status_parts.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test response is missing its status",
            )
        })?;
        if status_text.len() != 3 || !status_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test response status is not a three-digit decimal",
            ));
        }
        let status = status_text.parse::<u16>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("registry test response status is invalid: {error}"),
            )
        })?;
        let mut headers = HashMap::new();
        let mut header_count = 0usize;
        loop {
            let line =
                read_test_http_line_until(reader, MAX_REGISTRY_LINE_BYTES, &mut total, deadline)?;
            if line.is_empty() {
                break;
            }
            header_count += 1;
            if header_count > MAX_TEST_HTTP_RESPONSE_HEADERS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response had too many headers",
                ));
            }
            if line
                .first()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
                || !line.iter().all(|byte| {
                    byte.is_ascii() && (*byte == b'\t' || (*byte >= b' ' && *byte != b'\x7f'))
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response header is malformed",
                ));
            }
            let line = std::str::from_utf8(&line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("registry test response header is not UTF-8: {error}"),
                )
            })?;
            let (name, value) = line.split_once(':').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response header is missing a colon",
                )
            })?;
            if name.is_empty() || !name.bytes().all(is_http_token_byte) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response header name is invalid",
                ));
            }
            let name = name.to_ascii_lowercase();
            if headers
                .insert(name.clone(), value.trim().to_string())
                .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("registry test response repeated header {name}"),
                ));
            }
        }
        if headers.contains_key("transfer-encoding") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test response transfer encoding is unsupported",
            ));
        }
        let content_length =
            parse_test_content_length(headers.get("content-length").ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry test response is missing content length",
                )
            })?)?;
        if content_length > MAX_TEST_HTTP_RESPONSE_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry test response body exceeded the artifact budget",
            ));
        }
        let mut body = Vec::new();
        body.try_reserve_exact(content_length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "registry test response body allocation failed",
            )
        })?;
        body.resize(content_length, 0);
        read_test_http_body_until(reader, &mut body, deadline)?;
        Ok((status, headers, body))
    }

    fn read_test_http_response(
        reader: &mut impl TestHttpResponseReader,
    ) -> io::Result<(u16, HashMap<String, String>, Vec<u8>)> {
        read_test_http_response_with_timeout(reader, TEST_HTTP_RESPONSE_TIMEOUT)
    }

    fn write_test_get(stream: &mut impl Write, path: &str, connection: Option<&str>) {
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n")
            .expect("write registry test request line");
        if let Some(connection) = connection {
            write!(stream, "Connection: {connection}\r\n")
                .expect("write registry test connection header");
        }
        stream
            .write_all(b"\r\n")
            .expect("finish registry test request");
        stream.flush().expect("flush registry test request");
    }

    #[derive(Default)]
    struct RegistryLoadStats {
        success: usize,
        rejected: usize,
        internal_errors: usize,
    }

    impl RegistryLoadStats {
        fn add(&mut self, other: Self) {
            self.success += other.success;
            self.rejected += other.rejected;
            self.internal_errors += other.internal_errors;
        }

        fn classified(&self) -> usize {
            self.success + self.rejected + self.internal_errors
        }
    }

    fn registry_test_address(base_url: &str) -> SocketAddr {
        let port = base_url
            .strip_prefix("https://localhost:")
            .and_then(|value| value.strip_suffix("/v1/"))
            .expect("parse registry test address")
            .parse::<u16>()
            .expect("registry test port is numeric");
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn registry_load_remaining(deadline: Instant) -> io::Result<Duration> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "registry load-gate deadline exceeded",
            ))
        } else {
            Ok(remaining)
        }
    }

    fn connect_registry_load_tls(
        base_url: &str,
        certificate: rustls::pki_types::CertificateDer<'static>,
        deadline: Instant,
    ) -> io::Result<BufReader<StreamOwned<ClientConnection, TcpStream>>> {
        let connect_timeout = registry_load_remaining(deadline)?.min(Duration::from_secs(2));
        let stream = TcpStream::connect_timeout(&registry_test_address(base_url), connect_timeout)?;
        let remaining = registry_load_remaining(deadline)?;
        stream.set_read_timeout(Some(remaining))?;
        stream.set_write_timeout(Some(remaining))?;
        stream.set_nodelay(true)?;

        let mut roots = RootCertStore::empty();
        roots
            .add(certificate)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connection = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("localhost")
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?,
        )
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(BufReader::new(StreamOwned::new(connection, stream)))
    }

    fn read_registry_load_response(
        reader: &mut BufReader<StreamOwned<ClientConnection, TcpStream>>,
        deadline: Instant,
    ) -> io::Result<u16> {
        let remaining = registry_load_remaining(deadline)?;
        reader.get_mut().sock.set_read_timeout(Some(remaining))?;
        reader.get_mut().sock.set_write_timeout(Some(remaining))?;

        let mut total = 0usize;
        let status_line =
            read_crlf_line(reader, MAX_REQUEST_LINE_BYTES, &mut total).map_err(|failure| {
                io::Error::new(io::ErrorKind::InvalidData, failure.public_message)
            })?;
        let status_line = std::str::from_utf8(&status_line)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status"))?
            .parse::<u16>()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut content_length = None;
        loop {
            let line =
                read_crlf_line(reader, MAX_REGISTRY_LINE_BYTES, &mut total).map_err(|failure| {
                    io::Error::new(io::ErrorKind::InvalidData, failure.public_message)
                })?;
            if line.is_empty() {
                break;
            }
            let line = std::str::from_utf8(&line)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            let (name, value) = line.split_once(':').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP response header")
            })?;
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "duplicate HTTP content-length",
                    ));
                }
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
                );
            }
        }
        let content_length = content_length.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing HTTP content-length")
        })?;
        if content_length > MAX_REGISTRY_INDEX_BYTES as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry load response exceeded the index budget",
            ));
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body)?;
        Ok(status)
    }

    fn run_registry_load_client(
        base_url: String,
        certificate: rustls::pki_types::CertificateDer<'static>,
        requests: usize,
        deadline: Instant,
    ) -> RegistryLoadStats {
        let mut stats = RegistryLoadStats::default();
        let mut reader = match connect_registry_load_tls(&base_url, certificate, deadline) {
            Ok(reader) => reader,
            Err(_) => {
                stats.rejected = requests;
                return stats;
            }
        };
        for request_number in 0..requests {
            let remaining_requests = requests - request_number;
            let request = if request_number % 2 == 0 {
                "/v1/packages/math/index.toml"
            } else {
                "/v1/packages/math/index.toml.sig"
            };
            let result = (|| -> io::Result<u16> {
                let remaining = registry_load_remaining(deadline)?;
                reader.get_mut().sock.set_read_timeout(Some(remaining))?;
                reader.get_mut().sock.set_write_timeout(Some(remaining))?;
                write!(
                    reader.get_mut(),
                    "GET {request} HTTP/1.1\r\nHost: localhost\r\n{}\r\n",
                    if remaining_requests == 1 {
                        "Connection: close\r\n"
                    } else {
                        ""
                    }
                )?;
                reader.get_mut().flush()?;
                read_registry_load_response(&mut reader, deadline)
            })();
            match result {
                Ok(200) => stats.success += 1,
                Ok(500..=599) => stats.internal_errors += 1,
                Ok(_) => stats.rejected += 1,
                Err(_) => {
                    stats.rejected += remaining_requests;
                    break;
                }
            }
        }
        stats
    }

    fn run_registry_load_phase(
        base_url: &str,
        certificate: rustls::pki_types::CertificateDer<'static>,
        clients: usize,
        requests_per_client: usize,
        deadline: Instant,
    ) -> RegistryLoadStats {
        let barrier = Arc::new(Barrier::new(clients + 1));
        let mut handles = Vec::with_capacity(clients);
        for _ in 0..clients {
            let barrier = Arc::clone(&barrier);
            let base_url = base_url.to_string();
            let certificate = certificate.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                run_registry_load_client(base_url, certificate, requests_per_client, deadline)
            }));
        }
        barrier.wait();
        while handles.iter().any(|handle| !handle.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            handles.iter().all(thread::JoinHandle::is_finished),
            "registry load clients did not finish before the absolute deadline"
        );
        let mut stats = RegistryLoadStats::default();
        for handle in handles {
            stats.add(handle.join().expect("registry load client panicked"));
        }
        stats
    }

    fn wait_for_registry_accepts(accepted: &AtomicUsize, expected: usize, deadline: Instant) {
        while accepted.load(Ordering::Acquire) < expected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            accepted.load(Ordering::Acquire) >= expected,
            "registry did not accept the bounded blocker set before the deadline"
        );
    }

    fn put_artifact(
        agent: &ureq::Agent,
        base_url: &str,
        token: &str,
        artifact: &PackageArtifact,
        checksum: &str,
    ) -> u16 {
        let url = format!(
            "{}packages/{}/{}",
            base_url, artifact.name, artifact.version
        );
        let file = fs::File::open(&artifact.path).expect("open test package artifact");
        response_status(
            agent
                .put(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", PACKAGE_CONTENT_TYPE)
                .set("Content-Length", &artifact.size.to_string())
                .set("X-Ku-Checksum", checksum)
                .set(
                    "Idempotency-Key",
                    &format!("{}-{}-{checksum}", artifact.name, artifact.version),
                )
                .send(file),
        )
    }

    fn put_yank(
        agent: &ureq::Agent,
        base_url: &str,
        token: &str,
        name: &str,
        version: &str,
        idempotency_key: Option<&str>,
        body: &[u8],
    ) -> u16 {
        let url = format!("{base_url}packages/{name}/{version}/yank");
        let mut request = agent
            .put(&url)
            .set("Authorization", &format!("Bearer {token}"));
        if let Some(idempotency_key) = idempotency_key {
            request = request.set("Idempotency-Key", idempotency_key);
        }
        let result = if body.is_empty() {
            request.set("Content-Length", "0").call()
        } else {
            request
                .set("Content-Length", &body.len().to_string())
                .send_bytes(body)
        };
        response_status(result)
    }

    fn write_test_publish(
        writer: &mut impl Write,
        token: &str,
        artifact: &PackageArtifact,
        body: &[u8],
    ) {
        write_test_publish_with_checksum(writer, token, artifact, &artifact.checksum, body);
    }

    fn write_test_publish_with_checksum(
        writer: &mut impl Write,
        token: &str,
        artifact: &PackageArtifact,
        checksum: &str,
        body: &[u8],
    ) {
        let request = test_publish_request(token, artifact, checksum, body);
        // One TLS write makes the small rejected-body regression deterministic:
        // the request head and complete body can share the server's input buffer.
        writer
            .write_all(&request)
            .expect("write test publish request");
        writer.flush().expect("flush test publish request");
    }

    fn test_publish_request(
        token: &str,
        artifact: &PackageArtifact,
        checksum: &str,
        body: &[u8],
    ) -> Vec<u8> {
        let mut request = format!(
            "PUT /v1/packages/{}/{} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: {PACKAGE_CONTENT_TYPE}\r\nContent-Length: {}\r\nX-Ku-Checksum: {}\r\nIdempotency-Key: {}-{}-{}\r\nConnection: close\r\n\r\n",
            artifact.name,
            artifact.version,
            artifact.size,
            checksum,
            artifact.name,
            artifact.version,
            checksum,
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    fn read_closed_test_status(
        reader: &mut BufReader<StreamOwned<ClientConnection, TcpStream>>,
    ) -> u16 {
        let (status, headers, _) =
            read_test_http_response(reader).expect("read closed registry test response");
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
        assert!(
            reader
                .fill_buf()
                .expect("read registry TLS close notification")
                .is_empty(),
            "Connection: close must end the registry TLS stream cleanly"
        );
        status
    }

    fn put_buffered_artifact(
        base_url: &str,
        certificate: rustls::pki_types::CertificateDer<'static>,
        token: &str,
        artifact: &PackageArtifact,
        checksum: &str,
    ) -> u16 {
        let body = fs::read(&artifact.path).expect("read buffered test package artifact");
        assert_eq!(
            u64::try_from(body.len()).expect("test package length fits u64"),
            artifact.size
        );
        assert!(
            body.len() <= MAX_BUFFERED_REJECT_BODY_BYTES,
            "buffered rejection fixture exceeds the server's bounded discard limit"
        );
        let request = test_publish_request(token, artifact, checksum, &body);
        assert!(
            request.len() <= REGISTRY_TLS_INPUT_BUFFER_BYTES,
            "buffered rejection fixture exceeds the server TLS input buffer"
        );
        let mut reader = connect_test_tls(base_url, certificate);
        reader
            .get_mut()
            .write_all(&request)
            .expect("write buffered test publish request");
        reader
            .get_mut()
            .flush()
            .expect("flush buffered test publish request");
        read_closed_test_status(&mut reader)
    }

    fn put_buffered_yank(
        base_url: &str,
        certificate: rustls::pki_types::CertificateDer<'static>,
        token: &str,
        name: &str,
        version: &str,
        idempotency_key: Option<&str>,
        body: &[u8],
    ) -> u16 {
        assert!(
            body.len() <= MAX_BUFFERED_REJECT_BODY_BYTES,
            "buffered yank fixture exceeds the server's bounded discard limit"
        );
        let idempotency_header = idempotency_key
            .map(|key| format!("Idempotency-Key: {key}\r\n"))
            .unwrap_or_default();
        let mut request = format!(
            "PUT /v1/packages/{name}/{version}/yank HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n{idempotency_header}Connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        assert!(
            request.len() <= REGISTRY_TLS_INPUT_BUFFER_BYTES,
            "buffered yank fixture exceeds the server TLS input buffer"
        );
        let mut reader = connect_test_tls(base_url, certificate);
        reader
            .get_mut()
            .write_all(&request)
            .expect("write buffered test yank request");
        reader
            .get_mut()
            .flush()
            .expect("flush buffered test yank request");
        read_closed_test_status(&mut reader)
    }

    fn wait_for_mutation_names(server: &TestServer, names: &[&str]) {
        let state = server.state.upgrade().expect("registry state is running");
        let expected = names
            .iter()
            .map(|name| name.to_string())
            .collect::<HashSet<_>>();
        let deadline = Instant::now() + TEST_HTTP_RESPONSE_TIMEOUT;
        loop {
            let actual = state
                .mutation_admission
                .active_packages
                .lock()
                .expect("mutation admission mutex poisoned")
                .clone();
            if actual == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "mutation state did not settle before its deadline: expected {expected:?}, got {actual:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn registry_mutation_admission_is_bounded_and_releases_on_error_and_panic() {
        let admission = MutationAdmission::new();
        let first = admission.try_acquire("math").expect("admit first package");
        for _ in 0..32 {
            assert_eq!(
                admission
                    .try_acquire("math")
                    .err()
                    .expect("reject duplicate")
                    .status,
                429
            );
        }
        let other = ["text", "json", "time"].map(|name| {
            admission
                .try_acquire(name)
                .expect("admit different package")
        });
        assert_eq!(
            admission.active_packages.lock().unwrap().len(),
            MAX_ACTIVE_MUTATIONS
        );
        assert_eq!(
            admission
                .try_acquire("fifth")
                .err()
                .expect("reject global overflow")
                .status,
            429
        );
        drop(first);
        let replacement = admission
            .try_acquire("fifth")
            .expect("reuse released capacity");
        drop((other, replacement));
        assert!(admission.active_packages.lock().unwrap().is_empty());

        let result = (|| -> Result<(), RequestFailure> {
            let _guard = admission.try_acquire("math")?;
            ensure_request_deadline(Instant::now() - Duration::from_millis(1))?;
            Ok(())
        })();
        assert_eq!(result.expect_err("early deadline return").status, 408);
        assert!(admission.active_packages.lock().unwrap().is_empty());

        let panic = std::panic::catch_unwind(|| {
            let _guard = admission.try_acquire("math").expect("admit before panic");
            panic!("intentional mutation guard unwind");
        });
        assert!(panic.is_err());
        assert!(admission.active_packages.lock().unwrap().is_empty());
        drop(admission.try_acquire("math").expect("reuse after unwind"));
    }

    #[test]
    fn registry_mutation_admission_has_one_concurrent_winner_per_package() {
        let admission = Arc::new(MutationAdmission::new());
        let start = Arc::new(Barrier::new(9));
        let (ready_sender, ready_receiver) = mpsc::sync_channel(8);
        let mut releases = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let admission = Arc::clone(&admission);
            let start = Arc::clone(&start);
            let ready_sender = ready_sender.clone();
            let (release_sender, release_receiver) = mpsc::sync_channel(1);
            releases.push(release_sender);
            handles.push(thread::spawn(move || {
                start.wait();
                let slot = admission.try_acquire("math");
                let status = slot.as_ref().map_or_else(|failure| failure.status, |_| 200);
                ready_sender.send(status).expect("report admission result");
                release_receiver
                    .recv_timeout(TEST_HTTP_RESPONSE_TIMEOUT)
                    .expect("release concurrent mutation before its deadline");
                drop(slot);
            }));
        }
        start.wait();
        let statuses = (0..8)
            .map(|_| {
                ready_receiver
                    .recv_timeout(TEST_HTTP_RESPONSE_TIMEOUT)
                    .expect("all contenders must finish nonblocking admission")
            })
            .collect::<Vec<_>>();
        assert_eq!(statuses.iter().filter(|&&status| status == 200).count(), 1);
        assert_eq!(statuses.iter().filter(|&&status| status == 429).count(), 7);
        assert_eq!(admission.active_packages.lock().unwrap().len(), 1);
        for release in releases {
            release.send(()).expect("release contender");
        }
        for handle in handles {
            handle.join().expect("admission contender panicked");
        }
        assert!(admission.active_packages.lock().unwrap().is_empty());
    }

    #[test]
    fn registry_recovery_lock_rejects_expired_budget_before_io() {
        let root = TestRoot::new("recovery-lock-expired");
        assert_eq!(
            PackagePublishLock::acquire(
                &root.0,
                "expired",
                Instant::now() - Duration::from_millis(1),
            )
            .err()
            .expect("expired recovery must not start lock I/O")
            .status,
            503
        );
        assert!(!root.0.join("expired.lock").exists());
    }

    #[test]
    fn registry_mutation_os_lock_is_nonblocking_and_recovery_wait_is_bounded() {
        let root = TestRoot::new("mutation-os-lock");
        let held =
            PackagePublishLock::acquire(&root.0, "math", Instant::now() + Duration::from_secs(1))
                .expect("hold package lock");
        assert_eq!(
            PackagePublishLock::try_acquire(
                &root.0,
                "math",
                Instant::now() + Duration::from_secs(1),
            )
            .err()
            .expect("mutation does not wait on a busy OS lock")
            .status,
            429
        );
        assert_eq!(
            PackagePublishLock::acquire(
                &root.0,
                "math",
                Instant::now() + Duration::from_millis(20),
            )
            .err()
            .expect("recovery lock wait retains its absolute deadline")
            .status,
            503
        );
        drop(held);
        drop(
            PackagePublishLock::try_acquire(
                &root.0,
                "math",
                Instant::now() + Duration::from_secs(1),
            )
            .expect("reuse released OS lock"),
        );
        assert_eq!(
            PackagePublishLock::try_acquire(
                &root.0,
                "expired",
                Instant::now() - Duration::from_millis(1),
            )
            .err()
            .expect("expired admission must not start lock I/O")
            .status,
            408
        );
        assert!(!root.0.join("expired.lock").exists());
    }

    fn install_test_artifact(
        root: &Path,
        name: &str,
        version: &str,
        bytes: &[u8],
    ) -> (PathBuf, EntryMetadata) {
        let checksum = format!("sha256-{:x}", Sha256::digest(bytes));
        let metadata = EntryMetadata {
            name: name.to_string(),
            version: version.to_string(),
            checksum: checksum.clone(),
            dependencies: Vec::new(),
        };
        fs::create_dir_all(root).expect("create direct artifact version root");
        let path = root.join(artifact_storage_name(&checksum).expect("artifact storage name"));
        fs::write(&path, bytes).expect("write direct artifact");
        (path, metadata)
    }

    #[test]
    fn artifact_verification_transient_failure_retries_unchanged_fingerprint() {
        let root = TestRoot::new("artifact-transient-retry");
        let cache = ArtifactVerificationCache::new(2);
        let (path, metadata) = install_test_artifact(&root.0, "retry", "1.0.0", b"valid bytes");
        let (_, before) = open_bounded_artifact(&path).expect("open initial artifact");
        cache.fail_next_verification.store(true, Ordering::Relaxed);
        let failure = open_cached_verified_artifact(
            &cache,
            &root.0,
            &metadata,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("injected I/O failure must fail this request");
        assert_eq!(failure.status, 500);
        assert!(cache.inner.lock().unwrap().entries.is_empty());
        let (_, after) = open_bounded_artifact(&path).expect("reopen unchanged artifact");
        assert_eq!(before, after);
        open_cached_verified_artifact(
            &cache,
            &root.0,
            &metadata,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("same fingerprint must retry after transient I/O failure");
        assert_eq!(cache.hash_count.load(Ordering::Relaxed), 2);
        open_cached_verified_artifact(
            &cache,
            &root.0,
            &metadata,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("successful retry populates hot cache");
        assert_eq!(cache.hash_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn artifact_verification_waiters_have_deadlines_and_cache_is_bounded_lru() {
        let root = TestRoot::new("artifact-cache-unit");
        let cache = Arc::new(ArtifactVerificationCache::new(2));
        let (_first_path, first) =
            install_test_artifact(&root.0.join("first"), "first", "1.0.0", b"first");
        cache.hash_delay_ms.store(250, Ordering::Relaxed);
        let leader_cache = Arc::clone(&cache);
        let leader_root = root.0.join("first");
        let leader_metadata = first.clone();
        let leader = thread::spawn(move || {
            open_cached_verified_artifact(
                &leader_cache,
                &leader_root,
                &leader_metadata,
                Instant::now() + Duration::from_secs(2),
            )
        });
        let started = Instant::now();
        while cache.hash_count.load(Ordering::Relaxed) == 0
            && started.elapsed() < Duration::from_secs(1)
        {
            thread::yield_now();
        }
        assert_eq!(cache.hash_count.load(Ordering::Relaxed), 1);
        let failure = open_cached_verified_artifact(
            &cache,
            &root.0.join("first"),
            &first,
            Instant::now() + Duration::from_millis(40),
        )
        .expect_err("waiter must honor its own deadline");
        assert_eq!(failure.status, 408);
        leader
            .join()
            .expect("artifact leader panicked")
            .expect("artifact leader verifies");
        cache.hash_delay_ms.store(0, Ordering::Relaxed);

        let (_, second) =
            install_test_artifact(&root.0.join("second"), "second", "1.0.0", b"second");
        let (_, third) = install_test_artifact(&root.0.join("third"), "third", "1.0.0", b"third");
        open_cached_verified_artifact(
            &cache,
            &root.0.join("second"),
            &second,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("verify second artifact");
        open_cached_verified_artifact(
            &cache,
            &root.0.join("first"),
            &first,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("refresh first artifact LRU position");
        open_cached_verified_artifact(
            &cache,
            &root.0.join("third"),
            &third,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("verify third artifact");
        assert_eq!(cache.inner.lock().unwrap().entries.len(), 2);
        open_cached_verified_artifact(
            &cache,
            &root.0.join("second"),
            &second,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("evicted second artifact verifies again");
        assert_eq!(cache.hash_count.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn artifact_get_single_flight_hot_cache_invalidation_and_headers() {
        let root = TestRoot::new("artifact-cache-e2e");
        let mut files =
            test_server_files(&root.0, &[("token-cache", "cache")], Duration::from_secs(5));
        files.config.workers = 16;
        files.config.queue_capacity = 32;
        let server = TestServer::start(files.config.clone(), files.certificate);
        let package = write_test_package(
            &root.0.join("package"),
            "cache",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 7 }\n",
        );
        let artifact = pack_package(&package).expect("pack cache test package");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-cache",
                &artifact,
                &artifact.checksum,
            ),
            201
        );
        server
            .artifact_verifications
            .hash_delay_ms
            .store(100, Ordering::Relaxed);
        let url = format!(
            "{}artifacts/cache-1.0.0-{}.tar.zst",
            server.base_url, artifact.checksum
        );
        let barrier = Arc::new(Barrier::new(16));
        let handles = (0..16)
            .map(|_| {
                let agent = server.agent.clone();
                let url = url.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let response = agent.get(&url).call().expect("concurrent artifact GET");
                    assert_eq!(response.status(), 200);
                    let mut bytes = Vec::new();
                    response
                        .into_reader()
                        .read_to_end(&mut bytes)
                        .expect("read concurrent artifact");
                    bytes
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(
                handle.join().expect("artifact GET thread panicked"),
                fs::read(&artifact.path).expect("read source artifact")
            );
        }
        assert_eq!(
            server
                .artifact_verifications
                .hash_count
                .load(Ordering::Relaxed),
            1,
            "all cold concurrent GETs must share one full checksum pass"
        );
        server
            .artifact_verifications
            .hash_delay_ms
            .store(0, Ordering::Relaxed);

        let response = server.agent.get(&url).call().expect("hot artifact GET");
        assert_eq!(
            response.header("etag"),
            Some(format!("\"{}\"", artifact.checksum).as_str())
        );
        assert_eq!(
            response.header("cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        let mut hot_bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut hot_bytes)
            .expect("read hot artifact");
        assert_eq!(
            server
                .artifact_verifications
                .hash_count
                .load(Ordering::Relaxed),
            1,
            "hot GET must only fingerprint the newly opened handle"
        );

        let stored = files
            .config
            .data_dir
            .join(PACKAGES_DIR)
            .join("cache")
            .join(VERSIONS_DIR)
            .join("1.0.0")
            .join(artifact_storage_name(&artifact.checksum).unwrap());
        let mut changed = fs::read(&stored).expect("read committed artifact");
        changed[0] ^= 0xff;
        fs::write(&stored, changed).expect("replace committed artifact with same length");
        assert_eq!(response_status(server.agent.get(&url).call()), 500);
        assert_eq!(
            server
                .artifact_verifications
                .hash_count
                .load(Ordering::Relaxed),
            2,
            "same-length replacement must invalidate and rehash"
        );
        assert_eq!(response_status(server.agent.get(&url).call()), 500);
        assert_eq!(
            server
                .artifact_verifications
                .hash_count
                .load(Ordering::Relaxed),
            2,
            "unchanged corrupt fingerprint must use the bounded negative cache"
        );
    }

    #[test]
    fn registry_v1_real_tls_publish_acl_idempotency_conflict_restart_and_race() {
        let root = TestRoot::new("e2e");
        let files = test_server_files(
            &root.0,
            &[("token-math", "math"), ("token-race", "race")],
            Duration::from_secs(5),
        );
        let certificate = files.certificate.clone();
        let mut server = TestServer::start(files.config.clone(), files.certificate.clone());
        assert_eq!(
            response_status(
                server
                    .agent
                    .get(&format!("{}packages/missing/index.toml", server.base_url))
                    .call(),
            ),
            404,
            "real TLS registry must answer a basic request before publish"
        );

        let math_root = root.0.join("math");
        let math = write_test_package(
            &math_root,
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&math).expect("pack math package");
        let math_next = write_test_package(
            &root.0.join("math-next"),
            "math",
            "1.1.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 11 }\n",
        );
        assert_eq!(
            put_buffered_artifact(
                &server.base_url,
                certificate.clone(),
                "wrong-token",
                &artifact,
                &artifact.checksum,
            ),
            401
        );
        assert_eq!(
            put_buffered_artifact(
                &server.base_url,
                certificate.clone(),
                "token-math",
                &PackageArtifact {
                    name: "other".to_string(),
                    ..artifact.clone()
                },
                &artifact.checksum,
            ),
            403
        );
        let bad_checksum = format!("sha256-{}", "00".repeat(32));
        assert_eq!(
            put_buffered_artifact(
                &server.base_url,
                certificate.clone(),
                "token-math",
                &artifact,
                &bad_checksum,
            ),
            400
        );

        {
            let _agent =
                TestRegistryHttpAgentGuard::install(server.base_url.clone(), server.agent.clone());
            let receipt = publish_package(&math, "token-math").expect("publish through Ku client");
            assert_eq!(receipt.checksum, artifact.checksum);
            publish_package(&math, "token-math").expect("idempotent Ku publish");
            publish_package(&math_next, "token-math")
                .expect("second version publishes from cached signed history metadata");
        }

        let consumer_root = root.0.join("consumer");
        fs::create_dir_all(consumer_root.join("src")).expect("create consumer source");
        fs::write(
            consumer_root.join("ku.mod"),
            format!(
                "name = \"consumer\"\nregistry.url = \"{}\"\nregistry.public_key = \"{}\"\ndep.math = \"^1.0.0\"\n",
                server.base_url, server.public_key
            ),
        )
        .expect("write consumer manifest");
        let consumer_main = consumer_root.join("src/main.ku");
        fs::write(
            &consumer_main,
            "import { Value } from \"@math/main\"\nfn main() { print(Value()) }\n",
        )
        .expect("write consumer entry");
        {
            let _agent =
                TestRegistryHttpAgentGuard::install(server.base_url.clone(), server.agent.clone());
            run_cli(vec![
                "ku".to_string(),
                "package".to_string(),
                "resolve".to_string(),
                consumer_root.to_string_lossy().to_string(),
            ])
            .expect("consumer resolves and writes a lock from the real registry");
            let main = consumer_main.to_string_lossy().to_string();
            run_cli(vec![
                "ku".to_string(),
                "check".to_string(),
                "--locked".to_string(),
                main.clone(),
            ])
            .expect("consumer check --locked");
            run_cli(vec![
                "ku".to_string(),
                "run".to_string(),
                "--locked".to_string(),
                main.clone(),
            ])
            .expect("consumer run --locked");
            run_cli(vec![
                "ku".to_string(),
                "build".to_string(),
                "--native".to_string(),
                "--locked".to_string(),
                main,
            ])
            .expect("consumer native build --locked");
        }

        let index = server
            .agent
            .get(&format!("{}packages/math/index.toml", server.base_url))
            .call()
            .expect("get signed registry index")
            .into_string()
            .expect("read signed registry index");
        assert!(index.contains(&artifact.checksum));
        assert!(index.contains(&format!(
            "../../artifacts/math-1.0.0-{}.tar.zst",
            artifact.checksum
        )));
        let signature = server
            .agent
            .get(&format!("{}packages/math/index.toml.sig", server.base_url))
            .call()
            .expect("get registry index signature")
            .into_string()
            .expect("read registry signature");
        assert!(signature.starts_with("ed25519-"));
        let downloaded = server
            .agent
            .get(&format!(
                "{}artifacts/math-1.0.0-{}.tar.zst",
                server.base_url, artifact.checksum
            ))
            .call()
            .expect("download committed artifact")
            .into_reader();
        let mut downloaded = BufReader::new(downloaded);
        let mut bytes = Vec::new();
        downloaded
            .read_to_end(&mut bytes)
            .expect("read committed artifact");
        assert_eq!(
            Sha256::digest(&bytes),
            Sha256::digest(fs::read(&artifact.path).unwrap())
        );

        fs::write(
            math_root.join("src/main.ku"),
            "fn Value(): int { return 2 }\n",
        )
        .expect("change math source");
        let changed_math = discover_from_dir(&math_root)
            .expect("rediscover changed math")
            .expect("changed math exists");
        let changed_artifact = pack_package(&changed_math).expect("pack changed math");
        assert_eq!(
            put_buffered_artifact(
                &server.base_url,
                certificate.clone(),
                "token-math",
                &changed_artifact,
                &changed_artifact.checksum,
            ),
            409
        );

        let race_a = write_test_package(
            &root.0.join("race-a"),
            "race",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Winner(): int { return 1 }\n",
        );
        let race_b = write_test_package(
            &root.0.join("race-b"),
            "race",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Winner(): int { return 2 }\n",
        );
        let artifacts = [
            pack_package(&race_a).expect("pack first race artifact"),
            pack_package(&race_b).expect("pack second race artifact"),
        ];
        let barrier = Arc::new(Barrier::new(3));
        let mut publishers = Vec::new();
        for artifact in artifacts {
            let barrier = Arc::clone(&barrier);
            let base_url = server.base_url.clone();
            let certificate = certificate.clone();
            publishers.push(thread::spawn(move || {
                barrier.wait();
                let status = put_buffered_artifact(
                    &base_url,
                    certificate,
                    "token-race",
                    &artifact,
                    &artifact.checksum,
                );
                (artifact, status)
            }));
        }
        barrier.wait();
        let outcomes = publishers
            .into_iter()
            .map(|publisher| publisher.join().expect("race publisher panicked"))
            .collect::<Vec<_>>();
        // Wait for both attempts to finish before one explicit retry. Busy is
        // admission rejection, not success or a content conflict in disguise.
        let mut statuses = outcomes
            .into_iter()
            .map(|(artifact, status)| {
                if status == 429 {
                    put_buffered_artifact(
                        &server.base_url,
                        certificate.clone(),
                        "token-race",
                        &artifact,
                        &artifact.checksum,
                    )
                } else {
                    status
                }
            })
            .collect::<Vec<_>>();
        statuses.sort_unstable();
        assert_eq!(statuses, vec![201, 409]);

        let restart_port = server
            .base_url
            .strip_prefix("https://localhost:")
            .and_then(|value| value.strip_suffix("/v1/"))
            .expect("parse registry restart port")
            .parse::<u16>()
            .expect("registry restart port is numeric");
        server.stop();
        let pending = files
            .config
            .data_dir
            .join(PACKAGES_DIR)
            .join("math")
            .join(PUBLISH_PENDING_FILE);
        fs::write(&pending, b"1.0.0\n").expect("simulate pending publish marker");
        fs::remove_file(
            files
                .config
                .data_dir
                .join(PACKAGES_DIR)
                .join("math")
                .join(INDEX_CURRENT_FILE),
        )
        .expect("remove index pointer to force startup recovery");
        let mut restart_config = files.config.clone();
        restart_config.bind = SocketAddr::from(([127, 0, 0, 1], restart_port));
        let mut restarted = TestServer::start(restart_config, files.certificate.clone());
        assert!(
            !pending.exists(),
            "startup must clear recovered pending marker"
        );
        {
            // Restore the original bytes and point the author at the restarted listener.
            let recovered_math = write_test_package(
                &math_root,
                "math",
                "1.0.0",
                &restarted.base_url,
                &restarted.public_key,
                "fn Value(): int { return 1 }\n",
            );
            let _agent = TestRegistryHttpAgentGuard::install(
                restarted.base_url.clone(),
                restarted.agent.clone(),
            );
            publish_package(&recovered_math, "token-math")
                .expect("idempotent publish and signed-index verification survive restart");
        }
        restarted.stop();
        drop(restarted);
        drop(server);
        fs::remove_file(consumer_main.with_extension("c"))
            .expect("remove this fixture's online C before fresh offline emission");
        let lock_path = consumer_root.join("ku.lock");
        let locked =
            fs::read(&lock_path).expect("read registry consumer lock before offline build");
        let main = consumer_main.to_string_lossy().to_string();
        run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            "--offline".to_string(),
            main.clone(),
        ])
        .expect("consumer check --offline after registry shutdown");
        run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            "--offline".to_string(),
            main.clone(),
        ])
        .expect("consumer run --offline after registry shutdown");
        run_cli(vec![
            "ku".to_string(),
            "build".to_string(),
            "--native".to_string(),
            "--offline".to_string(),
            main,
        ])
        .expect("consumer native build --offline after registry shutdown");

        let c_path = consumer_main.with_extension("c");
        let c = fs::read_to_string(&c_path).expect("read registry consumer offline native C");
        assert!(
            c.lines().any(|line| {
                line.starts_with("int64_t __ku_import") && line.contains("_Value(")
            }) && c.lines().any(|line| line.trim() == "return 11;"),
            "offline C must contain the imported math 1.1.0 implementation"
        );
        assert!(
            !c.contains("run_source") && !c.contains("const SOURCE"),
            "offline C must not embed the Ku source runner"
        );
        assert_eq!(
            fs::read(&lock_path).expect("read registry consumer lock after offline build"),
            locked,
            "offline check, run and native build must not rewrite the lock"
        );

        // C emission above is a hard gate even on hosts without a C compiler.
        // Linking and execution reuse the bounded native test harness.
        let Some(executable) = compile_harness(&root.0, &c_path, "registry-consumer") else {
            eprintln!("skip: no C compiler found; registry offline C artifact was verified");
            return;
        };
        let relocated = TestRoot::new("native-relocated");
        let temp_parent = fs::canonicalize(env::temp_dir()).expect("resolve test temp parent");
        let fixture_root = fs::canonicalize(&root.0).expect("resolve owned registry test root");
        let relocated_root =
            fs::canonicalize(&relocated.0).expect("resolve independent native test root");
        assert!(root.0.is_absolute() && relocated.0.is_absolute());
        assert_eq!(fixture_root.parent(), Some(temp_parent.as_path()));
        assert_eq!(relocated_root.parent(), Some(temp_parent.as_path()));
        assert_eq!(fixture_root.file_name(), root.0.file_name());
        assert_eq!(relocated_root.file_name(), relocated.0.file_name());
        assert!(
            fixture_root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ku-registry-e2e-")),
            "cleanup must target only this test's newly created registry fixture"
        );
        assert!(
            !relocated_root.starts_with(&fixture_root)
                && !fixture_root.starts_with(&relocated_root),
            "the executable must move outside every source, cache and registry directory"
        );
        assert_eq!(executable.parent(), Some(root.0.as_path()));
        let relocated_executable = relocated_root.join(
            executable
                .file_name()
                .expect("compiled registry consumer executable name"),
        );
        fs::rename(&executable, &relocated_executable)
            .expect("move registry consumer executable into an independent directory");

        // Both registry listeners and their workers have been joined. Remove
        // only this validated, newly created fixture: all authors, consumer
        // sources, ku.lock, .ku cache, emitted C and registry data live inside it.
        fs::remove_dir_all(&fixture_root).expect("remove owned registry source and cache fixture");
        assert!(!fixture_root.exists() && !consumer_main.exists());
        assert!(relocated_executable.is_file());

        let mut command = Command::new(&relocated_executable);
        command.current_dir(&relocated_root);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS).unwrap_or_else(|error| {
            panic!("relocated registry native consumer was not bounded: {error}")
        });
        assert!(
            output.status.success(),
            "source-free registry native consumer failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout, b"11",
            "native executable must run the published Value"
        );
        assert!(
            output.stderr.is_empty(),
            "source-free native consumer wrote stderr"
        );
    }

    #[test]
    fn registry_mutation_same_package_flood_does_not_block_another_package() {
        let root = TestRoot::new("mutation-package-isolation");
        let files = test_server_files(
            &root.0,
            &[("token-math", "math"), ("token-text", "text")],
            Duration::from_secs(5),
        );
        let certificate = files.certificate.clone();
        let mut server = TestServer::start(files.config, files.certificate);
        let math = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let text = write_test_package(
            &root.0.join("text"),
            "text",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 2 }\n",
        );
        let artifact = pack_package(&math).expect("pack blocked package");
        let other_artifact = pack_package(&text).expect("pack independent package");
        let body = Arc::new(fs::read(&artifact.path).expect("read test package body"));
        assert!(body.len() <= MAX_BUFFERED_REJECT_BODY_BYTES);

        let mut slow = connect_test_tls(&server.base_url, certificate.clone());
        write_test_publish(slow.get_mut(), "token-math", &artifact, &[]);
        wait_for_mutation_names(&server, &["math"]);

        let barrier = Arc::new(Barrier::new(7));
        let mut duplicates = Vec::new();
        for index in 0..6 {
            let barrier = Arc::clone(&barrier);
            let base_url = server.base_url.clone();
            let certificate = certificate.clone();
            let artifact = artifact.clone();
            let body = Arc::clone(&body);
            let agent = server.agent.clone();
            duplicates.push(thread::spawn(move || {
                barrier.wait();
                if index % 2 == 0 {
                    let mut stream = connect_test_tls(&base_url, certificate);
                    write_test_publish(stream.get_mut(), "token-math", &artifact, &body);
                    read_test_http_response(&mut stream)
                        .expect("read same-package publish rejection")
                        .0
                } else {
                    put_yank(
                        &agent,
                        &base_url,
                        "token-math",
                        "math",
                        "1.0.0",
                        Some("yank-math-1.0.0"),
                        &[],
                    )
                }
            }));
        }
        barrier.wait();
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-text",
                &other_artifact,
                &other_artifact.checksum,
            ),
            201,
            "one hot package must not consume every mutation slot"
        );
        let deadline = Instant::now() + TEST_HTTP_RESPONSE_TIMEOUT;
        while duplicates.iter().any(|thread| !thread.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            duplicates.iter().all(thread::JoinHandle::is_finished),
            "duplicate mutations must reject without waiting for the slow upload"
        );
        for duplicate in duplicates {
            assert_eq!(duplicate.join().expect("duplicate mutation panicked"), 429);
        }
        wait_for_mutation_names(&server, &["math"]);
        let staging = server.state.upgrade().unwrap().storage.staging.clone();
        assert_eq!(
            fs::read_dir(&staging).expect("read active staging").count(),
            1,
            "rejected duplicate requests must not allocate upload staging"
        );

        slow.get_mut()
            .write_all(&body)
            .expect("finish admitted upload");
        slow.get_mut().flush().expect("flush admitted upload");
        assert_eq!(
            read_test_http_response(&mut slow)
                .expect("read admitted publish response")
                .0,
            201
        );
        wait_for_mutation_names(&server, &[]);
        assert_eq!(
            fs::read_dir(&staging)
                .expect("read completed staging")
                .count(),
            0
        );
        server.stop();
    }

    #[test]
    fn registry_mutation_busy_rejection_handles_buffered_and_absent_bodies() {
        let root = TestRoot::new("mutation-busy-body");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
        let certificate = files.certificate.clone();
        let mut server = TestServer::start(files.config, files.certificate);
        let math = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&math).expect("pack busy-body package");
        let body = fs::read(&artifact.path).expect("read busy-body package");
        assert!(body.len() <= MAX_BUFFERED_REJECT_BODY_BYTES);
        {
            let state = server.state.upgrade().expect("registry is running");
            // Exercise both admission rejection paths with an actual TLS client.
            for names in [&["math"][..], &["one", "two", "three", "four"][..]] {
                let slots = names
                    .iter()
                    .map(|name| {
                        state
                            .mutation_admission
                            .try_acquire(name)
                            .expect("hold deterministic admission blockers")
                    })
                    .collect::<Vec<_>>();
                for request_body in [&body[..], &[][..]] {
                    let mut stream = connect_test_tls(&server.base_url, certificate.clone());
                    write_test_publish(stream.get_mut(), "token-math", &artifact, request_body);
                    let (status, headers, _) = read_test_http_response(&mut stream)
                        .expect("busy rejection must arrive without waiting for more body bytes");
                    assert_eq!(status, 429);
                    assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
                    let mut end = [0u8; 1];
                    assert_eq!(
                        stream
                            .read(&mut end)
                            .expect("read busy TLS close without reset"),
                        0
                    );
                    wait_for_mutation_names(&server, names);
                    assert_eq!(fs::read_dir(&state.storage.staging).unwrap().count(), 0);
                }
                drop(slots);
                wait_for_mutation_names(&server, &[]);
            }
        }
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum
            ),
            201,
            "capacity is usable after both rejection paths"
        );
        server.stop();
    }

    #[test]
    fn registry_mutation_failures_release_admission_and_staging_before_retry() {
        let root = TestRoot::new("mutation-failure-release");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
        let certificate = files.certificate.clone();
        let mut server = TestServer::start(files.config, files.certificate);
        let math = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&math).expect("pack retry package");
        let body = fs::read(&artifact.path).expect("read retry package");
        let (staging, lock_path) = {
            let state = server.state.upgrade().expect("registry is running");
            (
                state.storage.staging.clone(),
                state.storage.locks.join("math.lock"),
            )
        };

        let bad_checksum = PackageArtifact {
            checksum: format!("sha256-{}", "00".repeat(32)),
            ..artifact.clone()
        };
        let invalid_bytes = b"not a package archive";
        let bad_archive = PackageArtifact {
            checksum: format!("sha256-{:x}", Sha256::digest(invalid_bytes)),
            size: invalid_bytes.len() as u64,
            ..artifact.clone()
        };
        for (attempt, request_body) in [
            (&bad_checksum, body.as_slice()),
            (&bad_archive, &invalid_bytes[..]),
        ] {
            let mut stream = connect_test_tls(&server.base_url, certificate.clone());
            write_test_publish(stream.get_mut(), "token-math", attempt, request_body);
            assert_eq!(
                read_test_http_response(&mut stream)
                    .expect("read invalid upload response")
                    .0,
                400
            );
            wait_for_mutation_names(&server, &[]);
            assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        }

        let mut truncated = connect_test_tls(&server.base_url, certificate.clone());
        write_test_publish(truncated.get_mut(), "token-math", &artifact, &[]);
        wait_for_mutation_names(&server, &["math"]);
        truncated.get_mut().conn.send_close_notify();
        truncated.get_mut().flush().expect("end a truncated upload");
        assert_eq!(
            read_test_http_response(&mut truncated)
                .expect("read truncated upload failure")
                .0,
            400
        );
        wait_for_mutation_names(&server, &[]);
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);

        fs::create_dir(&lock_path).expect("inject an invalid OS lock target");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum
            ),
            500
        );
        wait_for_mutation_names(&server, &[]);
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        fs::remove_dir(&lock_path).expect("remove the exact invalid lock fixture");

        for expected in [201, 200] {
            assert_eq!(
                put_artifact(
                    &server.agent,
                    &server.base_url,
                    "token-math",
                    &artifact,
                    &artifact.checksum
                ),
                expected
            );
            wait_for_mutation_names(&server, &[]);
            assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        }
        let conflict = write_test_package(
            &root.0.join("conflict"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 2 }\n",
        );
        let conflict = pack_package(&conflict).expect("pack conflicting version");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &conflict,
                &conflict.checksum
            ),
            409
        );
        wait_for_mutation_names(&server, &[]);
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum
            ),
            200,
            "a conflict must release the original package's slot"
        );
        server.stop();
    }

    #[test]
    fn registry_publish_and_yank_lock_before_package_root_access() {
        let root = TestRoot::new("mutation-lock-order");
        let files = test_server_files(
            &root.0,
            &[("token-math", "math"), ("token-missing", "missing")],
            Duration::from_secs(15),
        );
        let config = files.config.clone();
        let mut server = TestServer::start(files.config, files.certificate);
        let package = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&package).expect("pack lock-order package");
        let lock_root = fs::canonicalize(config.data_dir.join(LOCKS_DIR))
            .expect("canonical registry lock root");
        let packages_root = config.data_dir.join(PACKAGES_DIR);

        let publish_lock = PackagePublishLock::acquire(
            &lock_root,
            "math",
            Instant::now() + Duration::from_secs(10),
        )
        .expect("hold first-publish package lock");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            429,
            "publish must reject a busy OS lock instead of waiting with a mutation slot"
        );
        assert!(
            !packages_root.join("math").exists(),
            "first publish must not expose a half-initialized package root before acquiring its lock"
        );
        drop(publish_lock);
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201,
            "the rejected attempt must release admission so an explicit retry can commit"
        );

        let yank_lock = PackagePublishLock::acquire(
            &lock_root,
            "missing",
            Instant::now() + Duration::from_secs(10),
        )
        .expect("hold missing-package yank lock");
        assert_eq!(
            put_yank(
                &server.agent,
                &server.base_url,
                "token-missing",
                "missing",
                "1.0.0",
                Some("yank-missing-1.0.0"),
                &[],
            ),
            429,
            "yank must reject a busy OS lock before observing package storage"
        );
        assert!(
            !packages_root.join("missing").exists(),
            "yank must not create storage for an unknown package"
        );
        drop(yank_lock);
        assert_eq!(
            put_yank(
                &server.agent,
                &server.base_url,
                "token-missing",
                "missing",
                "1.0.0",
                Some("yank-missing-1.0.0"),
                &[],
            ),
            404
        );
        server.stop();
    }

    #[test]
    fn registry_yank_is_monotonic_and_preserves_locked_artifacts() {
        let root = TestRoot::new("yank-e2e");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
        let certificate = files.certificate.clone();
        let mut server = TestServer::start(files.config, files.certificate);
        let math = write_test_package(
            &root.0.join("math-1"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let math_next = write_test_package(
            &root.0.join("math-2"),
            "math",
            "1.1.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 11 }\n",
        );
        let artifact = pack_package(&math).expect("pack first yank test version");
        let next_artifact = pack_package(&math_next).expect("pack second yank test version");
        let _agent =
            TestRegistryHttpAgentGuard::install(server.base_url.clone(), server.agent.clone());
        publish_package(&math, "token-math").expect("publish first yank test version");
        publish_package(&math_next, "token-math").expect("publish second yank test version");

        assert_eq!(
            put_yank(
                &server.agent,
                &server.base_url,
                "wrong-token",
                "math",
                "1.1.0",
                Some("yank-math-1.1.0"),
                &[],
            ),
            401
        );
        assert_eq!(
            put_yank(
                &server.agent,
                &server.base_url,
                "token-math",
                "other",
                "1.1.0",
                Some("yank-other-1.1.0"),
                &[],
            ),
            403
        );
        assert_eq!(
            put_yank(
                &server.agent,
                &server.base_url,
                "token-math",
                "math",
                "9.9.9",
                Some("yank-math-9.9.9"),
                &[],
            ),
            404
        );
        assert_eq!(
            put_yank(
                &server.agent,
                &server.base_url,
                "token-math",
                "math",
                "1.1.0",
                Some("wrong-key"),
                &[],
            ),
            400
        );
        assert_eq!(
            put_buffered_yank(
                &server.base_url,
                certificate.clone(),
                "token-math",
                "math",
                "1.1.0",
                Some("yank-math-1.1.0"),
                b"x",
            ),
            400
        );
        let index_before_yank = server
            .agent
            .get(&format!("{}packages/math/index.toml", server.base_url))
            .call()
            .expect("read index before yank")
            .into_string()
            .expect("read index body before yank");
        assert!(index_before_yank.contains("version = \"1.1.0\""));

        let consumer_root = root.0.join("consumer");
        fs::create_dir_all(consumer_root.join("src")).expect("create yank consumer source");
        fs::write(
            consumer_root.join("ku.mod"),
            format!(
                "name = \"consumer\"\nregistry.url = \"{}\"\nregistry.public_key = \"{}\"\ndep.math = \"^1.0.0\"\n",
                server.base_url, server.public_key
            ),
        )
        .expect("write yank consumer manifest");
        fs::write(consumer_root.join("src/main.ku"), "fn main() {}\n")
            .expect("write yank consumer source");
        let mut consumer = discover_from_dir(&consumer_root)
            .expect("discover yank consumer")
            .expect("yank consumer package");
        resolve_remote_dependencies_with_mode(&mut consumer, DependencyResolveMode::Refresh)
            .expect("initial refresh selects newest version");
        assert_eq!(consumer.resolved_registry_dependencies[0].version, "1.1.0");
        write_lock(&consumer).expect("write pre-yank lock");
        let locked_cache_target = consumer.resolved_registry_dependencies[0]
            .package_root
            .parent()
            .expect("registry package root has cache target")
            .to_path_buf();

        let receipt = yank_package(&math_next, "token-math").expect("first yank succeeds");
        assert_eq!(receipt.version, "1.1.0");
        yank_package(&math_next, "token-math").expect("repeated yank succeeds");

        let barrier = Arc::new(Barrier::new(3));
        let mut repeated_yanks = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let agent = server.agent.clone();
            let base_url = server.base_url.clone();
            repeated_yanks.push(thread::spawn(move || {
                barrier.wait();
                put_yank(
                    &agent,
                    &base_url,
                    "token-math",
                    "math",
                    "1.1.0",
                    Some("yank-math-1.1.0"),
                    &[],
                )
            }));
        }
        barrier.wait();
        let outcomes = repeated_yanks
            .into_iter()
            .map(|repeated| repeated.join().expect("repeated yank panicked"))
            .collect::<Vec<_>>();
        for status in outcomes {
            let status = if status == 429 {
                put_yank(
                    &server.agent,
                    &server.base_url,
                    "token-math",
                    "math",
                    "1.1.0",
                    Some("yank-math-1.1.0"),
                    &[],
                )
            } else {
                status
            };
            assert_eq!(status, 200, "an explicit idempotent retry must succeed");
        }

        assert_eq!(
            put_buffered_artifact(
                &server.base_url,
                certificate,
                "token-math",
                &next_artifact,
                &next_artifact.checksum,
            ),
            200,
            "an idempotent duplicate publish must not unyank a version"
        );
        let index_after_yank = server
            .agent
            .get(&format!("{}packages/math/index.toml", server.base_url))
            .call()
            .expect("read index after yank")
            .into_string()
            .expect("read index body after yank");
        assert!(index_after_yank.contains("version = \"1.0.0\""));
        assert!(!index_after_yank.contains("version = \"1.1.0\""));
        assert_eq!(
            response_status(
                server
                    .agent
                    .get(&format!(
                        "{}artifacts/math-1.1.0-{}.tar.zst",
                        server.base_url, next_artifact.checksum
                    ))
                    .call(),
            ),
            200,
            "yank must retain the immutable artifact for existing locks"
        );

        fs::remove_dir_all(&locked_cache_target).expect("remove locked cache before redownload");
        let mut update_consumer = discover_from_dir(&consumer_root)
            .expect("rediscover default-update consumer")
            .expect("default-update consumer package");
        resolve_remote_dependencies_with_mode(&mut update_consumer, DependencyResolveMode::Update)
            .expect("default update redownloads the yanked version fixed by the existing lock");
        assert_eq!(
            update_consumer.resolved_registry_dependencies[0].version,
            "1.1.0"
        );
        fs::remove_dir_all(&locked_cache_target)
            .expect("remove default-update cache before locked redownload");
        let mut locked_consumer = discover_from_dir(&consumer_root)
            .expect("rediscover locked consumer")
            .expect("locked consumer package");
        resolve_remote_dependencies_with_mode(&mut locked_consumer, DependencyResolveMode::Locked)
            .expect("locked resolution redownloads the yanked immutable artifact directly");
        assert_eq!(
            locked_consumer.resolved_registry_dependencies[0].version,
            "1.1.0"
        );
        let mut offline_consumer = discover_from_dir(&consumer_root)
            .expect("rediscover offline consumer")
            .expect("offline consumer package");
        resolve_remote_dependencies_with_mode(
            &mut offline_consumer,
            DependencyResolveMode::Offline,
        )
        .expect("offline resolution reuses the cached yanked version fixed by the lock");
        assert_eq!(
            offline_consumer.resolved_registry_dependencies[0].version,
            "1.1.0"
        );

        let mut refreshed_consumer = discover_from_dir(&consumer_root)
            .expect("rediscover refresh consumer")
            .expect("refresh consumer package");
        resolve_remote_dependencies_with_mode(
            &mut refreshed_consumer,
            DependencyResolveMode::Refresh,
        )
        .expect("refresh excludes the yanked version");
        assert_eq!(
            refreshed_consumer.resolved_registry_dependencies[0].version,
            "1.0.0"
        );

        let exact_root = root.0.join("exact-consumer");
        fs::create_dir_all(exact_root.join("src")).expect("create exact consumer source");
        fs::write(
            exact_root.join("ku.mod"),
            format!(
                "name = \"exact_consumer\"\nregistry.url = \"{}\"\nregistry.public_key = \"{}\"\ndep.math = \"1.1.0\"\n",
                server.base_url, server.public_key
            ),
        )
        .expect("write exact consumer manifest");
        fs::write(exact_root.join("src/main.ku"), "fn main() {}\n")
            .expect("write exact consumer source");
        let mut exact_consumer = discover_from_dir(&exact_root)
            .expect("discover exact consumer")
            .expect("exact consumer package");
        let error = resolve_remote_dependencies_with_mode(
            &mut exact_consumer,
            DependencyResolveMode::Refresh,
        )
        .expect_err("fresh exact resolution must not select a yanked version");
        assert_eq!(error.code.as_deref(), Some("dependency_conflict"));

        yank_package(&math, "token-math").expect("yank the last visible version");
        yank_package(&math, "token-math").expect("repeat the last-version yank");
        let empty_index = server
            .agent
            .get(&format!("{}packages/math/index.toml", server.base_url))
            .call()
            .expect("read empty signed index")
            .into_string()
            .expect("read empty signed index body");
        assert_eq!(empty_index, "name = \"math\"\n");
        let signature = server
            .agent
            .get(&format!("{}packages/math/index.toml.sig", server.base_url))
            .call()
            .expect("read empty index signature")
            .into_string()
            .expect("read empty index signature body");
        assert!(signature.starts_with("ed25519-"));
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            200
        );
        let still_empty = server
            .agent
            .get(&format!("{}packages/math/index.toml", server.base_url))
            .call()
            .expect("read index after duplicate publish")
            .into_string()
            .expect("read index after duplicate publish body");
        assert_eq!(still_empty, empty_index);
        server.stop();
    }

    #[test]
    fn registry_http_parser_rejects_smuggling_forms() {
        let invalid = [
            "PUT /v1/packages/math/1.0.0 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            "PUT /v1/packages/math/1.0.0 HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n",
            "GET /v1/packages/math/index.toml HTTP/1.1\r\n folded: value\r\n\r\n",
            "GET https://localhost/v1/packages/math/index.toml HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET /v1/packages/%6dath/index.toml HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET /v1/packages\\math\\index.toml HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET /v1/packages/math/index.toml HTTP/1.1\r\n\r\n",
            "GET /v1/packages/math/index.toml HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\n\r\n",
            "GET /v1/packages/math/index.toml HTTP/1.1\r\nHost: localhost\r\nConnection: close,,keep-alive\r\n\r\n",
        ];
        for request in invalid {
            assert!(
                read_request_head(&mut BufReader::new(request.as_bytes())).is_err(),
                "parser accepted unsafe request: {request:?}"
            );
        }
    }

    #[test]
    fn registry_test_http_response_reader_rejects_untrusted_lengths_before_allocation() {
        let responses = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_TEST_HTTP_RESPONSE_BODY_BYTES + 1
            ),
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n".to_string(),
            "HTTP/1.1 200 OK\r\nContent-Length: 00\r\n\r\n".to_string(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                "9".repeat(128)
            ),
        ];

        for response in responses {
            let mut reader = BufReader::new(std::io::Cursor::new(response.into_bytes()));
            let error = read_test_http_response(&mut reader)
                .expect_err("an untrusted content length must fail before reading a body");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn registry_test_http_response_reader_bounds_header_count_and_bytes() {
        let mut too_many_headers = String::from("HTTP/1.1 200 OK\r\n");
        for index in 0..MAX_TEST_HTTP_RESPONSE_HEADERS {
            too_many_headers.push_str(&format!("X-Test-{index}: value\r\n"));
        }
        too_many_headers.push_str("Content-Length: 0\r\n\r\n");
        let mut reader = BufReader::new(std::io::Cursor::new(too_many_headers.into_bytes()));
        let error = read_test_http_response(&mut reader)
            .expect_err("the response header count must be bounded");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too many headers"));

        let mut oversized_headers = String::from("HTTP/1.1 200 OK\r\n");
        for index in 0..3 {
            oversized_headers.push_str(&format!("X-Large-{index}: {}\r\n", "a".repeat(6_000)));
        }
        oversized_headers.push_str("Content-Length: 0\r\n\r\n");
        let mut reader = BufReader::new(std::io::Cursor::new(oversized_headers.into_bytes()));
        let error = read_test_http_response(&mut reader)
            .expect_err("the cumulative response header bytes must be bounded");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("budget"));

        let mut reader = BufReader::new(std::io::Cursor::new(
            b"HTTP/1.1 200 OK\r\nX-Test: value\x7f\r\nContent-Length: 0\r\n\r\n".to_vec(),
        ));
        let error = read_test_http_response(&mut reader)
            .expect_err("ASCII DEL must not be accepted in a response header value");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("malformed"));
    }

    #[test]
    fn registry_test_http_response_reader_has_an_absolute_drip_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind malicious response stream");
        let address = listener
            .local_addr()
            .expect("read malicious response stream address");
        let writer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept malicious response stream");
            stream
                .set_nodelay(true)
                .expect("disable malicious response stream Nagle buffering");
            for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                .iter()
                .copied()
                .take(24)
            {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let stream = TcpStream::connect(address).expect("connect malicious response stream");
        stream
            .set_nodelay(true)
            .expect("disable response reader Nagle buffering");
        let mut reader = BufReader::new(stream);
        let started = Instant::now();
        let error = read_test_http_response_with_timeout(&mut reader, Duration::from_millis(70))
            .expect_err("a continuous drip must not refresh the absolute response deadline");
        let elapsed = started.elapsed();
        drop(reader);
        writer.join().expect("malicious response writer panicked");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(1),
            "the 70ms response deadline took {elapsed:?}"
        );
    }

    #[test]
    fn registry_http_connection_semantics_are_canonical() {
        let http11 = read_request_head(&mut BufReader::new(
            b"GET /v1/packages/math/index.toml HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice(),
        ))
        .expect("HTTP/1.1 defaults to persistent");
        assert!(http11.keep_alive);
        let http11_close = read_request_head(&mut BufReader::new(
            b"GET /v1/packages/math/index.toml HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".as_slice(),
        ))
        .expect("HTTP/1.1 close request parses");
        assert!(!http11_close.keep_alive);
        for request in [
            b"GET /v1/packages/math/index.toml HTTP/1.0\r\n\r\n".as_slice(),
            b"GET /v1/packages/math/index.toml HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"
                .as_slice(),
        ] {
            assert!(
                read_request_head(&mut BufReader::new(request)).is_err(),
                "the registry has one wire protocol: HTTP/1.1"
            );
        }
    }

    #[test]
    fn registry_rejected_put_does_not_wait_for_an_untrusted_body() {
        let root = TestRoot::new("rejected-put-no-drain");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
        let certificate = files.certificate.clone();
        let mut server = TestServer::start(files.config, files.certificate);
        let mut stream = connect_test_tls(&server.base_url, certificate);
        let checksum = format!("sha256-{}", "0".repeat(64));
        write!(
            stream.get_mut(),
            "PUT /v1/packages/math/1.0.0 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer invalid-token\r\nContent-Type: {PACKAGE_CONTENT_TYPE}\r\nContent-Length: {MAX_REGISTRY_DOWNLOAD_BYTES}\r\nX-Ku-Checksum: {checksum}\r\nIdempotency-Key: math-1.0.0-{checksum}\r\n\r\n"
        )
        .expect("write rejected PUT headers without its declared body");
        stream
            .get_mut()
            .flush()
            .expect("flush rejected PUT headers");

        let (status, headers, _) =
            read_test_http_response(&mut stream).expect("read rejected PUT response");
        assert_eq!(status, 401);
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
        assert!(
            stream
                .fill_buf()
                .expect("read rejected PUT TLS close")
                .is_empty(),
            "an unauthenticated PUT must close without draining its declared body"
        );
        server.stop();
    }

    #[test]
    fn registry_real_package_client_reuses_one_tls_connection_for_publish_verification() {
        let root = TestRoot::new("client-keep-alive");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
        let mut server = TestServer::start(files.config, files.certificate);
        let package = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        assert_eq!(server.accepted_connections.load(Ordering::Relaxed), 0);
        {
            let agent_guard =
                TestRegistryHttpAgentGuard::install(server.base_url.clone(), server.agent.clone());
            publish_package(&package, "token-math")
                .expect("real package client publishes and verifies the signed index");
            assert_eq!(
                agent_guard.matched_client_count(),
                1,
                "publish and signed-index verification must share one operation-scoped client"
            );
        }
        assert_eq!(
            server.accepted_connections.load(Ordering::Relaxed),
            1,
            "PUT, index signature, and index must reuse one TCP/TLS connection"
        );
        server.stop();
    }

    #[test]
    fn registry_real_tls_keep_alive_is_reused_bounded_and_exits() {
        let root = TestRoot::new("keep-alive");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
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
        let artifact = pack_package(&package).expect("pack keep-alive test package");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201
        );

        let mut reused = connect_test_tls(&server.base_url, certificate.clone());
        write_test_get(reused.get_mut(), "/v1/packages/math/index.toml", None);
        let (status, headers, index) =
            read_test_http_response(&mut reused).expect("read reused index response");
        assert_eq!(status, 200);
        assert_eq!(
            headers.get("connection").map(String::as_str),
            Some("keep-alive")
        );
        assert!(!index.is_empty());
        write_test_get(
            reused.get_mut(),
            "/v1/packages/math/index.toml.sig",
            Some("close"),
        );
        let (status, headers, signature) =
            read_test_http_response(&mut reused).expect("read reused signature response");
        assert_eq!(status, 200);
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
        assert!(signature.starts_with(b"ed25519-"));
        assert!(
            reused
                .fill_buf()
                .expect("read TLS close notification")
                .is_empty(),
            "Connection: close must end the reused TLS stream"
        );

        let mut rejected = connect_test_tls(&server.base_url, certificate.clone());
        rejected
            .get_mut()
            .write_all(
                b"GET /v1/packages/math/index.toml HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx",
            )
            .expect("write rejected request with an unsafe unread body");
        rejected.get_mut().flush().expect("flush rejected request");
        let (status, headers, _) =
            read_test_http_response(&mut rejected).expect("read rejected keep-alive response");
        assert_eq!(status, 400);
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
        assert!(
            rejected
                .fill_buf()
                .expect("read rejected TLS close")
                .is_empty(),
            "a request with an unconsumed body must not be reused"
        );

        let mut capped = connect_test_tls(&server.base_url, certificate.clone());
        for request_number in 1..=MAX_REQUESTS_PER_CONNECTION {
            write_test_get(capped.get_mut(), "/v1/packages/math/index.toml.sig", None);
            let (status, headers, _) =
                read_test_http_response(&mut capped).expect("read capped keep-alive response");
            assert_eq!(status, 200);
            let expected_remaining = MAX_REQUESTS_PER_CONNECTION - request_number;
            if expected_remaining == 0 {
                assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
            } else {
                assert_eq!(
                    headers.get("connection").map(String::as_str),
                    Some("keep-alive")
                );
                assert_eq!(
                    headers.get("keep-alive").map(String::as_str),
                    Some(
                        format!(
                            "timeout={}, max={expected_remaining}",
                            KEEP_ALIVE_IDLE_TIMEOUT.as_secs()
                        )
                        .as_str()
                    )
                );
            }
        }
        assert!(
            capped.fill_buf().expect("read capped TLS close").is_empty(),
            "the request cap must end the TLS stream"
        );

        let mut idle = connect_test_tls(&server.base_url, certificate);
        write_test_get(idle.get_mut(), "/v1/packages/math/index.toml", None);
        let (status, headers, _) =
            read_test_http_response(&mut idle).expect("read initial idle response");
        assert_eq!(status, 200);
        assert_eq!(
            headers.get("connection").map(String::as_str),
            Some("keep-alive")
        );
        idle.get_mut()
            .write_all(b"G")
            .expect("start a deliberately incomplete keep-alive request");
        idle.get_mut()
            .flush()
            .expect("flush incomplete keep-alive request");
        let started = Instant::now();
        server.stop();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a slow keep-alive request header must not delay shutdown beyond its one second bound"
        );
        let (status, headers, _) =
            read_test_http_response(&mut idle).expect("read timed-out idle response");
        assert_eq!(status, 400);
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
        assert!(
            idle.fill_buf().expect("read idle TLS close").is_empty(),
            "a timed-out keep-alive request header must close cleanly"
        );
    }

    #[test]
    fn registry_startup_clears_empty_first_publish_crash_state_once() {
        let root = TestRoot::new("empty-pending");
        let files = test_server_files(&root.0, &[("token-empty", "empty")], Duration::from_secs(2));
        let package_root = files.config.data_dir.join(PACKAGES_DIR).join("empty");
        let indexes = package_root.join(INDEX_GENERATIONS_DIR);
        fs::create_dir_all(package_root.join(VERSIONS_DIR)).expect("create empty versions root");
        fs::create_dir_all(&indexes).expect("create empty package index root");
        fs::write(package_root.join(PUBLISH_PENDING_FILE), b"1.0.0\n")
            .expect("write first-publish crash marker");
        let abandoned_generation = indexes.join("ab".repeat(32));
        fs::create_dir(&abandoned_generation).expect("create abandoned signed generation");
        fs::write(abandoned_generation.join("index.toml"), b"partial")
            .expect("write abandoned signed index");

        let first = RegistryServer::bind(files.config.clone())
            .expect("startup recovers crash before first version rename");
        assert!(!package_root.join(PUBLISH_PENDING_FILE).exists());
        assert!(!abandoned_generation.exists());
        drop(first);
        let second = RegistryServer::bind(files.config)
            .expect("clean restart does not repeatedly rescan first-publish crash state");
        drop(second);
    }

    #[test]
    fn registry_yank_pending_recovery_commits_only_after_the_tombstone_exists() {
        let root = TestRoot::new("yank-recovery");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(3));
        let mut server = TestServer::start(files.config.clone(), files.certificate.clone());
        let package = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&package).expect("pack yank recovery artifact");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201
        );
        server.stop();

        let package_root = files.config.data_dir.join(PACKAGES_DIR).join("math");
        let pending = package_root.join(PUBLISH_PENDING_FILE);
        fs::write(&pending, b"1.0.0\n").expect("simulate crash before yank tombstone");
        let mut before_marker = TestServer::start(files.config.clone(), files.certificate.clone());
        let visible_index = before_marker
            .agent
            .get(&format!(
                "{}packages/math/index.toml",
                before_marker.base_url
            ))
            .call()
            .expect("read recovered pre-marker index")
            .into_string()
            .expect("read recovered pre-marker index body");
        assert!(visible_index.contains("version = \"1.0.0\""));
        assert!(!pending.exists());
        before_marker.stop();

        fs::write(&pending, b"1.0.0\n").expect("simulate committed yank recovery marker");
        let yanks_root = fs::canonicalize(package_root.join(YANKS_DIR))
            .expect("canonical yank recovery storage");
        create_yank_marker(&yanks_root, "1.0.0").expect("create committed yank tombstone");
        let mut after_marker = TestServer::start(files.config.clone(), files.certificate.clone());
        let withdrawn_index = after_marker
            .agent
            .get(&format!(
                "{}packages/math/index.toml",
                after_marker.base_url
            ))
            .call()
            .expect("read recovered post-marker index")
            .into_string()
            .expect("read recovered post-marker index body");
        assert_eq!(withdrawn_index, "name = \"math\"\n");
        assert!(!pending.exists());
        assert!(yanks_root.join("1.0.0").is_dir());
        assert_eq!(
            response_status(
                after_marker
                    .agent
                    .get(&format!(
                        "{}artifacts/math-1.0.0-{}.tar.zst",
                        after_marker.base_url, artifact.checksum
                    ))
                    .call(),
            ),
            200
        );
        after_marker.stop();

        fs::create_dir(yanks_root.join("9.9.9"))
            .expect("create unknown tombstone for startup audit");
        let error = RegistryServer::bind(files.config)
            .err()
            .expect("startup must reject a tombstone for an unknown physical version");
        assert_eq!(error.code.as_deref(), Some("registry_startup_audit_failed"));
        assert!(error.message.contains("missing version"));
    }

    #[test]
    fn registry_startup_rejects_visible_plus_yanked_versions_over_physical_limit() {
        let root = TestRoot::new("physical-version-limit");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(5));
        let storage = RegistryStorage::open(&files.config.data_dir)
            .expect("initialize physical-limit registry storage");
        let package_root =
            package_storage_root(&storage, "math").expect("create physical-limit package root");
        let versions_root = ensure_request_child_directory(&package_root, VERSIONS_DIR)
            .expect("create physical-limit version storage");
        let yanks_root = ensure_request_child_directory(&package_root, YANKS_DIR)
            .expect("create physical-limit yank storage");
        ensure_request_child_directory(&package_root, INDEX_GENERATIONS_DIR)
            .expect("create physical-limit index storage");

        let visible = (0..MAX_REGISTRY_INDEX_VERSIONS)
            .map(|patch| EntryMetadata {
                name: "math".to_string(),
                version: format!("0.0.{patch}"),
                checksum: format!("sha256-{}", "a".repeat(64)),
                dependencies: Vec::new(),
            })
            .collect::<Vec<_>>();
        let signed = generate_signed_index(&SigningKey::from_bytes(&[0x11; 32]), "math", visible)
            .expect("generate an index at the exact visible-version limit");
        persist_signed_generation(&package_root, &signed)
            .expect("persist physical-limit signed index");
        switch_signed_index(&package_root, &signed.generation)
            .expect("select physical-limit signed index");

        fs::create_dir(versions_root.join("1.0.0"))
            .expect("create physical yanked version directory");
        create_yank_marker(&yanks_root, "1.0.0").expect("create extra yank marker");
        drop(storage);

        let error = RegistryServer::bind(files.config)
            .err()
            .expect("startup must reject visible plus yanked versions above the physical limit");
        assert_eq!(error.code.as_deref(), Some("registry_startup_audit_failed"));
        assert!(error.message.contains("physical package version count"));
    }

    #[test]
    fn registry_clean_startup_rejects_hidden_physical_version() {
        let root = TestRoot::new("hidden-physical-version");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(2));
        let visible = test_entry_metadata("math", "1.0.0", 'a');
        let hidden = test_entry_metadata("math", "2.0.0", 'b');
        install_clean_test_package_storage(
            &files.config,
            "math",
            &[visible.clone(), hidden],
            vec![visible],
            &[],
        );

        let error = RegistryServer::bind(files.config)
            .err()
            .expect("clean startup must reject a physical version hidden from the signed state");
        assert_eq!(error.code.as_deref(), Some("registry_startup_audit_failed"));
        assert!(
            error
                .message
                .contains("absent from the signed index and yank storage"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn registry_clean_startup_accepts_exact_visible_and_yanked_physical_set() {
        let root = TestRoot::new("exact-clean-version-set");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(2));
        let visible = test_entry_metadata("math", "1.0.0", 'a');
        let yanked = test_entry_metadata("math", "2.0.0", 'b');
        install_clean_test_package_storage(
            &files.config,
            "math",
            &[visible.clone(), yanked],
            vec![visible],
            &["2.0.0"],
        );

        let server = RegistryServer::bind(files.config)
            .expect("clean startup must accept an exact visible and yanked physical set");
        drop(server);
    }

    #[test]
    fn registry_clean_startup_rejects_visible_version_without_physical_storage() {
        let root = TestRoot::new("missing-visible-version");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(2));
        let visible = test_entry_metadata("math", "1.0.0", 'a');
        install_clean_test_package_storage(&files.config, "math", &[], vec![visible], &[]);

        let error = RegistryServer::bind(files.config)
            .err()
            .expect("clean startup must reject a signed version without physical storage");
        assert_eq!(error.code.as_deref(), Some("registry_startup_audit_failed"));
        assert!(
            error
                .message
                .contains("references missing physical version 1.0.0"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn registry_clean_startup_rejects_visible_metadata_mismatch() {
        let root = TestRoot::new("visible-metadata-mismatch");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(2));
        let physical = test_entry_metadata("math", "1.0.0", 'a');
        let indexed = test_entry_metadata("math", "1.0.0", 'b');
        install_clean_test_package_storage(&files.config, "math", &[physical], vec![indexed], &[]);

        let error = RegistryServer::bind(files.config)
            .err()
            .expect("clean startup must reject signed metadata that differs from physical storage");
        assert_eq!(error.code.as_deref(), Some("registry_startup_audit_failed"));
        assert!(
            error
                .message
                .contains("metadata does not match physical version 1.0.0"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn yank_tombstones_and_mutation_limits_fail_closed() {
        let root = TestRoot::new("yank-safety");
        let package_root = root.0.join("package");
        let versions_root = package_root.join(VERSIONS_DIR);
        let yanks_root = package_root.join(YANKS_DIR);
        fs::create_dir_all(versions_root.join("1.0.0"))
            .expect("create physical version for yank safety test");
        fs::create_dir_all(&yanks_root).expect("create yank safety storage");
        let yanks_root = fs::canonicalize(yanks_root).expect("canonical yank safety storage");

        let non_empty = yanks_root.join("1.0.0");
        fs::create_dir(&non_empty).expect("create non-empty yank marker");
        fs::write(non_empty.join("unexpected"), b"x").expect("fill yank marker");
        assert!(validate_yank_marker_directory(&non_empty, &yanks_root).is_err());
        fs::remove_dir_all(&non_empty).expect("remove non-empty yank marker");

        let symlink_target = root.0.join("symlink-target");
        fs::create_dir(&symlink_target).expect("create yank symlink target");
        let symlink_marker = yanks_root.join("1.0.0");
        #[cfg(unix)]
        let symlink_created = std::os::unix::fs::symlink(&symlink_target, &symlink_marker).is_ok();
        #[cfg(windows)]
        let symlink_created =
            std::os::windows::fs::symlink_dir(&symlink_target, &symlink_marker).is_ok();
        #[cfg(not(any(unix, windows)))]
        let symlink_created = false;
        if symlink_created {
            assert!(validate_yank_marker_directory(&symlink_marker, &yanks_root).is_err());
            #[cfg(windows)]
            fs::remove_dir(&symlink_marker).expect("remove yank marker directory symlink");
            #[cfg(not(windows))]
            fs::remove_file(&symlink_marker).expect("remove yank marker symlink");
        }

        fs::create_dir(yanks_root.join("9.9.9")).expect("create unknown yank marker");
        let canonical_package = fs::canonicalize(&package_root).expect("canonical package root");
        let canonical_versions = fs::canonicalize(&versions_root).expect("canonical versions root");
        let failure = load_yanked_versions(
            &canonical_package,
            &canonical_versions,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("an unknown yank marker must fail closed");
        assert_eq!(failure.status, 500);

        ensure_physical_version_capacity(MAX_REGISTRY_INDEX_VERSIONS - 1, 0)
            .expect("the last physical package version slot remains usable");
        ensure_physical_version_capacity(0, MAX_REGISTRY_INDEX_VERSIONS - 1)
            .expect("yanked versions count against the same physical limit");
        assert_eq!(
            ensure_physical_version_capacity(MAX_REGISTRY_INDEX_VERSIONS - 1, 1)
                .expect_err("yanking must not free a physical version slot")
                .status,
            409
        );

        let admission = MutationAdmission::new();
        let guards = ["one", "two", "three", "four"]
            .map(|name| admission.try_acquire(name).expect("fill mutation capacity"));
        assert_eq!(
            admission
                .try_acquire("five")
                .err()
                .expect("one request beyond the mutation limit must receive 429")
                .status,
            429
        );
        drop(guards);
        assert_eq!(
            admission.active_packages.lock().unwrap().len(),
            0,
            "all mutation slots must be released by their guards"
        );
    }

    #[test]
    fn registry_data_directory_has_one_lifetime_owner() {
        let root = TestRoot::new("instance-lock");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(2));
        let first = RegistryServer::bind(files.config.clone()).expect("bind first registry owner");
        let err = RegistryServer::bind(files.config.clone())
            .err()
            .expect("second registry owner must be rejected");
        assert_eq!(err.code.as_deref(), Some("registry_instance_locked"));
        drop(first);
        let restarted = RegistryServer::bind(files.config)
            .expect("registry data directory lock must release with its owner");
        drop(restarted);
    }

    #[test]
    fn registry_clean_restart_removes_orphan_index_generation() {
        let root = TestRoot::new("orphan-generation");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(3));
        let mut server = TestServer::start(files.config.clone(), files.certificate.clone());
        let package = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&package).expect("pack orphan cleanup artifact");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201
        );
        server.stop();

        let package_root = files.config.data_dir.join(PACKAGES_DIR).join("math");
        let current = fs::read_to_string(package_root.join(INDEX_CURRENT_FILE))
            .expect("read current signed index generation");
        let current = current.trim();
        let orphan_name = if current.eq_ignore_ascii_case(&"ab".repeat(32)) {
            "cd".repeat(32)
        } else {
            "ab".repeat(32)
        };
        let orphan = package_root.join(INDEX_GENERATIONS_DIR).join(orphan_name);
        fs::create_dir(&orphan).expect("create orphan signed index generation");
        fs::write(orphan.join("index.toml"), b"uncommitted generation")
            .expect("write orphan signed index content");

        let restarted = RegistryServer::bind(files.config)
            .expect("clean restart must tolerate and clean an orphan generation");
        assert!(
            !orphan.exists(),
            "startup must remove the orphan generation"
        );
        assert!(
            package_root
                .join(INDEX_GENERATIONS_DIR)
                .join(current)
                .is_dir(),
            "startup must preserve the committed generation"
        );
        drop(restarted);
    }

    #[test]
    fn old_index_generation_cleanup_refuses_an_unbounded_tree() {
        let root = TestRoot::new("orphan-generation-tree-limit");
        let package_root = root.0.join("package");
        let indexes = package_root.join(INDEX_GENERATIONS_DIR);
        fs::create_dir_all(&indexes).expect("create signed index storage");
        let package_root = fs::canonicalize(package_root).expect("canonical package root");
        let current = "11".repeat(32);
        let orphan = package_root
            .join(INDEX_GENERATIONS_DIR)
            .join("22".repeat(32));
        fs::create_dir(package_root.join(INDEX_GENERATIONS_DIR).join(&current))
            .expect("create current generation");
        fs::create_dir(&orphan).expect("create oversized orphan generation");
        let mut nested = orphan.clone();
        for depth in 0..35 {
            nested = nested.join(format!("d{depth}"));
            fs::create_dir(&nested).expect("create oversized orphan tree");
        }

        let failure = cleanup_old_index_generations(&package_root, &current)
            .expect_err("cleanup must refuse an old generation with an unbounded tree");
        assert_eq!(failure.status, 500);
        assert!(failure
            .internal
            .as_deref()
            .is_some_and(|message| message.contains("cleanup safety limits")));
        assert!(
            orphan.is_dir(),
            "refused orphan tree must remain for manual repair"
        );
        assert!(
            package_root
                .join(INDEX_GENERATIONS_DIR)
                .join(current)
                .is_dir(),
            "cleanup must preserve the current generation"
        );
    }

    #[test]
    fn damaged_cache_rebuild_defers_historical_artifact_failure_to_get() {
        let root = TestRoot::new("damaged-cache");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(3));
        let mut server = TestServer::start(files.config.clone(), files.certificate.clone());
        let package = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&package).expect("pack recovery test artifact");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201
        );
        server.stop();

        let package_root = files.config.data_dir.join(PACKAGES_DIR).join("math");
        let artifact_path = package_root
            .join(VERSIONS_DIR)
            .join("1.0.0")
            .join(artifact_storage_name(&artifact.checksum).expect("artifact storage name"));
        fs::write(&artifact_path, b"tampered historical artifact")
            .expect("tamper historical artifact");
        fs::write(package_root.join(INDEX_CURRENT_FILE), b"broken\n")
            .expect("damage signed index pointer");

        let mut restarted = TestServer::start(files.config.clone(), files.certificate.clone());
        assert_eq!(
            response_status(
                restarted
                    .agent
                    .get(&format!(
                        "{}artifacts/math-1.0.0-{}.tar.zst",
                        restarted.base_url, artifact.checksum
                    ))
                    .call(),
            ),
            500,
            "artifact GET must still fail closed on historical checksum corruption"
        );
        restarted.stop();
    }

    #[test]
    fn pending_version_artifact_corruption_blocks_startup_recovery() {
        let root = TestRoot::new("pending-corrupt");
        let files = test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(3));
        let mut server = TestServer::start(files.config.clone(), files.certificate.clone());
        let package = write_test_package(
            &root.0.join("math"),
            "math",
            "1.0.0",
            &server.base_url,
            &server.public_key,
            "fn Value(): int { return 1 }\n",
        );
        let artifact = pack_package(&package).expect("pack pending recovery artifact");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201
        );
        server.stop();

        let package_root = files.config.data_dir.join(PACKAGES_DIR).join("math");
        let artifact_path = package_root
            .join(VERSIONS_DIR)
            .join("1.0.0")
            .join(artifact_storage_name(&artifact.checksum).expect("artifact storage name"));
        fs::write(&artifact_path, b"tampered pending artifact").expect("tamper pending artifact");
        fs::write(package_root.join(PUBLISH_PENDING_FILE), b"1.0.0\n")
            .expect("write pending recovery marker");

        let err = RegistryServer::bind(files.config)
            .err()
            .expect("pending artifact corruption must block startup recovery");
        assert_eq!(err.code.as_deref(), Some("registry_startup_audit_failed"));
        assert!(err.message.contains("checksum"), "unexpected error: {err}");
    }

    #[test]
    #[ignore = "local bounded TLS load gate; run explicitly with --ignored --nocapture"]
    fn registry_tls_concurrency_keep_alive_and_overload_load_gate() {
        const WORKERS: usize = 4;
        const QUEUE_CAPACITY: usize = 8;
        const KEEP_ALIVE_CLIENTS: usize = 8;
        const REQUESTS_PER_KEEP_ALIVE_CLIENT: usize = 4;
        const OVERLOAD_CLIENTS: usize = 24;
        const TOTAL_GATE_TIMEOUT: Duration = Duration::from_secs(20);

        let root = TestRoot::new("bounded-load-gate");
        let mut files =
            test_server_files(&root.0, &[("token-math", "math")], Duration::from_secs(3));
        files.config.workers = WORKERS;
        files.config.queue_capacity = QUEUE_CAPACITY;
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
        let artifact = pack_package(&package).expect("pack load-gate package");
        assert_eq!(
            put_artifact(
                &server.agent,
                &server.base_url,
                "token-math",
                &artifact,
                &artifact.checksum,
            ),
            201
        );

        // The seed publish uses the same persistent protocol as real clients. Let its
        // one-second idle lease expire so it cannot consume a load-gate worker.
        thread::sleep(KEEP_ALIVE_IDLE_TIMEOUT + Duration::from_millis(50));
        let accepted_before_load = server.accepted_connections.load(Ordering::Acquire);
        let load_started = Instant::now();
        let total_deadline = load_started + TOTAL_GATE_TIMEOUT;

        let keep_alive_deadline =
            std::cmp::min(total_deadline, Instant::now() + Duration::from_secs(8));
        let keep_alive = run_registry_load_phase(
            &server.base_url,
            certificate.clone(),
            KEEP_ALIVE_CLIENTS,
            REQUESTS_PER_KEEP_ALIVE_CLIENT,
            keep_alive_deadline,
        );
        assert_eq!(
            keep_alive.success,
            KEEP_ALIVE_CLIENTS * REQUESTS_PER_KEEP_ALIVE_CLIENT,
            "the bounded keep-alive phase must complete every GET"
        );
        assert_eq!(keep_alive.rejected, 0);
        assert_eq!(keep_alive.internal_errors, 0);
        let accepted_after_keep_alive = server.accepted_connections.load(Ordering::Acquire);
        assert_eq!(
            accepted_after_keep_alive - accepted_before_load,
            KEEP_ALIVE_CLIENTS,
            "each concurrent client must reuse exactly one TLS connection"
        );

        // Occupy every worker with a bounded, incomplete TLS handshake, then fill the
        // finite queue. The following real TLS GET clients must be rejected quickly
        // instead of allocating more workers or waiting without a deadline.
        let address = registry_test_address(&server.base_url);
        let mut stalled = Vec::with_capacity(WORKERS);
        for _ in 0..WORKERS {
            stalled.push(
                TcpStream::connect_timeout(&address, Duration::from_secs(1))
                    .expect("open bounded active blocker"),
            );
        }
        wait_for_registry_accepts(
            &server.accepted_connections,
            accepted_after_keep_alive + WORKERS,
            std::cmp::min(total_deadline, Instant::now() + Duration::from_secs(2)),
        );
        thread::sleep(Duration::from_millis(50));

        let mut queued = Vec::with_capacity(QUEUE_CAPACITY);
        for _ in 0..QUEUE_CAPACITY {
            queued.push(
                TcpStream::connect_timeout(&address, Duration::from_secs(1))
                    .expect("open bounded queued blocker"),
            );
        }
        wait_for_registry_accepts(
            &server.accepted_connections,
            accepted_after_keep_alive + WORKERS + QUEUE_CAPACITY,
            std::cmp::min(total_deadline, Instant::now() + Duration::from_secs(2)),
        );

        let overload_deadline =
            std::cmp::min(total_deadline, Instant::now() + Duration::from_secs(2));
        let overload = run_registry_load_phase(
            &server.base_url,
            certificate,
            OVERLOAD_CLIENTS,
            1,
            overload_deadline,
        );
        assert_eq!(overload.internal_errors, 0);
        assert!(
            overload.rejected > 0,
            "a saturated bounded queue must reject at least one excess connection"
        );

        let elapsed = load_started.elapsed();
        let mut total = RegistryLoadStats::default();
        total.add(keep_alive);
        total.add(overload);
        let planned_requests =
            KEEP_ALIVE_CLIENTS * REQUESTS_PER_KEEP_ALIVE_CLIENT + OVERLOAD_CLIENTS;
        assert_eq!(total.classified(), planned_requests);
        let accepted = server
            .accepted_connections
            .load(Ordering::Acquire)
            .saturating_sub(accepted_before_load);
        let throughput = total.classified() as f64 / elapsed.as_secs_f64();
        eprintln!(
            "registry_load_gate elapsed_ms={} throughput_req_s={throughput:.1} success={} rejected={} internal_errors={} accepted_connections={accepted}",
            elapsed.as_millis(),
            total.success,
            total.rejected,
            total.internal_errors,
        );

        drop(queued);
        drop(stalled);
        server.stop();
        assert!(
            server.thread.is_none(),
            "the server and all joined worker threads must exit"
        );
        let staging = data_dir.join(STAGING_DIR);
        assert!(
            fs::read_dir(&staging)
                .expect("inspect registry staging after load gate")
                .next()
                .is_none(),
            "the load gate must leave no temporary registry staging entries"
        );
        fs::remove_dir_all(&root.0)
            .expect("all registry test resources must be releasable after shutdown");
        assert!(!root.0.exists());
    }

    #[test]
    fn registry_slow_tls_handshake_obeys_absolute_deadline_and_shutdown_is_bounded() {
        let root = TestRoot::new("deadline");
        let files = test_server_files(
            &root.0,
            &[("token-math", "math")],
            Duration::from_millis(300),
        );
        let mut server = TestServer::start(files.config, files.certificate);
        let stream = TcpStream::connect(registry_test_address(&server.base_url))
            .expect("open stalled TLS socket");
        thread::sleep(Duration::from_millis(500));
        let _ = stream.shutdown(Shutdown::Both);
        let started = Instant::now();
        server.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
