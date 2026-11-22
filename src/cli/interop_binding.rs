//! Prepare a bridge under the existing project cache and publish complete
//! content-addressed runtime directories beside built executables.

use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::Instant,
};

use sha2::{Digest, Sha256};

use crate::{
    error::{KuError, KuResult},
    interop::{self, RELATIVE_RUNTIME_ROOT},
    package::DependencyResolveMode,
};

use super::{acquire_build_file_lock_until, BuildLockMode, BUILD_LOCK_TIMEOUT};

const IDENTITY: &str = ".ku-bridge-identity";
const LOCAL_PYTHON: &str = "interop-python-path";
const MAX_FILES: usize = 20_000;
const MAX_BYTES: u64 = 512 * 1024 * 1024;

pub(super) struct Delivery<'a> {
    pub(super) output_dir: &'a Path,
    pub(super) matches_host: bool,
}

pub(super) fn prepare(
    import_root: &Path,
    package_cache: &Path,
    mode: DependencyResolveMode,
    delivery: Option<&Delivery<'_>>,
) -> KuResult<String> {
    prepare_with_python(import_root, package_cache, mode, delivery, None)
}

pub(super) fn prepare_with_python(
    import_root: &Path,
    package_cache: &Path,
    mode: DependencyResolveMode,
    delivery: Option<&Delivery<'_>>,
    explicit_python: Option<&Path>,
) -> KuResult<String> {
    let descriptor = import_root.join("ku.interop");
    let manifest = read_small(&descriptor)?;
    let kind = manifest.lines().next().unwrap_or_default().trim();
    if delivery.is_some_and(|output| !output.matches_host) {
        return Err(KuError::message(
            "interop bridge target is not the build host; restore and build on the destination ABI",
        ));
    }
    let remembered = if kind == "kind = \"python\""
        && explicit_python.is_none()
        && std::env::var_os("KU_PYTHON").is_none()
    {
        match read_text(&package_cache.join(LOCAL_PYTHON), 8192) {
            Ok(value) => Some(PathBuf::from(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(&package_cache.join(LOCAL_PYTHON), error)),
        }
    } else {
        None
    };
    let python = explicit_python.or(remembered.as_deref());
    let compiler = match kind {
        "kind = \"native\"" => interop::c_ffi::compiler_identity(import_root)?,
        "kind = \"python\"" => interop::python::compiler_identity(import_root, python)?,
        _ => {
            return Err(KuError::message(
                "ku.interop must declare kind = \"native\" or kind = \"python\"",
            ))
        }
    };
    let fingerprint =
        tree_digest(import_root, None).map_err(|error| io_error(import_root, error))?;
    let key = format!(
        "{:x}",
        Sha256::digest(format!(
            "ku-interop-v1\0{}\0{}\0{}\0{}\0{}\0{}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            env!("KU_BUILD_TARGET"),
            interop::implementation_digest(),
            compiler,
            fingerprint,
        ))
    );
    let cache = package_cache.join("interop");
    ensure_directory(&cache)?;
    let _guard = acquire_build_file_lock_until(
        &cache.join(format!("{key}.lock")),
        BuildLockMode::Exclusive,
        Instant::now() + BUILD_LOCK_TIMEOUT,
    )?;
    let cached = cache.join(&key);
    let library = match verified_bridge(&cached) {
        Ok(library) => library,
        Err(error) if mode == DependencyResolveMode::Offline => {
            return Err(KuError::message(format!(
                "offline interop artifact {} is missing or damaged: {error}; run ku i without --offline",
                cached.display()
            )));
        }
        Err(_) => {
            let stage = Stage::create(&cache)?;
            let plugin = match kind {
                "kind = \"native\"" => interop::c_ffi::prepare_bridge(import_root, &stage.0)?,
                "kind = \"python\"" => interop::python::prepare_bridge(
                    import_root,
                    &stage.0,
                    mode,
                    package_cache,
                    python,
                )?,
                _ => unreachable!("descriptor kind validated before cache access"),
            };
            let name = plugin
                .strip_prefix(&stage.0)
                .ok()
                .filter(|path| is_plain_leaf(path))
                .and_then(Path::to_str)
                .ok_or_else(|| {
                    KuError::message("interop bridge producer returned an invalid path")
                })?;
            let digest =
                tree_digest(&stage.0, Some(IDENTITY)).map_err(|error| io_error(&stage.0, error))?;
            fs::write(stage.0.join(IDENTITY), format!("{name}\n{digest}\n"))
                .map_err(|error| io_error(&stage.0, error))?;
            verified_bridge(&stage.0).map_err(|error| io_error(&stage.0, error))?;
            replace_tree(&stage.0, &cached)?;
            name.to_owned()
        }
    };
    if let Some(delivery) = delivery {
        let root = delivery.output_dir.join(RELATIVE_RUNTIME_ROOT);
        ensure_directory(&root)?;
        let _delivery_guard = acquire_build_file_lock_until(
            &root.join(format!("{key}.lock")),
            BuildLockMode::Exclusive,
            Instant::now() + BUILD_LOCK_TIMEOUT,
        )?;
        let destination = root.join(&key);
        if verified_bridge(&destination).as_ref().ok() != Some(&library) {
            let stage = Stage::create(&root)?;
            copy_tree(&cached, &stage.0).map_err(|error| io_error(&cached, error))?;
            verified_bridge(&stage.0).map_err(|error| io_error(&stage.0, error))?;
            replace_tree(&stage.0, &destination)?;
        }
        return Ok(format!("{RELATIVE_RUNTIME_ROOT}/{key}/{library}"));
    }
    let path = cached
        .join(&library)
        .canonicalize()
        .map_err(|error| io_error(&cached, error))?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| KuError::message("interop library path is not UTF-8"))
}

pub(super) fn remember_python(cache: &Path, path: &Path) -> KuResult<()> {
    let selection = if path.is_relative() && path.components().count() > 1 {
        std::env::current_dir()
            .map_err(|error| io_error(path, error))?
            .join(path)
    } else {
        path.to_path_buf()
    };
    let value = selection
        .to_str()
        .filter(|value| !value.is_empty() && value.len() <= 8192)
        .ok_or_else(|| KuError::message("Python interpreter selection must be a UTF-8 path"))?;
    let stage = Stage::create(cache)?;
    let source = stage.0.join(LOCAL_PYTHON);
    let destination = cache.join(LOCAL_PYTHON);
    fs::write(&source, value).map_err(|error| io_error(&source, error))?;
    fs::rename(&source, &destination).map_err(|error| io_error(&destination, error))
}

fn io_error(path: &Path, error: io::Error) -> KuError {
    KuError::message(format!("interop artifact {}: {error}", path.display()))
}

fn is_plain_leaf(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn read_small(path: &Path) -> KuResult<String> {
    read_text(path, 1024 * 1024).map_err(|error| io_error(path, error))
}

fn read_text(path: &Path, limit: u64) -> io::Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected file type or size",
        ));
    }
    let mut value = String::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut value)?;
    if value.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds interop size limit",
        ));
    }
    Ok(value)
}

fn verified_bridge(root: &Path) -> io::Result<String> {
    let identity = read_text(&root.join(IDENTITY), 512)?;
    let mut fields = identity.lines();
    let library = fields
        .next()
        .filter(|file| is_plain_leaf(Path::new(file)))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid interop library filename",
            )
        })?;
    let expected = fields
        .next()
        .filter(|value| value.len() == 64)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid interop artifact digest",
            )
        })?;
    if fields.next().is_some()
        || !fs::symlink_metadata(root.join(library))?.is_file()
        || tree_digest(root, Some(IDENTITY))? != expected
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "interop artifact digest mismatch",
        ));
    }
    Ok(library.to_owned())
}

fn ensure_directory(path: &Path) -> KuResult<()> {
    fs::create_dir_all(path).map_err(|error| io_error(path, error))?;
    if !fs::symlink_metadata(path)
        .map_err(|error| io_error(path, error))?
        .is_dir()
    {
        return Err(KuError::message(format!(
            "interop directory {} is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

struct Stage(PathBuf);

impl Stage {
    fn create(parent: &Path) -> KuResult<Self> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|error| KuError::message(error.to_string()))?;
        let leaf = format!(".ku-interop-{:032x}", u128::from_ne_bytes(nonce));
        let path = parent.join(leaf);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|error| io_error(&path, error))?;
        Ok(Self(path))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn replace_tree(stage: &Path, destination: &Path) -> KuResult<()> {
    // Other readers use this same cache/delivery lock. Keep an existing
    // directory available for rollback until the replacement is published.
    let backup = Stage::create(destination.parent().expect("interop output directory"))?;
    let old = backup.0.join("old");
    let previous = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => {
            fs::rename(destination, &old).map_err(|error| io_error(destination, error))?;
            true
        }
        Ok(_) => {
            return Err(KuError::message(
                "interop artifact path has unsafe file type",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(io_error(destination, error)),
    };
    if let Err(error) = fs::rename(stage, destination) {
        if previous {
            if let Err(restore_error) = fs::rename(&old, destination) {
                std::mem::forget(backup);
                return Err(KuError::message(format!(
                    "interop replace failed: {error}; rollback at {} failed: {restore_error}",
                    old.display()
                )));
            }
        }
        return Err(io_error(destination, error));
    }
    Ok(())
}

fn tree_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe interop directory",
        ));
    }
    let mut pending = vec![(PathBuf::new(), 0)];
    let mut files = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    while let Some((relative, depth)) = pending.pop() {
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let name: OsString = entry.file_name();
            let path = relative.join(name);
            let metadata = fs::symlink_metadata(entry.path())?;
            count += 1;
            if count > MAX_FILES || depth >= 64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "interop file or depth limit exceeded",
                ));
            }
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() {
                bytes += metadata.len();
                if bytes > MAX_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "interop artifact byte limit exceeded",
                    ));
                }
                files.push(path);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "interop tree contains a link or special file",
                ));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn tree_digest(root: &Path, exclude: Option<&str>) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut remaining = MAX_BYTES;
    let mut buffer = [0; 64 * 1024];
    for path in tree_files(root)? {
        if exclude.is_some_and(|name| path == Path::new(name)) {
            continue;
        }
        let mut file = fs::File::open(root.join(&path))?;
        let length = file.metadata()?.len();
        let name = path
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 interop path"))?
            .replace('\\', "/");
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(length.to_le_bytes());
        let mut total = 0;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            remaining = remaining.checked_sub(count as u64).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "interop artifact byte limit exceeded",
                )
            })?;
            hash.update(&buffer[..count]);
        }
        if total != length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "interop file changed during verification",
            ));
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    for path in tree_files(source)? {
        let output = destination.join(&path);
        fs::create_dir_all(output.parent().expect("interop file parent"))?;
        fs::copy(source.join(&path), output)?;
    }
    Ok(())
}
