//! File paths owned by Ku are staged under ASCII argument names for Windows
//! GNU tools whose narrow-char linker cannot reopen arbitrary Unicode paths.
//! The caller's working directory and unparsed KU_CC arguments stay unchanged.
use super::{
    is_plain_regular_file, link_library_identity, path_is_plain_directory,
    same_open_link_library_contents, KuError, MysqlHeaderSelection, PinnedLinkLibrary,
    TempBuildDir, VerifiedLinkOutput, MAX_NATIVE_LINK_OUTPUT_BYTES, MAX_PINNED_LINK_LIBRARY_BYTES,
};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};

const MAX_HEADER_FILES: usize = 4096;
const MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEADER_DEPTH: usize = 32;

pub(super) fn library_argument(
    workspace: Option<&mut Workspace>,
    library: &super::StagedLinkLibrary,
) -> Result<PathBuf, KuError> {
    match workspace {
        Some(workspace) => workspace.library(library.path(), library.sha256),
        None => Ok(super::strip_verbatim(library.path())),
    }
}

pub(super) struct Workspace {
    directory: TempBuildDir,
    temporary: TempBuildDir,
    cwd: PathBuf,
    argument_root: PathBuf,
    next_input: usize,
}

impl Workspace {
    pub(super) fn create() -> Result<Self, KuError> {
        Self::create_at(&env::current_dir().map_err(error)?, &env::temp_dir()).map_err(error)
    }

    pub(super) fn create_at(cwd: &Path, temporary: &Path) -> io::Result<Self> {
        // GCC canonicalizes its own temporary filenames before invoking as.
        // Relative TMP under a Unicode cwd therefore becomes an unrepresentable
        // absolute name again. Keep internal tool temporaries in a separately
        // owned, absolute ASCII directory even when inputs use relative names.
        let temporary_root = ascii_temp_root(temporary)?;
        let tool_temporary = TempBuildDir::create_private_in(&temporary_root, "gnu-tmp")?;
        // Relative ASCII names work even when cwd itself contains Unicode. Rust
        // sets that original cwd with the native Unicode process API; neither
        // GCC nor ld needs to convert its absolute spelling to a narrow string.
        if let Ok(directory) = TempBuildDir::create_private_in(cwd, "gnu-tools") {
            let argument_root = PathBuf::from(
                directory
                    .path()
                    .file_name()
                    .expect("private directory name"),
            );
            return Ok(Self {
                directory,
                temporary: tool_temporary,
                cwd: cwd.to_owned(),
                argument_root,
                next_input: 0,
            });
        }
        // A read-only caller cwd can still use an ASCII temp root. A short
        // spelling is only an optional fallback, never the sole Unicode fix.
        let directory = TempBuildDir::create_private_in(&temporary_root, "gnu-tools")?;
        Ok(Self {
            argument_root: directory.path().to_owned(),
            directory,
            temporary: tool_temporary,
            cwd: cwd.to_owned(),
            next_input: 0,
        })
    }

    pub(super) fn configure(&self, command: &mut Command) {
        command.current_dir(&self.cwd);
        let temporary = super::strip_verbatim(self.temporary.path());
        command
            .env("TMP", &temporary)
            .env("TEMP", &temporary)
            .env("TMPDIR", temporary);
    }

    pub(super) fn source(&mut self, source: &Path) -> Result<PathBuf, KuError> {
        self.copy_input(source, Path::new("source.c"), None)
    }

    pub(super) fn library(
        &mut self,
        source: &Path,
        expected: [u8; 32],
    ) -> Result<PathBuf, KuError> {
        let suffix = source
            .extension()
            .and_then(|name| name.to_str())
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 12
                    && name.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .unwrap_or("a");
        let name = format!("library-{}.{}", self.next_input, suffix);
        self.next_input += 1;
        self.copy_input(source, Path::new(&name), Some(expected))
    }

    fn copy_input(
        &self,
        source: &Path,
        name: &Path,
        expected: Option<[u8; 32]>,
    ) -> Result<PathBuf, KuError> {
        let pinned = PinnedLinkLibrary::capture(source, "GNU tool input")?;
        let destination = self.directory.path().join(name);
        let checksum = copy_open_file(
            &pinned.handle,
            &pinned.identity,
            &destination,
            MAX_PINNED_LINK_LIBRARY_BYTES,
        )?;
        if expected.is_some_and(|expected| expected != checksum) {
            return Err(KuError::message(
                "verified native library changed during ASCII tool staging",
            ));
        }
        Ok(self.argument_root.join(name))
    }

    pub(super) fn include(&mut self, source: &Path) -> Result<PathBuf, KuError> {
        let name = format!("headers-{}", self.next_input);
        self.next_input += 1;
        let destination = self.directory.path().join(&name);
        copy_header_tree(source, &destination)?;
        Ok(self.argument_root.join(name))
    }

    pub(super) fn mysql_headers(
        &mut self,
        selection: &MysqlHeaderSelection,
    ) -> Result<MysqlHeaderSelection, KuError> {
        // Copy each maximal configured include root once. Mapping its nested
        // search directories preserves include order and relative includes
        // such as mysql/../mariadb without flattening separate SDK layouts.
        let mut roots: Vec<(PathBuf, PathBuf)> = Vec::new();
        for root in &selection.include_dirs {
            if selection
                .include_dirs
                .iter()
                .any(|other| root != other && root.starts_with(other))
            {
                continue;
            }
            if !roots.iter().any(|(old, _)| old == root) {
                roots.push((root.clone(), self.include(root)?));
            }
        }
        let remap = |path: &Path| -> Result<PathBuf, KuError> {
            roots
                .iter()
                .find_map(|(root, staged)| {
                    path.strip_prefix(root)
                        .ok()
                        .map(|relative| staged.join(relative))
                })
                .ok_or_else(|| {
                    KuError::message(
                        "selected MySQL header is outside its configured include roots",
                    )
                })
        };
        Ok(MysqlHeaderSelection {
            header: remap(&selection.header)?,
            include_dirs: selection
                .include_dirs
                .iter()
                .map(|path| remap(path))
                .collect::<Result<_, _>>()?,
        })
    }

    pub(super) fn output_argument(&self) -> PathBuf {
        self.argument_root.join("program.exe")
    }
    pub(super) fn output_file(&self) -> PathBuf {
        self.directory.path().join("program.exe")
    }

    pub(super) fn copy_output(
        &self,
        verified: &VerifiedLinkOutput,
        destination: &Path,
    ) -> Result<(), KuError> {
        verified.verify_current_path().map_err(KuError::message)?;
        copy_open_file(
            &verified.file,
            &verified.identity,
            destination,
            MAX_NATIVE_LINK_OUTPUT_BYTES,
        )?;
        fs::set_permissions(
            destination,
            verified.file.metadata().map_err(error)?.permissions(),
        )
        .map_err(error)?;
        verified.verify_current_path().map_err(KuError::message)
    }
}

fn error(error: impl std::fmt::Display) -> KuError {
    KuError::message(format!(
        "cannot prepare private Windows GNU tool paths: {error}"
    ))
}

fn copy_open_file(
    source: &fs::File,
    identity: &super::LinkLibraryIdentity,
    destination: &Path,
    maximum: u64,
) -> Result<[u8; 32], KuError> {
    if identity.length > maximum
        || !same_open_link_library_contents(
            &link_library_identity(source).map_err(error)?,
            identity,
        )
    {
        return Err(KuError::message(
            "GNU tool input changed or exceeds its copy bound",
        ));
    }
    let mut input = source.try_clone().map_err(error)?;
    input.seek(SeekFrom::Start(0)).map_err(error)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(error)?;
    let mut left = identity.length;
    let mut checksum = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    while left != 0 {
        let count = usize::try_from(left.min(buffer.len() as u64)).expect("bounded chunk");
        input.read_exact(&mut buffer[..count]).map_err(error)?;
        output.write_all(&buffer[..count]).map_err(error)?;
        checksum.update(&buffer[..count]);
        left -= count as u64;
    }
    let mut extra = [0];
    if input.read(&mut extra).map_err(error)? != 0
        || !same_open_link_library_contents(
            &link_library_identity(source).map_err(error)?,
            identity,
        )
    {
        return Err(KuError::message(
            "GNU tool input changed during its private copy",
        ));
    }
    output.flush().map_err(error)?;
    drop(output);
    let metadata = fs::symlink_metadata(destination).map_err(error)?;
    if !is_plain_regular_file(&metadata) || metadata.len() != identity.length {
        return Err(KuError::message(
            "GNU tool private copy is not the expected regular file",
        ));
    }
    let mut copied = fs::File::open(destination).map_err(error)?;
    let mut actual = Sha256::new();
    let mut remaining = identity.length;
    while remaining != 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64)).expect("bounded chunk");
        copied.read_exact(&mut buffer[..count]).map_err(error)?;
        actual.update(&buffer[..count]);
        remaining -= count as u64;
    }
    if copied.read(&mut extra).map_err(error)? != 0 {
        return Err(KuError::message(
            "GNU tool private copy grew during verification",
        ));
    }
    let actual: [u8; 32] = actual.finalize().into();
    let checksum: [u8; 32] = checksum.finalize().into();
    if actual != checksum {
        return Err(KuError::message(
            "GNU tool input failed its private copy checksum",
        ));
    }
    Ok(checksum)
}

fn copy_header_tree(source: &Path, destination: &Path) -> Result<(), KuError> {
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf(), 0usize)];
    let mut count = 0usize;
    let mut bytes = 0u64;
    while let Some((source, destination, depth)) = pending.pop() {
        if depth > MAX_HEADER_DEPTH || !path_is_plain_directory(&source) {
            return Err(KuError::message(
                "native header tree contains an unsupported link or excessive depth",
            ));
        }
        fs::create_dir(&destination).map_err(error)?;
        for entry in fs::read_dir(source).map_err(error)? {
            let entry = entry.map_err(error)?;
            count += 1;
            if count > MAX_HEADER_FILES {
                return Err(KuError::message(
                    "native header tree exceeds its file bound",
                ));
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(error)?;
            let path = destination.join(entry.file_name());
            if path_is_plain_directory(&entry.path()) {
                pending.push((entry.path(), path, depth + 1));
            } else if is_plain_regular_file(&metadata) {
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| KuError::message("native header byte count overflow"))?;
                if bytes > MAX_HEADER_BYTES {
                    return Err(KuError::message(
                        "native header tree exceeds its byte bound",
                    ));
                }
                let file = fs::File::open(entry.path()).map_err(error)?;
                let identity = link_library_identity(&file).map_err(error)?;
                copy_open_file(&file, &identity, &path, MAX_HEADER_BYTES)?;
            } else {
                return Err(KuError::message(
                    "native header tree contains a non-regular entry",
                ));
            }
        }
    }
    Ok(())
}

fn ascii_temp_root(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let plain = super::strip_verbatim(&path);
    if plain.to_str().is_some_and(str::is_ascii) {
        return Ok(plain);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32;
        }
        let text: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut result = vec![0u16; 32768];
        // SAFETY: both buffers are live and the capacity matches the call.
        let size =
            unsafe { GetShortPathNameW(text.as_ptr(), result.as_mut_ptr(), result.len() as u32) };
        if size > 0 && (size as usize) < result.len() {
            let short = PathBuf::from(std::ffi::OsString::from_wide(&result[..size as usize]));
            if short.to_str().is_some_and(str::is_ascii)
                && fs::canonicalize(&short)? == fs::canonicalize(&path)?
            {
                return Ok(short);
            }
        }
    }
    Err(io::Error::other("GNU assembler/linker needs an absolute ASCII temporary directory; configure TMP/TEMP to an existing writable ASCII directory (Windows short names are not assumed to exist)"))
}
