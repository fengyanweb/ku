use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    error::{KuError, KuResult},
    package::{
        validate_package_name, MAX_PUBLISH_TOKEN_BYTES, MAX_REGISTRY_INDEX_VERSIONS,
        MAX_REGISTRY_LINE_BYTES, REGISTRY_TOKEN_ENV,
    },
    span::Span,
};

pub const REGISTRY_CREDENTIALS_FILE_ENV: &str = "KU_REGISTRY_CREDENTIALS_FILE";
pub const REGISTRY_USAGE: &str = "ku-registry\nku-registry token issue <exact-package-name>\nku-registry token revoke <exact-package-name>\nku-registry --help";

const MAX_CREDENTIAL_FILE_BYTES: u64 = 16 * 1024;
const TOKEN_RANDOM_BYTES: usize = 32;
const ADMIN_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const ADMIN_LOCK_POLL: Duration = Duration::from_millis(10);
const TOKEN_GENERATION_ATTEMPTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialRecord {
    pub(crate) token_hash: [u8; 32],
    pub(crate) package_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryCommand {
    Serve,
    Help,
    Issue { package_name: String },
    Revoke { package_name: String },
}

pub fn parse_registry_command(
    arguments: impl IntoIterator<Item = OsString>,
) -> KuResult<RegistryCommand> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.is_empty() {
        return Ok(RegistryCommand::Serve);
    }
    if arguments.as_slice() == [OsString::from("--help")] {
        return Ok(RegistryCommand::Help);
    }
    let usage = format!("usage:\n{REGISTRY_USAGE}");
    if arguments.len() != 3 || arguments[0] != "token" {
        return Err(admin_error("invalid_registry_command", &usage));
    }
    let action = arguments[1]
        .to_str()
        .ok_or_else(|| admin_error("invalid_registry_command", &usage))?;
    let package_name = arguments[2]
        .to_str()
        .ok_or_else(|| {
            admin_error(
                "invalid_registry_package_name",
                "registry package name must be valid UTF-8",
            )
        })?
        .to_string();
    validate_admin_package_name(&package_name)?;
    match action {
        "issue" => Ok(RegistryCommand::Issue { package_name }),
        "revoke" => Ok(RegistryCommand::Revoke { package_name }),
        _ => Err(admin_error("invalid_registry_command", usage)),
    }
}

pub fn issue_token_from_env(package_name: &str) -> KuResult<String> {
    let credentials_path = required_credentials_path()?;
    issue_token(&credentials_path, package_name, ADMIN_LOCK_TIMEOUT)
}

pub fn revoke_token_from_env(package_name: &str) -> KuResult<()> {
    let credentials_path = required_credentials_path()?;
    let token = env::var(REGISTRY_TOKEN_ENV).map_err(|_| {
        admin_error(
            "missing_registry_token",
            format!("required registry environment variable {REGISTRY_TOKEN_ENV} is not set"),
        )
    })?;
    validate_presented_token(&token)?;
    revoke_token(
        &credentials_path,
        package_name,
        token.as_bytes(),
        ADMIN_LOCK_TIMEOUT,
    )
}

pub fn write_issued_token(token: &str, output: &mut impl Write) -> KuResult<()> {
    validate_presented_token(token)?;
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    writeln!(output, "{token}")
        .and_then(|()| output.flush())
        .map_err(|err| {
            admin_error(
                "registry_token_output_failed",
                format!(
                    "issued credential sha256-{} was committed, but token output failed ({:?}); inspect and revoke this exact hash from the credentials file before retrying; the plaintext token is not available in diagnostics",
                    encode_hex(&token_hash), err.kind()
                ),
            )
        })
}

pub fn write_revocation_confirmation(package_name: &str, output: &mut impl Write) -> KuResult<()> {
    validate_admin_package_name(package_name)?;
    writeln!(output, "revoked {package_name}")
        .and_then(|()| output.flush())
        .map_err(|err| {
            admin_error(
                "registry_revocation_output_failed",
                format!(
                    "registry credential revocation was committed, but confirmation output failed ({:?}); a retry can report that the credential was already revoked",
                    err.kind()
                ),
            )
        })
}

pub(crate) fn parse_credential_records(bytes: &[u8]) -> KuResult<Vec<CredentialRecord>> {
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            format!(
                "registry credentials must be a regular file no larger than {MAX_CREDENTIAL_FILE_BYTES} bytes"
            ),
        ));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            "registry credentials file must be valid UTF-8",
        )
    })?;
    let mut credentials = Vec::new();
    let mut seen = HashSet::new();
    for (index, raw_line) in source.lines().enumerate() {
        let Some(record) = parse_credential_line(raw_line, index + 1)? else {
            continue;
        };
        if credentials.len() >= MAX_REGISTRY_INDEX_VERSIONS {
            return Err(admin_error(
                "invalid_registry_credentials",
                "registry credentials file has too many ACL entries",
            ));
        }
        if !seen.insert((record.token_hash, record.package_name.clone())) {
            return Err(admin_error(
                "invalid_registry_credentials",
                format!("registry credentials line {} is duplicated", index + 1),
            ));
        }
        credentials.push(record);
    }
    Ok(credentials)
}

fn parse_credential_line(raw_line: &str, line_number: usize) -> KuResult<Option<CredentialRecord>> {
    if raw_line.len() > MAX_REGISTRY_LINE_BYTES {
        return Err(admin_error(
            "invalid_registry_credentials",
            format!("registry credentials line {line_number} is too long"),
        ));
    }
    let line = raw_line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut fields = line.split_ascii_whitespace();
    let (Some(hash), Some(package_name), None) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(admin_error(
            "invalid_registry_credentials",
            format!(
                "registry credentials line {line_number} must be '<sha256-token-hash> <exact-package-name>'"
            ),
        ));
    };
    let Some(hash) = hash.strip_prefix("sha256-") else {
        return Err(invalid_hash_error(line_number));
    };
    let token_hash = decode_hex_array::<32>(hash).map_err(|_| invalid_hash_error(line_number))?;
    validate_package_name(package_name, Span::default()).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            format!("registry credentials line {line_number} has an invalid package name"),
        )
    })?;
    Ok(Some(CredentialRecord {
        token_hash,
        package_name: package_name.to_string(),
    }))
}

fn invalid_hash_error(line_number: usize) -> KuError {
    admin_error(
        "invalid_registry_credentials",
        format!("registry credentials line {line_number} has an invalid token hash"),
    )
}

fn issue_token(path: &Path, package_name: &str, timeout: Duration) -> KuResult<String> {
    issue_token_with_writer(path, package_name, timeout, write_credentials_atomically)
}

fn issue_token_with_writer(
    path: &Path,
    package_name: &str,
    timeout: Duration,
    writer: impl FnOnce(&Path, &[u8]) -> KuResult<()>,
) -> KuResult<String> {
    validate_admin_package_name(package_name)?;
    let path = normalize_credentials_path(path)?;
    let deadline = absolute_deadline(timeout)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    let records = parse_credential_records(&original)?;
    if records.len() >= MAX_REGISTRY_INDEX_VERSIONS {
        return Err(admin_error(
            "registry_credentials_full",
            "registry credentials file has reached its ACL entry limit",
        ));
    }

    for _ in 0..TOKEN_GENERATION_ATTEMPTS {
        let token = generate_token()?;
        let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if records.iter().any(|record| record.token_hash == token_hash) {
            continue;
        }
        let line = format!("sha256-{} {package_name}\n", encode_hex(&token_hash));
        let mut replacement = original.clone();
        if !replacement.is_empty() && !replacement.ends_with(b"\n") {
            replacement.push(b'\n');
        }
        replacement.extend_from_slice(line.as_bytes());
        validate_replacement(&replacement)?;
        writer(&path, &replacement).map_err(|err| {
            annotate_committed_credential_error(err, "issued", package_name, &token_hash)
        })?;
        return Ok(token);
    }
    Err(admin_error(
        "registry_random_failed",
        "failed to generate a unique registry token",
    ))
}

fn revoke_token(path: &Path, package_name: &str, token: &[u8], timeout: Duration) -> KuResult<()> {
    revoke_token_with_writer(
        path,
        package_name,
        token,
        timeout,
        write_credentials_atomically,
    )
}

fn revoke_token_with_writer(
    path: &Path,
    package_name: &str,
    token: &[u8],
    timeout: Duration,
    writer: impl FnOnce(&Path, &[u8]) -> KuResult<()>,
) -> KuResult<()> {
    validate_admin_package_name(package_name)?;
    let path = normalize_credentials_path(path)?;
    let deadline = absolute_deadline(timeout)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    parse_credential_records(&original)?;
    let token_hash: [u8; 32] = Sha256::digest(token).into();
    let source = std::str::from_utf8(&original).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            "registry credentials file must be valid UTF-8",
        )
    })?;
    let mut replacement = Vec::with_capacity(original.len());
    let mut removed = false;
    for (index, segment) in source.split_inclusive('\n').enumerate() {
        let raw_line = segment.strip_suffix('\n').unwrap_or(segment);
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let record = parse_credential_line(raw_line, index + 1)?;
        if record.as_ref().is_some_and(|record| {
            record.token_hash == token_hash && record.package_name == package_name
        }) {
            removed = true;
        } else {
            replacement.extend_from_slice(segment.as_bytes());
        }
    }
    if !removed {
        return Err(admin_error(
            "registry_credential_not_found",
            "the supplied registry token has no credential for the requested package; it may already have been revoked by a previous committed attempt",
        ));
    }
    if parse_credential_records(&replacement)?.is_empty() {
        return Err(admin_error(
            "registry_last_credential",
            "cannot revoke the last registry credential; issue a replacement credential first",
        ));
    }
    validate_replacement(&replacement)?;
    writer(&path, &replacement).map_err(|err| {
        annotate_committed_credential_error(err, "revoked", package_name, &token_hash)
    })
}

fn annotate_committed_credential_error(
    mut error: KuError,
    action: &str,
    package_name: &str,
    token_hash: &[u8; 32],
) -> KuError {
    if error.code.as_deref() == Some("registry_credentials_commit_uncertain") {
        error.message.push_str(&format!(
            "; {action} credential sha256-{} for package {package_name}; inspect this exact hash before retrying, since the replacement is already visible and its durable state is uncertain",
            encode_hex(token_hash)
        ));
    }
    error
}

fn validate_replacement(bytes: &[u8]) -> KuResult<()> {
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "registry_credentials_full",
            format!("registry credentials file would exceed {MAX_CREDENTIAL_FILE_BYTES} bytes"),
        ));
    }
    parse_credential_records(bytes).map(|_| ())
}

fn generate_token() -> KuResult<String> {
    let mut random = [0u8; TOKEN_RANDOM_BYTES];
    getrandom::fill(&mut random).map_err(|_| {
        admin_error(
            "registry_random_failed",
            "failed to generate a registry token from the operating system random source",
        )
    })?;
    Ok(format!("ku_{}", encode_hex(&random)))
}

fn validate_presented_token(token: &str) -> KuResult<()> {
    if token.is_empty()
        || token.len() > MAX_PUBLISH_TOKEN_BYTES
        || !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return Err(admin_error(
            "invalid_registry_token",
            format!(
                "{REGISTRY_TOKEN_ENV} must be a non-empty ASCII bearer token of at most {MAX_PUBLISH_TOKEN_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

fn validate_admin_package_name(package_name: &str) -> KuResult<()> {
    validate_package_name(package_name, Span::default()).map_err(|_| {
        admin_error(
            "invalid_registry_package_name",
            "registry token administration requires one exact valid package name",
        )
    })
}

fn required_credentials_path() -> KuResult<PathBuf> {
    let path = env::var_os(REGISTRY_CREDENTIALS_FILE_ENV).ok_or_else(|| {
        admin_error(
            "missing_registry_config",
            format!(
                "required registry environment variable {REGISTRY_CREDENTIALS_FILE_ENV} is not set"
            ),
        )
    })?;
    if path.is_empty() {
        return Err(admin_error(
            "invalid_registry_file",
            format!("{REGISTRY_CREDENTIALS_FILE_ENV} must not be empty"),
        ));
    }
    Ok(PathBuf::from(path))
}

fn normalize_credentials_path(path: &Path) -> KuResult<PathBuf> {
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            admin_error(
                "invalid_registry_file",
                "registry credentials path must name a file",
            )
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = match parent {
        Some(parent) => parent.to_path_buf(),
        None => env::current_dir().map_err(|err| {
            admin_error(
                "invalid_registry_file",
                format!("failed to resolve registry credentials directory: {err}"),
            )
        })?,
    };
    let metadata = fs::symlink_metadata(&parent).map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to inspect registry credentials directory: {err}"),
        )
    })?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(admin_error(
            "invalid_registry_file",
            "registry credentials directory must be a real directory",
        ));
    }
    let parent = fs::canonicalize(&parent).map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to resolve registry credentials directory: {err}"),
        )
    })?;
    Ok(parent.join(file_name))
}

fn read_credentials_if_present(path: &Path) -> KuResult<Vec<u8>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(admin_error(
                "invalid_registry_file",
                format!("failed to inspect registry credentials file: {err}"),
            ));
        }
    };
    validate_regular_metadata(&metadata, "credentials")?;
    if metadata.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            format!(
                "registry credentials must be a regular file no larger than {MAX_CREDENTIAL_FILE_BYTES} bytes"
            ),
        ));
    }
    let file = open_nofollow(path, false, false).map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to open registry credentials file: {err}"),
        )
    })?;
    let opened = file.metadata().map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to inspect opened registry credentials file: {err}"),
        )
    })?;
    validate_regular_metadata(&opened, "credentials")?;
    if opened.len() != metadata.len() || opened.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            "registry credentials file changed while it was opened",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| {
            admin_error(
                "invalid_registry_file",
                format!("failed to read registry credentials file: {err}"),
            )
        })?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            format!("registry credentials file exceeds {MAX_CREDENTIAL_FILE_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

#[derive(Debug)]
struct CredentialFileLock {
    file: fs::File,
}

impl CredentialFileLock {
    fn acquire(credentials_path: &Path, deadline: Instant) -> KuResult<Self> {
        let lock_path = sibling_lock_path(credentials_path)?;
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            validate_regular_metadata(&metadata, "credentials lock")?;
        }
        let file = open_nofollow(&lock_path, true, true).map_err(|err| {
            admin_error(
                "registry_credentials_lock_failed",
                format!("failed to open registry credentials lock: {err}"),
            )
        })?;
        let opened = file.metadata().map_err(|err| {
            admin_error(
                "registry_credentials_lock_failed",
                format!("failed to inspect registry credentials lock: {err}"),
            )
        })?;
        validate_regular_metadata(&opened, "credentials lock")?;
        ensure_lock_path_identity(&lock_path, &file)?;
        loop {
            if Instant::now() >= deadline {
                return Err(credentials_lock_timeout_error());
            }
            match file.try_lock() {
                Ok(()) => {
                    ensure_lock_path_identity(&lock_path, &file)?;
                    if Instant::now() >= deadline {
                        let _ = file.unlock();
                        return Err(credentials_lock_timeout_error());
                    }
                    return Ok(Self { file });
                }
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(credentials_lock_timeout_error());
                    }
                    thread::sleep(remaining.min(ADMIN_LOCK_POLL));
                }
                Err(fs::TryLockError::Error(err)) => {
                    return Err(admin_error(
                        "registry_credentials_lock_failed",
                        format!("failed to acquire registry credentials lock: {err}"),
                    ));
                }
            }
        }
    }
}

fn credentials_lock_timeout_error() -> KuError {
    admin_error(
        "registry_credentials_lock_timeout",
        "registry credentials lock wait exceeded its absolute deadline",
    )
}

fn ensure_lock_path_identity(path: &Path, opened: &fs::File) -> KuResult<()> {
    let current = open_nofollow(path, false, true).map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to re-open registry credentials lock: {err}"),
        )
    })?;
    let metadata = current.metadata().map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to inspect current registry credentials lock: {err}"),
        )
    })?;
    validate_regular_metadata(&metadata, "credentials lock")?;
    if file_identity(opened)? != file_identity(&current)? {
        return Err(admin_error(
            "registry_credentials_lock_replaced",
            "registry credentials lock was replaced while it was opened",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    file: u64,
}

#[cfg(unix)]
fn file_identity(file: &fs::File) -> KuResult<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to identify registry credentials lock: {err}"),
        )
    })?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &fs::File) -> KuResult<FileIdentity> {
    use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};

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
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            information: *mut FileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<FileInformation>::uninit();
    // SAFETY: `file` owns a valid handle for the call, and Windows initializes
    // the complete structure before returning a nonzero status.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(admin_error(
            "registry_credentials_lock_failed",
            format!(
                "failed to identify registry credentials lock: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: the preceding OS call returned success.
    let information = unsafe { information.assume_init() };
    Ok(FileIdentity {
        volume: u64::from(information.volume_serial_number),
        file: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(file: &fs::File) -> KuResult<FileIdentity> {
    let metadata = file.metadata().map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to identify registry credentials lock: {err}"),
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos() as u64)
        .unwrap_or_default();
    Ok(FileIdentity {
        volume: metadata.len(),
        file: modified,
    })
}

impl Drop for CredentialFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn sibling_lock_path(credentials_path: &Path) -> KuResult<PathBuf> {
    let file_name = credentials_path.file_name().ok_or_else(|| {
        admin_error(
            "invalid_registry_file",
            "registry credentials path must name a file",
        )
    })?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(credentials_path.with_file_name(lock_name))
}

struct CredentialFilePermissions {
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
    #[cfg(windows)]
    dacl: credential_dacl::Dacl,
    #[cfg(not(any(unix, windows)))]
    permissions: fs::Permissions,
}

impl CredentialFilePermissions {
    fn read_existing(path: &Path) -> KuResult<Option<Self>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(credential_permissions_error("inspect original", err)),
        };
        validate_regular_metadata(&metadata, "credentials")?;
        let file = open_nofollow(path, false, false)
            .map_err(|err| credential_permissions_error("open original", err))?;
        let metadata = file
            .metadata()
            .map_err(|err| credential_permissions_error("inspect opened original", err))?;
        validate_regular_metadata(&metadata, "credentials")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure_credential_file_has_no_extended_acl(&file, "original")?;
            Ok(Some(Self {
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode() & 0o7777,
            }))
        }
        #[cfg(windows)]
        {
            Ok(Some(Self {
                dacl: credential_dacl::Dacl::read(&file)
                    .map_err(|err| credential_permissions_error("read original DACL", err))?,
            }))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Some(Self {
                permissions: metadata.permissions(),
            }))
        }
    }

    fn apply(&self, file: &fs::File) -> KuResult<()> {
        #[cfg(unix)]
        {
            use std::os::unix::{
                fs::{MetadataExt, PermissionsExt},
                io::AsRawFd,
            };
            let metadata = file
                .metadata()
                .map_err(|err| credential_permissions_error("inspect staging ownership", err))?;
            if metadata.uid() != self.uid || metadata.gid() != self.gid {
                // SAFETY: the live File owns the descriptor; these are the
                // validated original file's uid/gid, not path-selected targets.
                if unsafe { libc::fchown(file.as_raw_fd(), self.uid, self.gid) } != 0 {
                    return Err(credential_permissions_error(
                        "preserve original uid/gid on staging",
                        io::Error::last_os_error(),
                    ));
                }
            }
            file.set_permissions(fs::Permissions::from_mode(self.mode))
                .map_err(|err| {
                    credential_permissions_error("preserve original mode on staging", err)
                })?;
            let applied = file
                .metadata()
                .map_err(|err| credential_permissions_error("verify staging permissions", err))?;
            if applied.uid() != self.uid
                || applied.gid() != self.gid
                || applied.mode() & 0o7777 != self.mode
            {
                return Err(credential_permissions_error(
                    "verify staging permissions",
                    io::Error::other("uid/gid/mode did not match"),
                ));
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.dacl.apply(file).map_err(|err| {
                credential_permissions_error("preserve original DACL on staging", err)
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            file.set_permissions(self.permissions.clone())
                .map_err(|err| credential_permissions_error("preserve original permissions", err))
        }
    }
}

fn credential_permissions_error(action: &str, error: io::Error) -> KuError {
    admin_error(
        "registry_credentials_permissions_failed",
        format!("failed to {action} before registry credentials replacement: {error}"),
    )
}

#[cfg(unix)]
fn credential_acl_unsupported(role: &str, reason: &str) -> KuError {
    admin_error(
        "registry_credentials_acl_unsupported",
        format!(
            "{role} registry credentials file {reason}; replacement stopped before writing credentials; use a trusted credential directory with reliably inspectable ordinary Unix permissions"
        ),
    )
}

#[cfg(target_os = "linux")]
fn ensure_credential_file_has_no_extended_acl(file: &fs::File, role: &str) -> KuResult<()> {
    use std::os::unix::io::AsRawFd;

    // A mode-only POSIX ACL has one 4-byte header and exactly three 8-byte
    // entries. One fixed-size read rejects larger ACLs without allocating,
    // retrying or racing a separate size query against the attribute read.
    let mut value = [0u8; 28];
    // SAFETY: the File owns a live fd, the attribute name is NUL terminated,
    // and value provides the full writable capacity passed to fgetxattr.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if size < 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENODATA) => return Ok(()),
            // Unsupported inspection does not prove absence of access rules.
            Some(libc::ENOTSUP) => {
                return Err(credential_acl_unsupported(
                    role,
                    "does not support reliable access ACL inspection",
                ));
            }
            // An ACL too large for three base entries is necessarily extended.
            Some(libc::ERANGE) => {
                return Err(credential_acl_unsupported(
                    role,
                    "has an extended access ACL that cannot be preserved safely",
                ));
            }
            _ => {
                return Err(credential_permissions_error(
                    &format!("inspect {role} access ACL"),
                    error,
                ));
            }
        }
    }
    if size as usize == value.len() && linux_access_acl_is_mode_only(&value) {
        Ok(())
    } else {
        // Unknown/malformed encodings must not silently become mode bits.
        Err(credential_acl_unsupported(
            role,
            "has an extended or unrecognized access ACL that cannot be preserved safely",
        ))
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_access_acl_is_mode_only(value: &[u8]) -> bool {
    // Linux UAPI: posix_acl_xattr.h version 2, little-endian fields;
    // posix_acl.h USER_OBJ=1, GROUP_OBJ=4, OTHER=32, undefined id=-1.
    if value.len() != 28 || value[..4] != 2u32.to_le_bytes() {
        return false;
    }
    value[4..]
        .chunks_exact(8)
        .zip([1u16, 4, 32])
        .all(|(entry, expected_tag)| {
            u16::from_le_bytes([entry[0], entry[1]]) == expected_tag
                && u16::from_le_bytes([entry[2], entry[3]]) <= 7
                && entry[4..] == u32::MAX.to_le_bytes()
        })
}

#[cfg(target_os = "macos")]
fn ensure_credential_file_has_no_extended_acl(file: &fs::File, role: &str) -> KuResult<()> {
    credential_macos_acl::ensure_mode_only(file, role)
}

#[cfg(any(target_os = "macos", test))]
fn macos_extended_acl_header_is_mode_only(header: &[u32; 11], size: isize) -> bool {
    // Darwin acl_copy_ext's portable header has magic, two 16-byte UUIDs,
    // entry count and flags. Empty ACLs with flags (including deferred
    // inheritance on rename) must not be treated as ordinary mode bits.
    size == 44 && u32::from_be(header[0]) == 0x012c_c16d && header[9] == 0 && header[10] == 0
}

#[cfg(target_os = "macos")]
mod credential_macos_acl {
    use super::{
        credential_acl_unsupported, credential_permissions_error,
        macos_extended_acl_header_is_mode_only,
    };
    use crate::error::KuResult;
    use std::{ffi::c_void, fs, io, os::unix::io::AsRawFd};

    // Darwin libc exports these APIs in libSystem, but libc's Rust bindings
    // do not expose them. Values and signatures are from Apple's sys/acl.h.
    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;

    #[link(name = "System")]
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
        fn acl_copy_ext(
            buffer: *mut c_void,
            acl: *mut c_void,
            size: libc::ssize_t,
        ) -> libc::ssize_t;
        fn acl_free(acl: *mut c_void) -> libc::c_int;
        #[cfg(test)]
        fn acl_from_text(text: *const libc::c_char) -> *mut c_void;
        #[cfg(test)]
        fn acl_set_fd(fd: libc::c_int, acl: *mut c_void) -> libc::c_int;
    }

    struct Acl(*mut c_void);

    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this is the unique owner of an ACL returned by libc.
            unsafe { acl_free(self.0) };
        }
    }

    pub(super) fn ensure_mode_only(file: &fs::File, role: &str) -> KuResult<()> {
        // SAFETY: the descriptor is owned by the live File. Use EXTENDED, not
        // the unsupported POSIX ACCESS type mentioned by older Darwin manpages.
        let raw = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            // Darwin filesec_get_property(FILESEC_ACL) reports ENOENT when
            // fstatx_np found no ACL on this already-open descriptor.
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            if error.raw_os_error() == Some(libc::ENOTSUP)
                || error.raw_os_error() == Some(libc::EOPNOTSUPP)
            {
                return Err(credential_acl_unsupported(
                    role,
                    "does not support reliable extended ACL inspection",
                ));
            }
            return Err(credential_permissions_error(
                &format!("inspect {role} extended ACL"),
                error,
            ));
        }
        let acl = Acl(raw);
        // acl_copy_ext validates the ACL and rejects anything larger than the
        // empty 44-byte header. u32 storage supplies the C header's alignment;
        // no opaque libc structs are dereferenced and no ACE loop is needed.
        let mut header = [0u32; 11];
        // SAFETY: acl is alive, and header is a 44-byte, aligned writable buffer.
        let size = unsafe { acl_copy_ext(header.as_mut_ptr().cast(), acl.0, 44) };
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ERANGE) {
                return Err(credential_permissions_error(
                    &format!("inspect {role} extended ACL header"),
                    error,
                ));
            }
        }
        if macos_extended_acl_header_is_mode_only(&header, size) {
            Ok(())
        } else {
            Err(credential_acl_unsupported(
                role,
                "has extended ACL entries or flags that cannot be preserved safely",
            ))
        }
    }

    #[cfg(test)]
    pub(super) fn install_test_acl(file: &fs::File, text: &std::ffi::CStr) -> io::Result<()> {
        // The fixtures use a fixed UUID, so ACL creation does not require
        // shell commands, directory-service lookups or account privileges.
        let raw = unsafe { acl_from_text(text.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let acl = Acl(raw);
        if unsafe { acl_set_fd(file.as_raw_fd(), acl.0) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn ensure_credential_file_has_no_extended_acl(_file: &fs::File, _role: &str) -> KuResult<()> {
    Err(admin_error(
        "registry_credentials_acl_unsupported",
        "this Unix platform cannot safely inspect credential ACLs; replacement stopped before writing credentials",
    ))
}

#[cfg(windows)]
mod credential_dacl {
    use std::{ffi::c_void, fs, io, mem, os::windows::io::AsRawHandle, ptr};

    const SE_FILE_OBJECT: u32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 4;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
    const UNPROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x2000_0000;
    const SE_DACL_PROTECTED: u16 = 0x1000;

    #[repr(C)]
    struct AclSizeInformation {
        ace_count: u32,
        bytes_in_use: u32,
        bytes_free: u32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn SetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
        fn GetSecurityDescriptorControl(
            descriptor: *const c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            descriptor: *const c_void,
            present: *mut i32,
            dacl: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn GetAclInformation(
            acl: *const c_void,
            information: *mut c_void,
            length: u32,
            class: u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    pub(super) struct Dacl {
        descriptor: *mut c_void,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Snapshot {
        protected: bool,
        bytes: Option<Vec<u8>>,
    }

    impl Dacl {
        #[cfg(test)]
        pub(super) fn from_sddl(sddl: &str) -> io::Result<Self> {
            #[link(name = "advapi32")]
            unsafe extern "system" {
                fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text: *const u16,
                    revision: u32,
                    descriptor: *mut *mut c_void,
                    descriptor_size: *mut u32,
                ) -> i32;
            }
            let text = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let mut descriptor = ptr::null_mut();
            // SAFETY: text is terminated UTF-16 and the API returns a
            // LocalAlloc-owned descriptor, released by the same Drop path.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if descriptor.is_null() {
                return Err(io::Error::other("Windows returned a null test descriptor"));
            }
            Ok(Self { descriptor })
        }

        pub(super) fn read(file: &fs::File) -> io::Result<Self> {
            let mut descriptor = ptr::null_mut();
            // SAFETY: the File owns a live handle. GetSecurityInfo allocates
            // the returned descriptor with LocalAlloc; Drop releases it.
            let error = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut descriptor,
                )
            };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            if descriptor.is_null() {
                return Err(io::Error::other(
                    "Windows returned a null security descriptor",
                ));
            }
            Ok(Self { descriptor })
        }

        fn parts(&self) -> io::Result<(bool, *mut c_void)> {
            let mut control = 0u16;
            let mut revision = 0u32;
            let mut present = 0i32;
            let mut defaulted = 0i32;
            let mut dacl = ptr::null_mut();
            // SAFETY: self owns the valid Windows-allocated descriptor and all
            // output pointers refer to initialized, correctly typed locals.
            if unsafe { GetSecurityDescriptorControl(self.descriptor, &mut control, &mut revision) }
                == 0
                || unsafe {
                    GetSecurityDescriptorDacl(
                        self.descriptor,
                        &mut present,
                        &mut dacl,
                        &mut defaulted,
                    )
                } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok((
                control & SE_DACL_PROTECTED != 0,
                if present == 0 { ptr::null_mut() } else { dacl },
            ))
        }

        pub(super) fn snapshot(&self) -> io::Result<Snapshot> {
            let (protected, dacl) = self.parts()?;
            let bytes = if dacl.is_null() {
                None
            } else {
                let mut size = AclSizeInformation {
                    ace_count: 0,
                    bytes_in_use: 0,
                    bytes_free: 0,
                };
                // SAFETY: dacl points into self's live descriptor. Windows
                // fills the fixed-size AclSizeInformation structure.
                if unsafe {
                    GetAclInformation(
                        dacl,
                        (&mut size as *mut AclSizeInformation).cast(),
                        mem::size_of::<AclSizeInformation>() as u32,
                        2,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if !(8..=65_535).contains(&size.bytes_in_use) {
                    return Err(io::Error::other(
                        "Windows returned an invalid DACL byte length",
                    ));
                }
                // SAFETY: bytes_in_use is the OS-verified initialized ACL
                // range in the owned descriptor, excluding unused padding.
                Some(
                    unsafe {
                        std::slice::from_raw_parts(dacl.cast::<u8>(), size.bytes_in_use as usize)
                    }
                    .to_vec(),
                )
            };
            Ok(Snapshot { protected, bytes })
        }

        pub(super) fn apply(&self, file: &fs::File) -> io::Result<()> {
            let expected = self.snapshot()?;
            let (protected, dacl) = self.parts()?;
            let information = DACL_SECURITY_INFORMATION
                | if protected {
                    PROTECTED_DACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_DACL_SECURITY_INFORMATION
                };
            // SAFETY: the staging handle has WRITE_DAC. The borrowed ACL stays
            // live for the call. No ignore-ACL-error behavior is enabled.
            let error = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    information,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut(),
                )
            };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            if Self::read(file)?.snapshot()? != expected {
                return Err(io::Error::other(
                    "staging DACL did not exactly match the original",
                ));
            }
            Ok(())
        }
    }

    impl Drop for Dacl {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.descriptor);
            }
        }
    }
}

struct CredentialParentSync {
    #[cfg(not(windows))]
    directory: fs::File,
}

impl CredentialParentSync {
    fn open(parent: &Path) -> KuResult<Self> {
        #[cfg(not(windows))]
        {
            Ok(Self {
                directory: fs::File::open(parent).map_err(|err| {
                    admin_error("registry_credentials_write_failed", format!(
                        "failed to open registry credentials directory for synchronization before replacement: {err}"
                    ))
                })?,
            })
        }
        #[cfg(windows)]
        {
            let _ = parent;
            Ok(Self {})
        }
    }

    fn sync(&self) -> io::Result<()> {
        #[cfg(not(windows))]
        {
            self.directory.sync_all()
        }
        #[cfg(windows)]
        {
            Ok(())
        }
    }
}

fn write_credentials_atomically(path: &Path, bytes: &[u8]) -> KuResult<()> {
    write_credentials_atomically_with_hook(path, bytes, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtomicWriteCheckpoint {
    AfterPermissions,
    BeforeReplace,
    AfterReplace,
}

fn write_credentials_atomically_with_hook(
    path: &Path,
    bytes: &[u8],
    checkpoint: impl FnMut(AtomicWriteCheckpoint) -> io::Result<()>,
) -> KuResult<()> {
    write_credentials_atomically_with_hooks(path, bytes, checkpoint, CredentialParentSync::sync)
}

fn write_credentials_atomically_with_hooks(
    path: &Path,
    bytes: &[u8],
    mut checkpoint: impl FnMut(AtomicWriteCheckpoint) -> io::Result<()>,
    sync_parent: impl FnOnce(&CredentialParentSync) -> io::Result<()>,
) -> KuResult<()> {
    validate_replacement(bytes)?;
    reject_existing_link(path, "credentials")?;
    let parent = path.parent().ok_or_else(|| {
        admin_error(
            "invalid_registry_file",
            "registry credentials path must have a parent directory",
        )
    })?;
    // Predictable directory-open/permission failures must happen before the
    // replacement, while later fsync failures still report a committed state.
    let parent_sync = CredentialParentSync::open(parent)?;
    let permissions = CredentialFilePermissions::read_existing(path)?;
    let temporary_path = staging_path(path)?;
    remove_stale_staging_file(&temporary_path)?;
    let mut temporary = TemporaryCredentialFile::new(temporary_path.clone());
    let mut file = open_new_private_file(&temporary_path).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to create registry credentials staging file: {err}"),
        )
    })?;
    // A parent's default/inheritable ACL can add entries even when creation
    // requested mode 0600. Inspect the actual empty staging fd, including the
    // first issue when no original permissions exist yet.
    #[cfg(unix)]
    ensure_credential_file_has_no_extended_acl(&file, "staging")?;
    if let Some(permissions) = permissions {
        permissions.apply(&file)?;
    }
    checkpoint(AtomicWriteCheckpoint::AfterPermissions).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!(
                "registry credentials write stopped before staging contents were written: {err}"
            ),
        )
    })?;
    file.write_all(bytes).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to write registry credentials staging file: {err}"),
        )
    })?;
    file.sync_all().map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to sync registry credentials staging file: {err}"),
        )
    })?;
    drop(file);
    checkpoint(AtomicWriteCheckpoint::BeforeReplace).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("registry credentials write stopped before atomic replacement: {err}"),
        )
    })?;
    reject_existing_link(path, "credentials")?;
    replace_file_atomically(&temporary_path, path).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to atomically replace registry credentials file: {err}"),
        )
    })?;
    temporary.keep();
    checkpoint(AtomicWriteCheckpoint::AfterReplace)
        .map_err(|err| committed_credentials_error("post-replacement checkpoint", err))?;
    sync_parent(&parent_sync)
        .map_err(|err| committed_credentials_error("directory synchronization", err))
}

fn committed_credentials_error(phase: &str, error: io::Error) -> KuError {
    admin_error(
        "registry_credentials_commit_uncertain",
        format!(
            "registry credentials replacement was committed, but durability is uncertain because {phase} failed ({:?}); do not assume that the change was rolled back",
            error.kind()
        ),
    )
}

fn staging_path(path: &Path) -> KuResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        admin_error(
            "invalid_registry_file",
            "registry credentials path must name a file",
        )
    })?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(".tmp");
    Ok(path.with_file_name(temporary_name))
}

fn remove_stale_staging_file(path: &Path) -> KuResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(admin_error(
                "registry_credentials_write_failed",
                format!("failed to inspect registry credentials staging file: {err}"),
            ));
        }
    };
    validate_regular_metadata(&metadata, "credentials staging")?;
    fs::remove_file(path).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to remove stale registry credentials staging file: {err}"),
        )
    })
}

struct TemporaryCredentialFile {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TemporaryCredentialFile {
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

impl Drop for TemporaryCredentialFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn reject_existing_link(path: &Path, kind: &str) -> KuResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_regular_metadata(&metadata, kind),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(admin_error(
            "invalid_registry_file",
            format!("failed to inspect registry {kind} file: {err}"),
        )),
    }
}

fn validate_regular_metadata(metadata: &fs::Metadata, kind: &str) -> KuResult<()> {
    if !metadata.is_file() || metadata_is_reparse_point(metadata) {
        return Err(admin_error(
            "invalid_registry_file",
            format!("registry {kind} must be a real regular file"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn open_nofollow(path: &Path, create: bool, write: bool) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_nofollow(path: &Path, create: bool, write: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_nofollow(path: &Path, create: bool, write: bool) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn open_new_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_new_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_new_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
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

fn absolute_deadline(timeout: Duration) -> KuResult<Instant> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        admin_error(
            "registry_credentials_lock_failed",
            "registry credentials lock deadline overflowed",
        )
    })
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

fn admin_error(code: &str, message: impl Into<String>) -> KuError {
    KuError::package(code, message.into(), Span::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "ku-registry-admin-{label}-{}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create registry admin test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn command_surface_has_one_issue_and_revoke_form() {
        assert_eq!(
            parse_registry_command(Vec::new()).unwrap(),
            RegistryCommand::Serve
        );
        assert_eq!(
            parse_registry_command([OsString::from("--help")]).unwrap(),
            RegistryCommand::Help
        );
        assert_eq!(
            parse_registry_command(["token", "issue", "math"].map(OsString::from)).unwrap(),
            RegistryCommand::Issue {
                package_name: "math".to_string()
            }
        );
        assert_eq!(
            parse_registry_command(["token", "revoke", "math"].map(OsString::from)).unwrap(),
            RegistryCommand::Revoke {
                package_name: "math".to_string()
            }
        );
        for invalid in [
            vec!["issue", "math"],
            vec!["token", "add", "math"],
            vec!["token", "issue", "math", "secret"],
        ] {
            assert!(parse_registry_command(invalid.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn issue_and_revoke_preserve_compatible_credentials() {
        let root = TestDirectory::new("roundtrip");
        let path = root.0.join("credentials.txt");
        let old_hash: [u8; 32] = Sha256::digest(b"old-token").into();
        fs::write(
            &path,
            format!("# existing\r\nsha256-{} other\r\n", encode_hex(&old_hash)),
        )
        .unwrap();
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        assert!(token.starts_with("ku_"));
        assert_eq!(token.len(), 3 + TOKEN_RANDOM_BYTES * 2);
        let issued = fs::read(&path).unwrap();
        assert!(issued.starts_with(b"# existing\r\n"));
        let records = parse_credential_records(&issued).unwrap();
        assert_eq!(records.len(), 2);
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
        let revoked = fs::read(&path).unwrap();
        assert_eq!(parse_credential_records(&revoked).unwrap().len(), 1);
        assert!(String::from_utf8(revoked).unwrap().contains("# existing"));
    }

    #[test]
    fn revoking_the_last_credential_fails_without_rewriting_the_file() {
        let root = TestDirectory::new("last-credential");
        let path = root.0.join("credentials.txt");
        let token = "only-token";
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let original = format!("# keep\nsha256-{} math\n", encode_hex(&hash));
        fs::write(&path, original.as_bytes()).unwrap();
        let error = revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1))
            .expect_err("the last service credential must remain valid");
        assert_eq!(error.code.as_deref(), Some("registry_last_credential"));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[test]
    fn atomic_crash_boundaries_leave_a_readable_old_or_new_file() {
        let root = TestDirectory::new("crash-boundaries");
        let path = root.0.join("credentials.txt");
        let old_hash: [u8; 32] = Sha256::digest(b"old").into();
        let new_hash: [u8; 32] = Sha256::digest(b"new").into();
        let old = format!("sha256-{} math\n", encode_hex(&old_hash));
        let new = format!("sha256-{} math\n", encode_hex(&new_hash));
        fs::write(&path, old.as_bytes()).unwrap();

        let before = write_credentials_atomically_with_hook(&path, new.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::BeforeReplace {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        });
        assert!(before.is_err());
        assert_eq!(fs::read(&path).unwrap(), old.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();

        let after = write_credentials_atomically_with_hook(&path, new.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::AfterReplace {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        });
        assert!(after.is_err());
        assert_eq!(fs::read(&path).unwrap(), new.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();
    }

    #[test]
    fn committed_issue_and_revoke_errors_report_exact_hash_without_plaintext() {
        let root = TestDirectory::new("commit-error-recovery");
        let path = root.0.join("credentials.txt");
        let old_token = "old-plaintext-must-not-appear-in-errors";
        let old_hash: [u8; 32] = Sha256::digest(old_token.as_bytes()).into();
        let old = format!("sha256-{} math\n", encode_hex(&old_hash));
        fs::write(&path, old.as_bytes()).unwrap();

        let error =
            issue_token_with_writer(&path, "math", Duration::from_secs(1), |path, bytes| {
                write_credentials_atomically_with_hooks(
                    path,
                    bytes,
                    |_| Ok(()),
                    |_| Err(io::Error::other("simulated directory fsync failure")),
                )
            })
            .expect_err("a post-commit fsync error must not return a successful token");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_commit_uncertain")
        );
        let committed = parse_credential_records(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            committed.len(),
            2,
            "the issue was committed despite fsync failure"
        );
        let issued_hash = committed
            .iter()
            .find(|record| record.token_hash != old_hash)
            .expect("locate newly committed hash")
            .token_hash;
        assert!(error.message.contains(&format!(
            "issued credential sha256-{}",
            encode_hex(&issued_hash)
        )));
        assert!(error.message.contains("durability is uncertain"));
        assert!(!error.message.contains(old_token));
        assert!(
            !error.message.contains("ku_"),
            "no generated plaintext may enter diagnostics"
        );
        assert!(!staging_path(&path).unwrap().exists());

        let error = revoke_token_with_writer(
            &path,
            "math",
            old_token.as_bytes(),
            Duration::from_secs(1),
            |path, bytes| {
                write_credentials_atomically_with_hook(path, bytes, |point| {
                    if point == AtomicWriteCheckpoint::AfterReplace {
                        Err(io::Error::other("simulated post-replacement failure"))
                    } else {
                        Ok(())
                    }
                })
            },
        )
        .expect_err("a post-commit revoke error must identify the committed mutation");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_commit_uncertain")
        );
        assert!(error.message.contains(&format!(
            "revoked credential sha256-{}",
            encode_hex(&old_hash)
        )));
        assert!(!error.message.contains(old_token));
        let after_revoke = fs::read(&path).unwrap();
        let remaining = parse_credential_records(&after_revoke).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].token_hash, issued_hash);
        let retry = revoke_token(&path, "math", old_token.as_bytes(), Duration::from_secs(1))
            .expect_err("retrying an already committed revocation has no matching credential");
        assert_eq!(retry.code.as_deref(), Some("registry_credential_not_found"));
        assert!(retry.message.contains("already have been revoked"));
        assert_eq!(fs::read(&path).unwrap(), after_revoke);
    }

    #[test]
    fn token_and_revocation_output_failures_are_committed_and_redacted() {
        struct FailingOutput<'a> {
            fail_write: bool,
            private_error: &'a str,
            written: Vec<u8>,
        }
        impl Write for FailingOutput<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.fail_write {
                    Err(io::Error::other(self.private_error.to_string()))
                } else {
                    self.written.extend_from_slice(bytes);
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other(self.private_error.to_string()))
            }
        }
        let token = format!("ku_{}", "a".repeat(64));
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        for fail_write in [true, false] {
            let mut output = FailingOutput {
                fail_write,
                private_error: &token,
                written: Vec::new(),
            };
            let error =
                write_issued_token(&token, &mut output).expect_err("injected output failure");
            assert_eq!(error.code.as_deref(), Some("registry_token_output_failed"));
            assert!(error
                .message
                .contains(&format!("sha256-{}", encode_hex(&hash))));
            assert!(error.message.contains("committed"));
            assert!(
                !error.to_string().contains(&token),
                "even a writer's private error must be redacted"
            );
            if fail_write {
                assert!(output.written.is_empty());
            } else {
                assert_eq!(output.written, format!("{token}\n").as_bytes());
            }
            let error = write_revocation_confirmation("math", &mut output)
                .expect_err("revoke confirmation failure must be structured, not a panic");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_revocation_output_failed")
            );
            assert!(error.message.contains("committed"));
            assert!(!error.to_string().contains(&token));
        }
        let mut success = Vec::new();
        write_issued_token(&token, &mut success).unwrap();
        assert_eq!(success, format!("{token}\n").as_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn windows_existing_dacl_is_preserved_before_writing_and_after_issue_revoke() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = TestDirectory::new("windows-preserve-dacl");
        let path = root.0.join("credentials.txt");
        let hash: [u8; 32] = Sha256::digest(b"original").into();
        let original = format!("sha256-{} original\n", encode_hex(&hash));
        fs::write(&path, original.as_bytes()).unwrap();
        let private = credential_dacl::Dacl::from_sddl("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)")
            .expect("create private DACL without symlink privilege");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(0x1000_0000)
            .open(&path)
            .expect("open original with security access");
        private.apply(&file).expect("install private original DACL");
        let expected = credential_dacl::Dacl::read(&file)
            .unwrap()
            .snapshot()
            .unwrap();
        drop(file);
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::AfterPermissions {
                let staging = open_nofollow(&staging_path(&path).unwrap(), false, false).unwrap();
                assert_eq!(
                    staging.metadata().unwrap().len(),
                    0,
                    "copy DACL before writing credential hashes"
                );
                assert_eq!(
                    credential_dacl::Dacl::read(&staging)
                        .unwrap()
                        .snapshot()
                        .unwrap(),
                    expected
                );
                Err(io::Error::other("stop after permission verification"))
            } else {
                Ok(())
            }
        })
        .expect_err("injected pre-write stop");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_write_failed")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        let current = open_nofollow(&path, false, false).unwrap();
        assert_eq!(
            credential_dacl::Dacl::read(&current)
                .unwrap()
                .snapshot()
                .unwrap(),
            expected
        );
        drop(current);
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
        let current = open_nofollow(&path, false, false).unwrap();
        assert_eq!(
            credential_dacl::Dacl::read(&current)
                .unwrap()
                .snapshot()
                .unwrap(),
            expected
        );
    }

    fn linux_acl_bytes(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in entries {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&permissions.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn linux_mode_only_acl_parser_rejects_extended_and_unknown_encodings() {
        for owner in 0..8 {
            for group in 0..8 {
                for other in 0..8 {
                    assert!(linux_access_acl_is_mode_only(&linux_acl_bytes(&[
                        (1, owner, u32::MAX),
                        (4, group, u32::MAX),
                        (32, other, u32::MAX),
                    ])));
                }
            }
        }
        let valid = linux_acl_bytes(&[(1, 6, u32::MAX), (4, 4, u32::MAX), (32, 0, u32::MAX)]);
        for length in 0..valid.len() {
            assert!(!linux_access_acl_is_mode_only(&valid[..length]));
        }
        let mut wrong_version = valid.clone();
        wrong_version[0] = 3;
        assert!(!linux_access_acl_is_mode_only(&wrong_version));
        for tag in [0u16, 2, 8, 16, 33] {
            let mut named_or_unknown = valid.clone();
            named_or_unknown[12..14].copy_from_slice(&tag.to_le_bytes());
            assert!(!linux_access_acl_is_mode_only(&named_or_unknown));
        }
        let mut bad_permissions = valid.clone();
        bad_permissions[6..8].copy_from_slice(&8u16.to_le_bytes());
        assert!(!linux_access_acl_is_mode_only(&bad_permissions));
        let mut named_id = valid.clone();
        named_id[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(!linux_access_acl_is_mode_only(&named_id));
        let mut trailing_entry = valid.clone();
        trailing_entry.extend_from_slice(&[0; 8]);
        assert!(!linux_access_acl_is_mode_only(&trailing_entry));
        let out_of_order =
            linux_acl_bytes(&[(4, 4, u32::MAX), (1, 6, u32::MAX), (32, 0, u32::MAX)]);
        assert!(!linux_access_acl_is_mode_only(&out_of_order));
    }

    #[test]
    fn macos_empty_acl_header_rejects_entries_flags_and_unknown_encodings() {
        let mut header = [0u32; 11];
        header[0] = 0x012c_c16du32.to_be();
        assert!(macos_extended_acl_header_is_mode_only(&header, 44));
        for size in [-1, 0, 43, 45, 68] {
            assert!(!macos_extended_acl_header_is_mode_only(&header, size));
        }
        for flags in [1u32, 0xffff, 1 << 16, 1 << 17, 1 << 31, u32::MAX] {
            header[10] = flags.to_be();
            assert!(!macos_extended_acl_header_is_mode_only(&header, 44));
        }
        header[10] = 0;
        header[9] = 1u32.to_be();
        assert!(!macos_extended_acl_header_is_mode_only(&header, 44));
        header[9] = 0;
        header[0] = 0;
        assert!(!macos_extended_acl_header_is_mode_only(&header, 44));
    }

    #[cfg(target_os = "linux")]
    fn install_linux_test_acl(file: &fs::File, default_acl: bool) -> Option<Vec<u8>> {
        use std::os::unix::io::AsRawFd;
        let named_uid = if unsafe { libc::geteuid() } == 1 {
            2
        } else {
            1
        };
        let value = linux_acl_bytes(&[
            (1, 7, u32::MAX),
            (2, 4, named_uid),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ]);
        let name = if default_acl {
            c"system.posix_acl_default"
        } else {
            c"system.posix_acl_access"
        };
        // SAFETY: the test owns this file/dir fd and the serialized ACL buffer.
        if unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTSUP) {
                eprintln!(
                    "skip extended ACL fixture: temporary filesystem has no POSIX ACL support"
                );
                return None;
            }
            panic!("install real Linux ACL fixture: {error}");
        }
        Some(value)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_source_extended_acl_is_rejected_without_replacing_credentials() {
        use std::os::unix::io::AsRawFd;
        let root = TestDirectory::new("linux-source-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let file = fs::File::open(&path).unwrap();
        let Some(expected_acl) = install_linux_test_acl(&file, false) else {
            return;
        };
        let mut reached_write = false;
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |_| {
            reached_write = true;
            Ok(())
        })
        .expect_err("source extended ACL must not be silently discarded");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert!(error.message.contains("original"));
        assert!(!reached_write, "ACL rejection must precede staging writes");
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
        let mut actual_acl = [0u8; 44];
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"system.posix_acl_access".as_ptr(),
                actual_acl.as_mut_ptr().cast(),
                actual_acl.len(),
            )
        };
        assert_eq!(size, actual_acl.len() as isize);
        assert_eq!(actual_acl.as_slice(), expected_acl.as_slice());
        let error = issue_token(&path, "math", Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_inherited_staging_acl_is_rejected_for_existing_and_first_credentials() {
        let root = TestDirectory::new("linux-default-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        // Create the original before installing the parent's default ACL, so
        // this test reaches the separate staging-fd check.
        fs::write(&path, original.as_bytes()).unwrap();
        let parent = fs::File::open(&root.0).unwrap();
        if install_linux_test_acl(&parent, true).is_none() {
            return;
        }
        let original_file = fs::File::open(&path).unwrap();
        ensure_credential_file_has_no_extended_acl(&original_file, "original").unwrap();
        let probe_path = root.0.join("empty-probe");
        let probe = open_new_private_file(&probe_path).unwrap();
        assert_eq!(probe.metadata().unwrap().len(), 0);
        assert_eq!(
            ensure_credential_file_has_no_extended_acl(&probe, "staging")
                .unwrap_err()
                .code
                .as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        drop(probe);
        fs::remove_file(&probe_path).unwrap();
        for destination in [&path, &root.0.join("first-credentials.txt")] {
            let mut reached_write = false;
            let error =
                write_credentials_atomically_with_hook(destination, original.as_bytes(), |_| {
                    reached_write = true;
                    Ok(())
                })
                .expect_err("inherited ACL must be rejected before any bytes are written");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_credentials_acl_unsupported")
            );
            assert!(error.message.contains("staging"));
            assert!(!reached_write);
            assert!(!staging_path(destination).unwrap().exists());
        }
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!root.0.join("first-credentials.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_source_extended_acl_is_rejected_without_replacing_credentials() {
        let root = TestDirectory::new("macos-source-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let file = fs::File::open(&path).unwrap();
        credential_macos_acl::install_test_acl(
            &file,
            c"!#acl 1\nuser:00000000-0000-0000-0000-000000000001:::allow:read\n",
        )
        .expect("install real extended macOS ACL without account lookup");
        let mut reached_write = false;
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |_| {
            reached_write = true;
            Ok(())
        })
        .expect_err("source extended ACL must not be silently discarded");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert!(error.message.contains("original"));
        assert!(!reached_write);
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
        let error = issue_token(&path, "math", Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_inherited_staging_acl_is_rejected_for_existing_and_first_credentials() {
        let root = TestDirectory::new("macos-inherit-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let parent = fs::File::open(&root.0).unwrap();
        credential_macos_acl::install_test_acl(
            &parent,
            c"!#acl 1\nuser:00000000-0000-0000-0000-000000000001:::allow,file_inherit,only_inherit:read\n",
        ).expect("install inheritable macOS ACL");
        let original_file = fs::File::open(&path).unwrap();
        ensure_credential_file_has_no_extended_acl(&original_file, "original").unwrap();
        let probe_path = root.0.join("empty-probe");
        let probe = open_new_private_file(&probe_path).unwrap();
        assert_eq!(probe.metadata().unwrap().len(), 0);
        assert_eq!(
            ensure_credential_file_has_no_extended_acl(&probe, "staging")
                .unwrap_err()
                .code
                .as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        drop(probe);
        fs::remove_file(&probe_path).unwrap();
        for destination in [&path, &root.0.join("first-credentials.txt")] {
            let mut reached_write = false;
            let error =
                write_credentials_atomically_with_hook(destination, original.as_bytes(), |_| {
                    reached_write = true;
                    Ok(())
                })
                .expect_err("inherited ACL must be rejected before any bytes are written");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_credentials_acl_unsupported")
            );
            assert!(error.message.contains("staging"));
            assert!(!reached_write);
            assert!(!staging_path(destination).unwrap().exists());
        }
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!root.0.join("first-credentials.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_explicit_empty_acl_allows_issue_and_revoke() {
        let root = TestDirectory::new("macos-empty-acl");
        let path = root.0.join("credentials.txt");
        fs::write(
            &path,
            format!(
                "sha256-{} original\n",
                encode_hex(&Sha256::digest(b"original"))
            ),
        )
        .unwrap();
        let file = fs::File::open(&path).unwrap();
        credential_macos_acl::install_test_acl(&file, c"!#acl 1\n")
            .expect("install explicitly empty macOS ACL");
        ensure_credential_file_has_no_extended_acl(&file, "original").unwrap();
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_empty_acl_with_inheritance_flags_is_rejected_before_write() {
        let root = TestDirectory::new("macos-empty-acl-flags");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let file = fs::File::open(&path).unwrap();
        credential_macos_acl::install_test_acl(&file, c"!#acl 1 no_inherit\n")
            .expect("install empty macOS ACL with a nontrivial inheritance flag");
        let mut reached_write = false;
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |_| {
            reached_write = true;
            Ok(())
        })
        .expect_err("an empty ACL with flags is not mode-only");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert!(!reached_write);
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_existing_uid_gid_and_mode_are_preserved() {
        use std::os::unix::{
            fs::{MetadataExt, PermissionsExt},
            io::AsRawFd,
        };
        let root = TestDirectory::new("unix-preserve-owner-mode");
        let path = root.0.join("credentials.txt");
        let hash: [u8; 32] = Sha256::digest(b"original").into();
        let original = format!("sha256-{} original\n", encode_hex(&hash));
        fs::write(&path, original.as_bytes()).unwrap();
        // Root-capable runs exercise a genuinely different owner and group;
        // ordinary CI users verify their existing uid/gid without privilege.
        if unsafe { libc::geteuid() } == 0 {
            let file = fs::File::open(&path).unwrap();
            assert_eq!(unsafe { libc::fchown(file.as_raw_fd(), 65_534, 65_534) }, 0);
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let expected = (before.uid(), before.gid(), before.mode() & 0o7777);
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::AfterPermissions {
                let staging = fs::metadata(staging_path(&path).unwrap()).unwrap();
                assert_eq!(staging.len(), 0);
                assert_eq!(
                    (staging.uid(), staging.gid(), staging.mode() & 0o7777),
                    expected
                );
                Err(io::Error::other("stop before credential bytes are written"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_write_failed")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        let issued = fs::metadata(&path).unwrap();
        assert_eq!(
            (issued.uid(), issued.gid(), issued.mode() & 0o7777),
            expected
        );
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
        let revoked = fs::metadata(&path).unwrap();
        assert_eq!(
            (revoked.uid(), revoked.gid(), revoked.mode() & 0o7777),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_permission_copy_and_parent_open_failures_stop_before_commit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skip unprivileged ownership/directory access failures under root");
            return;
        }
        let root = TestDirectory::new("unix-precommit-access-failure");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let staging = open_new_private_file(&staging_path(&path).unwrap()).unwrap();
        let metadata = staging.metadata().unwrap();
        let foreign = CredentialFilePermissions {
            uid: if metadata.uid() == 1 { 2 } else { 1 },
            gid: metadata.gid(),
            mode: 0o600,
        };
        let error = foreign
            .apply(&staging)
            .expect_err("unprivileged owner change must fail");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_permissions_failed")
        );
        assert_eq!(staging.metadata().unwrap().len(), 0);
        drop(staging);
        fs::remove_file(staging_path(&path).unwrap()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());

        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o300)).unwrap();
        let result = issue_token(&path, "math", Duration::from_secs(1));
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.expect_err("directory read-open denial must precede replacement");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_write_failed")
        );
        assert!(error.message.contains("before replacement"));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
    }

    const CRASH_FIXTURE_MODE: &str = "KU_TEST_REGISTRY_ADMIN_CRASH_STAGE";
    const CRASH_FIXTURE_PATH: &str = "KU_TEST_REGISTRY_ADMIN_CRASH_PATH";
    const CRASH_FIXTURE_EXIT_CODE: i32 = 86;

    #[test]
    fn atomic_crash_fixture_child() {
        let Ok(mode) = env::var(CRASH_FIXTURE_MODE) else {
            return;
        };
        let path =
            PathBuf::from(env::var_os(CRASH_FIXTURE_PATH).expect("crash fixture credentials path"));
        let new_hash: [u8; 32] = Sha256::digest(b"crash-new").into();
        let replacement = format!("sha256-{} math\n", encode_hex(&new_hash));
        write_credentials_atomically_with_hook(&path, replacement.as_bytes(), |point| {
            let should_exit = matches!(
                (mode.as_str(), point),
                ("before", AtomicWriteCheckpoint::BeforeReplace)
                    | ("after", AtomicWriteCheckpoint::AfterReplace)
            );
            if should_exit {
                // This boundary needs a real process stop without Rust stack
                // unwinding. A non-zero exit preserves that property while
                // avoiding OS crash-reporting/debugger latency from abort().
                std::process::exit(CRASH_FIXTURE_EXIT_CODE);
            }
            Ok(())
        })
        .expect("crash fixture reaches selected checkpoint");
    }

    #[test]
    fn real_process_crash_before_or_after_replace_keeps_a_parseable_file() {
        let root = TestDirectory::new("real-crash");
        let path = root.0.join("credentials.txt");
        let old_hash: [u8; 32] = Sha256::digest(b"crash-old").into();
        let new_hash: [u8; 32] = Sha256::digest(b"crash-new").into();
        let old = format!("sha256-{} math\n", encode_hex(&old_hash));
        let new = format!("sha256-{} math\n", encode_hex(&new_hash));
        fs::write(&path, old.as_bytes()).unwrap();

        run_crashing_fixture(&path, "before");
        assert_eq!(fs::read(&path).unwrap(), old.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();
        assert!(staging_path(&path).unwrap().is_file());

        write_credentials_atomically(&path, old.as_bytes())
            .expect("next operation cleans crash staging and preserves old file");
        assert!(!staging_path(&path).unwrap().exists());
        run_crashing_fixture(&path, "after");
        assert_eq!(fs::read(&path).unwrap(), new.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();
        assert!(!staging_path(&path).unwrap().exists());
    }

    fn run_crashing_fixture(path: &Path, mode: &str) {
        let mut child = Command::new(env::current_exe().expect("current lib test executable"))
            .args([
                "--exact",
                "registry_admin::tests::atomic_crash_fixture_child",
                "--nocapture",
            ])
            .env(CRASH_FIXTURE_MODE, mode)
            .env(CRASH_FIXTURE_PATH, path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn registry admin crash fixture");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait().expect("poll registry admin crash fixture") {
                Some(status) => break status,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                None => {
                    let _ = child.kill();
                    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        match child
                            .try_wait()
                            .expect("reap timed-out registry admin crash fixture")
                        {
                            Some(_) => break,
                            None if Instant::now() < cleanup_deadline => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            None => panic!(
                                "registry admin crash fixture could not be reaped within 2 seconds"
                            ),
                        }
                    }
                    panic!("registry admin crash fixture exceeded its 5 second deadline");
                }
            }
        };
        assert_eq!(
            status.code(),
            Some(CRASH_FIXTURE_EXIT_CODE),
            "crash fixture must reach the selected non-unwinding exit"
        );
    }

    #[test]
    fn staging_failure_does_not_rewrite_the_original_and_stale_file_is_reused() {
        let root = TestDirectory::new("staging-recovery");
        let path = root.0.join("credentials.txt");
        let original_hash: [u8; 32] = Sha256::digest(b"original").into();
        let original = format!("sha256-{} original\n", encode_hex(&original_hash));
        fs::write(&path, original.as_bytes()).unwrap();
        let staging = staging_path(&path).unwrap();
        fs::create_dir(&staging).unwrap();
        let error = issue_token(&path, "math", Duration::from_secs(1))
            .expect_err("non-file staging path must fail closed");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_file"));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());

        fs::remove_dir(&staging).unwrap();
        fs::write(&staging, b"incomplete previous staging bytes").unwrap();
        issue_token(&path, "math", Duration::from_secs(1))
            .expect("a bounded stale regular staging file can be replaced under the lock");
        assert!(!staging.exists());
        assert_eq!(
            parse_credential_records(&fs::read(&path).unwrap())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn lock_wait_has_an_absolute_deadline() {
        let root = TestDirectory::new("lock-deadline");
        let path = normalize_credentials_path(&root.0.join("credentials.txt")).unwrap();
        let held =
            CredentialFileLock::acquire(&path, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = CredentialFileLock::acquire(&path, Instant::now() + Duration::from_millis(50))
            .expect_err("second lock must time out");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_lock_timeout")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
    }

    #[test]
    fn expired_deadline_rejects_an_immediately_available_lock() {
        let root = TestDirectory::new("expired-free-lock");
        let path = normalize_credentials_path(&root.0.join("credentials.txt")).unwrap();
        let error = CredentialFileLock::acquire(&path, Instant::now())
            .expect_err("an already expired deadline must not acquire a free lock");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_lock_timeout")
        );
        CredentialFileLock::acquire(&path, Instant::now() + Duration::from_secs(1))
            .expect("expired attempt must not leave the file locked");
    }

    #[test]
    fn lock_identity_check_rejects_a_regular_file_replacement() {
        let root = TestDirectory::new("lock-replacement");
        let credentials = normalize_credentials_path(&root.0.join("credentials.txt")).unwrap();
        let lock_path = sibling_lock_path(&credentials).unwrap();
        let opened = open_nofollow(&lock_path, true, true).unwrap();
        let displaced = root.0.join("displaced.lock");
        fs::rename(&lock_path, &displaced).unwrap();
        let replacement = open_nofollow(&lock_path, true, true).unwrap();
        drop(replacement);
        let error = ensure_lock_path_identity(&lock_path, &opened)
            .expect_err("different lock file identities must be rejected");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_lock_replaced")
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_credentials_and_lock_files_are_private_and_links_are_rejected() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let root = TestDirectory::new("unix-safety");
        let path = root.0.join("credentials.txt");
        issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(sibling_lock_path(&path).unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );

        let target = root.0.join("target.txt");
        fs::write(&target, b"untouched").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert!(issue_token(&path, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");

        let second = TestDirectory::new("unix-lock-link");
        let second_path = second.0.join("credentials.txt");
        let lock_target = second.0.join("lock-target.txt");
        fs::write(&lock_target, b"lock-target-untouched").unwrap();
        symlink(&lock_target, sibling_lock_path(&second_path).unwrap()).unwrap();
        assert!(issue_token(&second_path, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&lock_target).unwrap(), b"lock-target-untouched");
    }

    #[cfg(windows)]
    #[test]
    fn windows_credentials_and_lock_reparse_points_are_rejected() {
        use std::os::windows::fs::symlink_file;

        fn create_link_or_skip(target: &Path, link: &Path) -> bool {
            match symlink_file(target, link) {
                Ok(()) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                    ) || error.raw_os_error() == Some(1314) =>
                {
                    eprintln!(
                        "skipping Windows reparse-point assertion because file symlink creation is unavailable: {error}"
                    );
                    false
                }
                Err(error) => panic!("create Windows file symlink: {error}"),
            }
        }

        let root = TestDirectory::new("windows-file-reparse");
        let target = root.0.join("target.txt");
        let credentials = root.0.join("credentials.txt");
        fs::write(&target, b"untouched").unwrap();
        if !create_link_or_skip(&target, &credentials) {
            return;
        }
        assert!(issue_token(&credentials, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");

        let second = TestDirectory::new("windows-lock-reparse");
        let second_credentials = second.0.join("credentials.txt");
        let lock_target = second.0.join("lock-target.txt");
        fs::write(&lock_target, b"lock-target-untouched").unwrap();
        if !create_link_or_skip(
            &lock_target,
            &sibling_lock_path(&second_credentials).unwrap(),
        ) {
            return;
        }
        assert!(issue_token(&second_credentials, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&lock_target).unwrap(), b"lock-target-untouched");
    }

    #[test]
    fn parser_enforces_utf8_line_entry_and_file_bounds() {
        assert!(parse_credential_records(&[0xff]).is_err());
        assert!(
            parse_credential_records(&vec![b'x'; MAX_CREDENTIAL_FILE_BYTES as usize + 1]).is_err()
        );
        let long = format!("#{}\n", "x".repeat(MAX_REGISTRY_LINE_BYTES));
        assert!(parse_credential_records(long.as_bytes()).is_err());
        let duplicate_hash: [u8; 32] = Sha256::digest(b"same").into();
        let line = format!("sha256-{} math\n", encode_hex(&duplicate_hash));
        assert!(parse_credential_records(format!("{line}{line}").as_bytes()).is_err());
        assert!(parse_credential_records(b"").unwrap().is_empty());
    }
}
