//! Opt-in deployment of the author's explicitly redistributable CPython.

use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::error::KuResult;

use super::{fs_error, invalid, PythonToolchain};

const MAX_FILES: usize = 10_000;
const MAX_BYTES: u64 = 200 * 1024 * 1024;

pub(super) struct Runtime {
    files: Vec<(PathBuf, PathBuf)>,
    pub(super) library: PathBuf,
    pub(super) executable: PathBuf,
}

impl Runtime {
    pub(super) fn selected(toolchain: &PythonToolchain) -> KuResult<Option<Self>> {
        let Some(notices) = env::var_os("KU_PYTHON_REDISTRIBUTION_NOTICES") else {
            return Ok(None);
        };
        if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
            return Err(invalid(
                "Bundled CPython runtime currently requires a supported Unix host",
            ));
        }
        let library = toolchain
            .library
            .strip_prefix(&toolchain.home)
            .map_err(|_| {
                invalid("Redistributable CPython library must be under its selected base prefix")
            })?;
        let executable = toolchain
            .executable
            .strip_prefix(&toolchain.home)
            .map_err(|_| {
                invalid("Redistributable CPython executable must be under its selected base prefix")
            })?;
        if !library.starts_with("lib")
            || library.components().count() != 2
            || !executable.starts_with("bin")
            || executable.components().count() != 2
        {
            return Err(invalid(
                "Bundled CPython requires the relocatable bin/ and lib/ layout",
            ));
        }
        let lib = toolchain.home.join("lib");
        let mut files = collect(&lib, Path::new("lib"), true)?;
        let stdlib_license = format!("lib/python{}/LICENSE.txt", toolchain.minor);
        if !files
            .iter()
            .any(|(target, _)| target == Path::new(&stdlib_license))
        {
            return Err(invalid(
                "Bundled CPython standard-library LICENSE.txt is missing",
            ));
        }
        files.push((executable.to_path_buf(), toolchain.executable.clone()));

        let notices = PathBuf::from(notices);
        let notices = notices
            .canonicalize()
            .map_err(|error| fs_error(&notices, error))?;
        let notice_files = collect(&notices, Path::new("notices"), false)?;
        if notice_files.is_empty() {
            return Err(invalid(
                "Bundled CPython requires the distributor's original license files",
            ));
        }
        files.extend(notice_files);
        if files.len() > MAX_FILES {
            return Err(invalid("Bundled CPython contains too many files"));
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Some(Self {
            files,
            library: Path::new("python").join(library),
            executable: Path::new("python").join(executable),
        }))
    }

    pub(super) fn identity(&self) -> KuResult<String> {
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        for (target, source) in &self.files {
            let file = fs::File::open(source).map_err(|error| fs_error(source, error))?;
            digest.update(target.as_os_str().as_encoded_bytes());
            digest.update([0]);
            let copied = io::copy(&mut file.take(MAX_BYTES + 1), &mut digest)
                .map_err(|error| fs_error(source, error))?;
            total = total
                .checked_add(copied)
                .ok_or_else(|| invalid("Bundled CPython size overflow"))?;
            if total > MAX_BYTES {
                return Err(invalid("Bundled CPython exceeds the runtime byte limit"));
            }
            digest.update(copied.to_le_bytes());
        }
        Ok(format!("{:x}", digest.finalize()))
    }

    pub(super) fn stage(&self, staging: &Path) -> KuResult<()> {
        let root = staging.join("python");
        fs::create_dir(&root).map_err(|error| fs_error(&root, error))?;
        let mut total = 0_u64;
        for (name, source) in &self.files {
            let destination = root.join(name);
            let parent = destination
                .parent()
                .ok_or_else(|| invalid("Invalid CPython runtime path"))?;
            fs::create_dir_all(parent).map_err(|error| fs_error(parent, error))?;
            let remaining = MAX_BYTES.saturating_sub(total);
            let mut source_file =
                fs::File::open(source).map_err(|error| fs_error(source, error))?;
            let mut dest_file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|error| fs_error(&destination, error))?;
            let mode = source_file
                .metadata()
                .map_err(|error| fs_error(source, error))?
                .permissions();
            let copied = io::copy(
                &mut source_file.by_ref().take(remaining + 1),
                &mut dest_file,
            )
            .map_err(|error| fs_error(&destination, error))?;
            if copied > remaining {
                return Err(invalid("Bundled CPython exceeds the runtime byte limit"));
            }
            total += copied;
            fs::set_permissions(&destination, mode)
                .map_err(|error| fs_error(&destination, error))?;
        }
        Ok(())
    }
}

fn collect(root: &Path, prefix: &Path, skip_nonruntime: bool) -> KuResult<Vec<(PathBuf, PathBuf)>> {
    let root = root.canonicalize().map_err(|error| fs_error(root, error))?;
    let mut pending = vec![(root.clone(), prefix.to_path_buf(), 0)];
    let mut files = Vec::new();
    let mut total = 0_u64;
    let mut entries = 0;
    while let Some((directory, relative, depth)) = pending.pop() {
        if depth > 32 {
            return Err(invalid("Bundled CPython tree exceeds the depth limit"));
        }
        for entry in fs::read_dir(&directory).map_err(|error| fs_error(&directory, error))? {
            let entry = entry.map_err(|error| fs_error(&directory, error))?;
            entries += 1;
            if entries > MAX_FILES {
                return Err(invalid("Bundled CPython tree exceeds the file limit"));
            }
            let name = entry.file_name();
            if skip_nonruntime
                && (matches!(
                    name.to_str(),
                    Some("__pycache__" | "site-packages" | "pkgconfig")
                ) || Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext == "a" || ext == "pyc"))
            {
                continue;
            }
            let path = entry.path();
            let contained = path
                .canonicalize()
                .map_err(|error| fs_error(&path, error))?;
            if !contained.starts_with(&root) {
                return Err(invalid(format!(
                    "Bundled CPython runtime entry '{}' escapes its installation",
                    path.display()
                )));
            }
            let target = relative.join(name);
            let meta = contained
                .metadata()
                .map_err(|error| fs_error(&contained, error))?;
            if meta.is_file() {
                total = total.saturating_add(meta.len());
                if total > MAX_BYTES {
                    return Err(invalid("Bundled CPython tree exceeds the byte limit"));
                }
                files.push((target, contained));
            } else if meta.is_dir() {
                pending.push((contained, target, depth + 1));
            } else {
                return Err(invalid("Bundled CPython tree contains a special file"));
            }
        }
    }
    Ok(files)
}
