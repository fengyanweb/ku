//! Private short ASCII paths for Windows GNU compiler subprocesses.
//! Package and delivery paths keep their original native Unicode spelling.

use std::{
    fs::{self, File, Metadata},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Command,
};

use crate::error::{KuError, KuResult};

const MAX_TREE_ENTRIES: usize = 4096;
const MAX_TREE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 32;

pub(crate) fn is_target(target: &str) -> bool {
    let target = target.trim();
    target.ends_with("-windows-gnu") || target.ends_with("-mingw32") || target.ends_with("-mingw64")
}

pub(crate) struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub(crate) fn create() -> KuResult<Self> {
        let base = ascii_temp_root().map_err(error)?;
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(error)?;
        let mut label = String::from("ku-interop-");
        for byte in nonce {
            use std::fmt::Write;
            write!(label, "{byte:02x}").expect("String accepts writes");
        }
        let root = base.join(label);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root).map_err(error)?;
        Ok(Self { root })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn configure(&self, command: &mut Command) {
        command
            .current_dir(&self.root)
            .env("TMP", &self.root)
            .env("TEMP", &self.root)
            .env("TMPDIR", &self.root);
    }

    /// Copy a verified source file and return the plain relative tool argument.
    pub(crate) fn copy_file(
        &self,
        source: &Path,
        member: &Path,
        maximum: u64,
    ) -> KuResult<PathBuf> {
        let target = self.path().join(member_path(member)?);
        copy_regular(source, &target, maximum)?;
        Ok(member.to_path_buf())
    }

    /// Keep include order and relative includes by mirroring the checked tree.
    pub(crate) fn copy_tree(&self, source: &Path, member: &Path) -> KuResult<PathBuf> {
        let target = self.path().join(member_path(member)?);
        let mut pending = vec![(source.to_path_buf(), target, 0)];
        let mut entries = 0;
        let mut bytes = 0;
        while let Some((from, to, depth)) = pending.pop() {
            let metadata = plain_metadata(&from)?;
            if metadata.is_dir() {
                if depth > MAX_DEPTH {
                    return Err(error(
                        "native compiler input tree exceeds its directory depth limit",
                    ));
                }
                fs::create_dir(&to).map_err(error)?;
                for entry in fs::read_dir(&from).map_err(error)? {
                    let entry = entry.map_err(error)?;
                    let name = PathBuf::from(entry.file_name());
                    member_path(&name)?;
                    entries += 1;
                    if entries > MAX_TREE_ENTRIES {
                        return Err(error("native compiler input tree exceeds its entry limit"));
                    }
                    pending.push((entry.path(), to.join(name), depth + 1));
                }
            } else if metadata.is_file() {
                let left = MAX_TREE_BYTES - bytes;
                bytes += copy_regular(&from, &to, left)?;
            } else {
                return Err(error(
                    "native compiler input is neither a regular file nor a directory",
                ));
            }
        }
        Ok(member.to_path_buf())
    }

    pub(crate) fn copy_output(
        &self,
        member: &Path,
        destination: &Path,
        maximum: u64,
    ) -> KuResult<()> {
        let source = self.path().join(member_path(member)?);
        if plain_metadata(&source)?.len() == 0 {
            return Err(error("native compiler output was empty"));
        }
        copy_regular(&source, destination, maximum)?;
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn error(detail: impl std::fmt::Display) -> KuError {
    KuError::message(format!("cannot stage Windows GNU interop tools: {detail}"))
}

fn member_path(path: &Path) -> KuResult<&Path> {
    if path.as_os_str().is_empty()
        || !path.components().all(|part| match part {
            Component::Normal(name) => name.to_str().is_some_and(|name| {
                name.is_ascii()
                    && !name
                        .bytes()
                        .any(|byte| byte < 32 || b":\"?*|<>\\".contains(&byte))
            }),
            _ => false,
        })
    {
        return Err(error(
            "native compiler input must have a relative ASCII name",
        ));
    }
    Ok(path)
}

fn plain_metadata(path: &Path) -> KuResult<Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(error)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(error(format!(
                "native compiler input '{}' is a reparse point",
                path.display()
            )));
        }
    }
    if metadata.file_type().is_symlink() {
        return Err(error(format!(
            "native compiler input '{}' is a symlink",
            path.display()
        )));
    }
    Ok(metadata)
}

fn copy_regular(source: &Path, target: &Path, maximum: u64) -> KuResult<u64> {
    let before = plain_metadata(source)?;
    if !before.is_file() || before.len() > maximum {
        return Err(error(format!(
            "native compiler input '{}' is not a bounded regular file",
            source.display()
        )));
    }
    let input = File::open(source).map_err(error)?;
    let opened = input.metadata().map_err(error)?;
    if !opened.is_file()
        || opened.len() != before.len()
        || opened.modified().ok() != before.modified().ok()
    {
        return Err(error("native compiler input changed while it was opened"));
    }
    let mut reader = input.take(before.len() + 1);
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(error)?;
    let count = io::copy(&mut reader, &mut output).map_err(error)?;
    let after = reader.get_ref().metadata().map_err(error)?;
    if count != before.len()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err(error(
            "native compiler input changed during its private copy",
        ));
    }
    output.sync_all().map_err(error)?;
    Ok(count)
}

fn ascii_temp_root() -> io::Result<PathBuf> {
    let base = std::env::temp_dir().canonicalize()?;
    let plain = plain_absolute(&base);
    if short_ascii(&plain) {
        return Ok(plain);
    }
    #[cfg(windows)]
    if let Some(alias) = short_path_name(&base) {
        let alias = plain_absolute(&alias);
        if short_ascii(&alias) && alias.canonicalize()? == base {
            return Ok(alias);
        }
    }
    Err(io::Error::other(
        "configure TMP/TEMP to a writable, short, absolute ASCII directory",
    ))
}

fn short_ascii(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| text.len() <= 180 && text.is_ascii())
}

fn plain_absolute(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        if let Some(unc) = path.strip_prefix("UNC\\") {
            return PathBuf::from(format!("\\\\{unc}"));
        }
        return PathBuf::from(path);
    }
    path.to_path_buf()
}

#[cfg(windows)]
fn short_path_name(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32;
    }
    let input: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut result = vec![0u16; 32768];
    // SAFETY: the input and output buffers are live and their lengths match this call.
    let count =
        unsafe { GetShortPathNameW(input.as_ptr(), result.as_mut_ptr(), result.len() as u32) };
    let count = usize::try_from(count).ok()?;
    if count > 0 && count < result.len() {
        Some(PathBuf::from(std::ffi::OsString::from_wide(
            &result[..count],
        )))
    } else {
        None
    }
}
