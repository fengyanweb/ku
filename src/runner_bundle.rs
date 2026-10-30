//! Runner packaging identity. Release resolution never searches a source tree.
use crate::{
    cli::{run_build_process_bounded, run_build_process_capture_stdout, TempBuildDir},
    error::{KuError, KuResult},
    span::Span,
    stdlib::json,
    value::Value,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const MANIFEST: &str = "ku-runtime.json";
const LIMIT: u64 = 1_000_000;
const FILE_LIMIT: usize = 1000;
const PAYLOAD_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustIdentity {
    pub release: String,
    pub commit_hash: String,
    pub commit_date: String,
    pub host: String,
    pub toolchain: String,
}
impl RustIdentity {
    fn built() -> Self {
        Self {
            release: env!("KU_BUILD_RUSTC_RELEASE").into(),
            commit_hash: env!("KU_BUILD_RUSTC_COMMIT").into(),
            commit_date: env!("KU_BUILD_RUSTC_DATE").into(),
            host: env!("KU_BUILD_RUSTC_HOST").into(),
            toolchain: format!(
                "{}-{}",
                env!("KU_BUILD_RUSTC_RELEASE"),
                env!("KU_BUILD_RUSTC_HOST")
            ),
        }
    }
    fn parse(text: &str) -> KuResult<Self> {
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(&format!("{key}: ")))
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| error(format!("rustc -vV is missing {key}")))
        };
        let release = field("release")?;
        let host = field("host")?;
        Ok(Self {
            toolchain: format!("{release}-{host}"),
            release,
            host,
            commit_hash: field("commit-hash")?,
            commit_date: field("commit-date")?,
        })
    }
    fn from_json(value: &Value) -> KuResult<Self> {
        Ok(Self {
            release: string(value, "release")?,
            commit_hash: string(value, "commit_hash")?,
            commit_date: string(value, "commit_date")?,
            host: string(value, "host")?,
            toolchain: string(value, "toolchain")?,
        })
    }
    fn compatible(&self, other: &Self) -> bool {
        self.release == other.release
            && self.commit_hash == other.commit_hash
            && self.commit_date == other.commit_date
            && self.host == other.host
    }
    fn value(&self) -> Value {
        object([
            ("release", Value::String(self.release.clone())),
            ("commit_hash", Value::String(self.commit_hash.clone())),
            ("commit_date", Value::String(self.commit_date.clone())),
            ("host", Value::String(self.host.clone())),
            ("toolchain", Value::String(self.toolchain.clone())),
        ])
    }
}
// This is deliberately a hidden Rust API: source-development probes compare the
// actual linked library with the CLI, instead of guessing from file mtimes.
#[doc(hidden)]
pub fn build_identity() -> &'static str {
    env!("KU_BUILD_IDENTITY_SHA256")
}

#[derive(Clone, Debug)]
struct FileEntry {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Debug)]
pub struct RuntimeBundle {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub mode: &'static str,
    pub source_commit: String,
    pub bundle_id: String,
    pub target: String,
    pub rustc: RustIdentity,
    pub runtime: String,
    files: Vec<FileEntry>,
}
#[derive(Clone, Debug)]
pub struct Compiler {
    pub executable: PathBuf,
    pub identity: RustIdentity,
    pub source: &'static str,
}
#[derive(Debug)]
pub struct Prepared {
    pub runtime: PathBuf,
    pub dependency_dirs: Vec<PathBuf>,
}
fn error(text: impl Into<String>) -> KuError {
    KuError::message(format!("Runner: {}", text.into()))
}
fn object<const N: usize>(values: [(&str, Value); N]) -> Value {
    Value::Object(values.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn member<'a>(v: &'a Value, key: &str) -> KuResult<&'a Value> {
    match v {
        Value::Object(map) => map
            .get(key)
            .ok_or_else(|| error(format!("manifest missing '{key}'"))),
        _ => Err(error("manifest requires an object")),
    }
}
fn string(v: &Value, key: &str) -> KuResult<String> {
    match member(v, key)? {
        Value::String(s) if !s.is_empty() && s.len() <= 32768 => Ok(s.clone()),
        _ => Err(error(format!("manifest '{key}' must be a nonempty string"))),
    }
}
fn number(v: &Value, key: &str) -> KuResult<u64> {
    match member(v, key)? {
        Value::Int(n) if *n >= 0 => Ok(*n as u64),
        _ => Err(error(format!(
            "manifest '{key}' must be a nonnegative integer"
        ))),
    }
}
fn parse_json_file(path: &Path) -> KuResult<Value> {
    let file =
        fs::File::open(path).map_err(|e| error(format!("cannot read {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| error(e.to_string()))?;
    if bytes.len() as u64 > LIMIT {
        return Err(error("manifest exceeds 1MB"));
    }
    let text = String::from_utf8(bytes).map_err(|_| error("manifest must be UTF-8"))?;
    match json::eval("decode", &[Value::String(text)], Span::default())? {
        Some(Value::Result { ok: true, value }) => Ok(*value),
        _ => Err(error(format!("invalid JSON in {}", path.display()))),
    }
}
fn hex_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn relative(path: &str) -> KuResult<PathBuf> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || Path::new(path).is_absolute()
    {
        return Err(error(format!(
            "invalid installation-relative path '{path}'"
        )));
    }
    Ok(PathBuf::from(path))
}
fn plain(path: &Path) -> bool {
    let Ok(m) = fs::symlink_metadata(path) else {
        return false;
    };
    if m.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
fn installed_path(root: &Path, name: &str) -> KuResult<PathBuf> {
    let rel = relative(name)?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        if !plain(&current) {
            return Err(error(format!(
                "missing or linked bundle file component: {}",
                current.display()
            )));
        }
    }
    let canonical = fs::canonicalize(&current).map_err(|e| error(e.to_string()))?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return Err(error(format!(
            "bundle file escapes installation root: {name}"
        )));
    }
    Ok(canonical)
}
fn file_entry(v: &Value) -> KuResult<FileEntry> {
    let entry = FileEntry {
        path: string(v, "path")?,
        bytes: number(v, "bytes")?,
        sha256: string(v, "sha256")?,
    };
    relative(&entry.path)?;
    if !hex_digest(&entry.sha256) || entry.bytes > PAYLOAD_LIMIT {
        return Err(error("invalid bundle digest or file size"));
    }
    Ok(entry)
}
fn verify_file(root: &Path, entry: &FileEntry, dest: Option<&Path>) -> KuResult<()> {
    let path = installed_path(root, &entry.path)?;
    let mut source = fs::File::open(&path).map_err(|e| error(e.to_string()))?;
    if source.metadata().map_err(|e| error(e.to_string()))?.len() != entry.bytes {
        return Err(error(format!("bundle size mismatch: {}", entry.path)));
    }
    let mut output = if let Some(dest) = dest {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| error(e.to_string()))?;
        }
        Some(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)
                .map_err(|e| error(e.to_string()))?,
        )
    } else {
        None
    };
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut block = [0u8; 65536];
    loop {
        let count = source.read(&mut block).map_err(|e| error(e.to_string()))?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or_else(|| error("bundle size overflow"))?;
        if bytes > entry.bytes {
            return Err(error(format!("bundle grew while reading: {}", entry.path)));
        }
        digest.update(&block[..count]);
        if let Some(out) = output.as_mut() {
            out.write_all(&block[..count])
                .map_err(|e| error(e.to_string()))?;
        }
    }
    if bytes != entry.bytes || format!("{:x}", digest.finalize()) != entry.sha256 {
        return Err(error(format!(
            "bundle SHA-256 mismatch: {}; reinstall the complete Ku archive",
            entry.path
        )));
    }
    Ok(())
}
fn hash_file(path: &Path) -> KuResult<(u64, String)> {
    let mut f = fs::File::open(path).map_err(|e| error(e.to_string()))?;
    let n = f.metadata().map_err(|e| error(e.to_string()))?.len();
    if n > PAYLOAD_LIMIT {
        return Err(error("development dependency too large"));
    }
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut block = [0u8; 65536];
    loop {
        let n = f.read(&mut block).map_err(|e| error(e.to_string()))?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > PAYLOAD_LIMIT {
            return Err(error("development dependency grew too large"));
        }
        hash.update(&block[..n]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}
impl RuntimeBundle {
    pub fn load(requested_target: Option<&str>) -> KuResult<Self> {
        let exe = env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|e| error(format!("cannot locate Ku executable: {e}")))?;
        Self::at(&exe, requested_target)
    }
    fn at(exe: &Path, requested_target: Option<&str>) -> KuResult<Self> {
        let root = exe
            .parent()
            .ok_or_else(|| error("executable has no installation root"))?
            .to_path_buf();
        let manifest = root.join(MANIFEST);
        if fs::symlink_metadata(&manifest).is_ok() && !plain(&manifest) {
            return Err(error("runtime manifest must not be a link/reparse point"));
        }
        if !manifest.exists() {
            let profile = PathBuf::from(env!("KU_BUILD_PROFILE_DIR"));
            let actual_profile = if root.file_name().is_some_and(|n| n == "deps") {
                root.parent().unwrap_or(&root)
            } else {
                &root
            };
            if fs::canonicalize(&profile).ok().as_deref() == Some(actual_profile)
                && Path::new(env!("KU_BUILD_SOURCE_ROOT"))
                    .join("Cargo.toml")
                    .is_file()
            {
                return Self::development(exe, actual_profile, requested_target);
            }
            return Err(error(format!("missing {} beside {}; keep the complete installation archive (CLI, runtime and deps) together; ku doctor --backend runner",MANIFEST,exe.display())));
        }
        if !plain(&manifest) {
            return Err(error("runtime manifest must be a plain file"));
        }
        let m = parse_json_file(&manifest)?;
        if string(&m, "schema")? != "ku-runtime-bundle-v1"
            || string(&m, "ku_version")? != env!("CARGO_PKG_VERSION")
        {
            return Err(error("runtime bundle schema/Ku version mismatch"));
        }
        let source_commit = string(&m, "source_commit")?;
        if source_commit != env!("KU_BUILD_SOURCE_COMMIT") {
            return Err(error(
                "CLI and runtime source commit differ; reinstall the complete bundle",
            ));
        }
        let rustc = RustIdentity::from_json(member(&m, "rustc")?)?;
        if !rustc.compatible(&RustIdentity::built()) {
            return Err(error("CLI and runtime compiler identities differ"));
        }
        let host = string(&m, "host")?;
        let target = string(&m, "runtime_target")?;
        if host != env!("KU_BUILD_HOST") || target != env!("KU_BUILD_TARGET") || host != rustc.host
        {
            return Err(error("CLI/runtime host, target or ABI mismatch"));
        }
        if requested_target.is_some_and(|t| t != target) {
            return Err(error(format!(
                "bundle only contains Runner runtime for {target}; no runtime for requested target"
            )));
        }
        if string(&m, "profile")? != env!("KU_BUILD_PROFILE") {
            return Err(error("CLI/runtime build profile mismatch"));
        }
        match member(&m, "features")? {
            Value::Array(v) if v.is_empty() => (),
            _ => return Err(error("this CLI expects the default empty feature set")),
        }
        let cli = file_entry(member(&m, "cli")?)?;
        if installed_path(&root, &cli.path)? != exe {
            return Err(error(
                "manifest CLI does not identify the running executable",
            ));
        }
        verify_file(&root, &cli, None)?;
        let bundle_id = string(&m, "bundle_id")?;
        if !hex_digest(&bundle_id) {
            return Err(error("invalid bundle_id"));
        }
        let runtime = string(&m, "runtime_rlib")?;
        let dep_name = string(&m, "dependency_manifest")?;
        let dep_path = installed_path(&root, &dep_name)?;
        let dep_hash = string(&m, "dependency_manifest_sha256")?;
        let (_, actual_hash) = hash_file(&dep_path)?;
        if !hex_digest(&dep_hash) || dep_hash != actual_hash {
            return Err(error("dependency manifest SHA-256 mismatch"));
        }
        let deps = parse_json_file(&dep_path)?;
        if string(&deps, "schema")? != "ku-runtime-dependencies-v1"
            || string(&deps, "bundle_id")? != bundle_id
            || string(&deps, "runtime_target")? != target
            || RustIdentity::from_json(member(&deps, "rustc")?)? != rustc
        {
            return Err(error("runtime/dependency manifest identities differ"));
        }
        let Value::Array(values) = member(&deps, "files")? else {
            return Err(error("dependency files must be an array"));
        };
        if values.is_empty() || values.len() > FILE_LIMIT {
            return Err(error("runtime dependency count outside 1..1000"));
        }
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        let mut total = 0u64;
        for value in values {
            let entry = file_entry(value)?;
            let key = if cfg!(windows) {
                entry.path.to_lowercase()
            } else {
                entry.path.clone()
            };
            if !seen.insert(key) {
                return Err(error("duplicate runtime dependency path"));
            }
            total = total
                .checked_add(entry.bytes)
                .ok_or_else(|| error("dependency size overflow"))?;
            if total > PAYLOAD_LIMIT {
                return Err(error("bundle dependency payload exceeds 2GiB"));
            }
            verify_file(&root, &entry, None)?;
            files.push(entry);
        }
        if !files
            .iter()
            .any(|f| f.path == runtime && f.path.ends_with(".rlib"))
        {
            return Err(error(
                "runtime_rlib is absent from the exact dependency manifest",
            ));
        }
        Ok(Self {
            root,
            executable: exe.to_path_buf(),
            mode: "release-bundle",
            source_commit,
            bundle_id,
            target,
            rustc,
            runtime,
            files,
        })
    }
    fn development(exe: &Path, root: &Path, requested: Option<&str>) -> KuResult<Self> {
        let target = env!("KU_BUILD_TARGET").to_string();
        if requested.is_some_and(|t| t != target) {
            return Err(error("source-development runtime has a different target; build the matching runtime explicitly"));
        }
        let mut names = Vec::new();
        let mut ku = Vec::new();
        let deps = root.join("deps");
        let mut count = 0;
        for item in
            fs::read_dir(&deps).map_err(|e| error(format!("development deps unavailable: {e}")))?
        {
            count += 1;
            if count > 16384 {
                return Err(error("development deps directory entry bound exceeded"));
            }
            let path = item.map_err(|e| error(e.to_string()))?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            if name.starts_with("libku-") && name.ends_with(".rlib") {
                ku.push(format!("deps/{name}"));
                continue;
            }
            if ["rlib", "a", "lib", "dll", "so", "dylib"]
                .iter()
                .any(|ext| path.extension().is_some_and(|e| e == *ext))
            {
                names.push(format!("deps/{name}"));
            }
        }
        let runtime = if root.join("libku.rlib").is_file() {
            "libku.rlib".to_string()
        } else if ku.len() == 1 {
            ku.remove(0)
        } else {
            return Err(error("ambiguous source-development runtime; run cargo build --lib --bin ku in an isolated target directory"));
        };
        names.push(runtime.clone());
        names.sort();
        if names.len() > FILE_LIMIT {
            return Err(error("too many development runtime dependencies"));
        }
        let mut files = Vec::new();
        let mut total = 0u64;
        for name in names {
            let path = installed_path(root, &name)?;
            let (bytes, sha256) = hash_file(&path)?;
            total += bytes;
            if total > PAYLOAD_LIMIT {
                return Err(error("development runtime dependencies exceed 2GiB"));
            }
            files.push(FileEntry {
                path: name,
                bytes,
                sha256,
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
            executable: exe.to_path_buf(),
            mode: "source-development",
            source_commit: env!("KU_BUILD_SOURCE_COMMIT").into(),
            bundle_id: build_identity().into(),
            target,
            rustc: RustIdentity::built(),
            runtime,
            files,
        })
    }
    pub fn prepare(&self, temporary: &Path) -> KuResult<Prepared> {
        let mut dirs = HashSet::new();
        for f in &self.files {
            let dest = temporary.join(relative(&f.path)?);
            verify_file(&self.root, f, Some(&dest))?;
            if let Some(parent) = dest.parent() {
                dirs.insert(parent.to_path_buf());
            }
        }
        let mut dependency_dirs: Vec<_> = dirs.into_iter().collect();
        dependency_dirs.sort();
        Ok(Prepared {
            runtime: temporary.join(&self.runtime),
            dependency_dirs,
        })
    }
    pub fn compiler(&self) -> KuResult<Compiler> {
        let installed = installed_toolchains();
        if let Some(path) = env::var_os("KU_RUNNER_RUSTC") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(error(
                    "KU_RUNNER_RUSTC must name one absolute rustc executable path",
                ));
            }
            let compiler = probe_compiler(&path, "explicit KU_RUNNER_RUSTC")?;
            if !self.rustc.compatible(&compiler.identity) {
                return Err(error(format!("explicit KU_RUNNER_RUSTC identity mismatch: needs {} ({}) for {}, got {} ({}); explicit selection was not ignored",self.rustc.release,self.rustc.commit_hash,self.target,compiler.identity.release,compiler.identity.commit_hash)));
            }
            return Ok(compiler);
        }
        let mut system = "unavailable".to_string();
        if let Some(path) = find_program("rustc") {
            match probe_compiler(&path, "system rustc") {
                Ok(c) => {
                    system = format!("{} ({})", c.identity.release, c.identity.commit_hash);
                    if self.rustc.compatible(&c.identity) {
                        return Ok(c);
                    }
                }
                Err(e) => system = e.message,
            }
        }
        if let Some(rustup) = find_program("rustup") {
            if installed.iter().any(|s| s == &self.rustc.toolchain) {
                let mut command = readonly_command(&rustup);
                command.args(["which", "--toolchain", &self.rustc.toolchain, "rustc"]);
                if let Ok(text) = capture(&mut command) {
                    let path = PathBuf::from(text.trim());
                    if path.is_absolute() {
                        let c = probe_compiler(&path, "installed exact rustup toolchain")?;
                        if self.rustc.compatible(&c.identity) {
                            return Ok(c);
                        }
                    }
                }
            }
        }
        Err(error(format!("cannot build: the available Rust compiler does not match this Ku runtime.\nKu: {}\nRequired: Rust {} commit {} target {}\nSystem: {}\nExact toolchain: {} (not available or incompatible)\nRun: ku doctor --backend runner\nInstall explicitly: ku doctor --install-runner-toolchain\nThis does not change your global Rust default.",env!("CARGO_PKG_VERSION"),self.rustc.release,self.rustc.commit_hash,self.target,system,self.rustc.toolchain)))
    }
    pub fn link_probe(&self, compiler: &Compiler) -> KuResult<()> {
        let temp =
            TempBuildDir::create_private("runner-doctor").map_err(|e| error(e.to_string()))?;
        let deps = self.prepare(&temp.path().join("runtime"))?;
        let source = temp.path().join("identity.rs");
        let output = temp.path().join(if cfg!(windows) {
            "identity.exe"
        } else {
            "identity"
        });
        fs::write(
            &source,
            "fn main() { print!(\"{}\", ku::runner_bundle::build_identity()); }",
        )
        .map_err(|e| error(e.to_string()))?;
        let mut command = compiler.command();
        command
            .arg("--edition=2021")
            .arg(&source)
            .arg("--extern")
            .arg(format!("ku={}", deps.runtime.display()))
            .arg("--target")
            .arg(&self.target)
            .arg("-o")
            .arg(&output);
        add_search_paths(&mut command, &deps);
        let (status, _) =
            run_build_process_capture_stdout(&mut command, Duration::from_secs(120), 65536)
                .map_err(|e| error(format!("bounded Runner link probe failed: {e}")))?;
        if !status.success() {
            return Err(error(format!("Runner link probe failed ({status}); compiler={}, target={}. Check platform linker/system libraries; use ku build -v for detailed compiler diagnostics.",compiler.executable.display(),self.target)));
        }
        let mut command = readonly_command(&output);
        let text = capture(&mut command)?;
        if text != build_identity() {
            return Err(error("CLI and selected source-development/runtime library build identities differ; rebuild CLI and library together"));
        }
        Ok(())
    }
}
impl Compiler {
    pub fn command(&self) -> Command {
        readonly_command(&self.executable)
    }
}
pub fn add_search_paths(command: &mut Command, deps: &Prepared) {
    for dir in &deps.dependency_dirs {
        command
            .arg("-L")
            .arg(format!("dependency={}", dir.display()))
            .arg("-L")
            .arg(format!("native={}", dir.display()));
    }
}
fn readonly_command(path: &Path) -> Command {
    let mut c = Command::new(path);
    c.env("RUSTUP_AUTO_INSTALL", "0");
    c
}
fn capture(command: &mut Command) -> KuResult<String> {
    let (status, bytes) = run_build_process_capture_stdout(command, PROBE_TIMEOUT, 65536)
        .map_err(|e| error(format!("bounded tool probe failed: {e}")))?;
    if !status.success() {
        return Err(error(format!("tool probe exited with {status}")));
    }
    String::from_utf8(bytes).map_err(|_| error("tool probe output is not UTF-8"))
}
fn find_program(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let cwd = env::current_dir().ok()?;
    for dir in env::split_paths(&path).take(4096) {
        let dir = if dir.is_absolute() {
            dir
        } else {
            cwd.join(dir)
        };
        let candidate = dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        });
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
fn installed_toolchains() -> Vec<String> {
    let Some(rustup) = find_program("rustup") else {
        return vec![];
    };
    let mut c = readonly_command(&rustup);
    c.args(["toolchain", "list"]);
    capture(&mut c)
        .ok()
        .map(|s| {
            s.lines()
                .filter_map(|l| l.split_whitespace().next())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn supported_readonly_rustup(version: &str) -> bool {
    version
        .split_whitespace()
        .nth(1)
        .and_then(|v| {
            let mut n = v.split('.').filter_map(|p| p.parse::<u32>().ok());
            Some((n.next()?, n.next()?) >= (1, 28))
        })
        .unwrap_or(false)
}
fn rustup_proxy_manager(path: &Path, canonical: &Path) -> KuResult<Option<PathBuf>> {
    if canonical.file_stem().is_some_and(|name| name == "rustup") {
        return Ok(Some(canonical.to_path_buf()));
    }
    let sibling = path.parent().map(|p| {
        p.join(if cfg!(windows) {
            "rustup.exe"
        } else {
            "rustup"
        })
    });
    for manager in sibling.into_iter().chain(find_program("rustup")) {
        if manager.is_file()
            && fs::metadata(&manager).map(|m| m.len()).ok()
                == fs::metadata(canonical).map(|m| m.len()).ok()
            && hash_file(&manager)? == hash_file(canonical)?
        {
            return Ok(Some(manager));
        }
    }
    Ok(None)
}

fn probe_compiler(path: &Path, source: &'static str) -> KuResult<Compiler> {
    // Never invoke a possibly old Rustup proxy with a missing cwd override:
    // direct installed binaries remain supported without Rustup.
    let canonical =
        fs::canonicalize(path).map_err(|e| error(format!("compiler unavailable: {e}")))?;
    let manager = rustup_proxy_manager(path, &canonical)?;
    if let Some(manager) = manager {
        let mut c = readonly_command(&manager);
        c.arg("--version");
        let version = capture(&mut c)?;
        if !supported_readonly_rustup(&version) {
            return Err(error(
                "read-only probing requires Rustup 1.28+ or a direct installed rustc path",
            ));
        }
    } else if !canonical
        .parent()
        .and_then(Path::parent)
        .is_some_and(|sysroot| sysroot.join("lib/rustlib").is_dir())
    {
        return Err(error("compiler is not a recognized installed rustc/sysroot or supported Rustup proxy; point KU_RUNNER_RUSTC to the real sysroot/bin/rustc executable"));
    }
    let mut c = readonly_command(path);
    c.arg("-vV");
    let identity = RustIdentity::parse(&capture(&mut c)?)?;
    let mut c = readonly_command(path);
    c.args(["--print", "sysroot"]);
    let sysroot = PathBuf::from(capture(&mut c)?.trim());
    if !sysroot.is_absolute() {
        return Err(error("rustc reported a relative sysroot"));
    }
    let executable = sysroot
        .join("bin")
        .join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
    let mut c = readonly_command(&executable);
    c.arg("-vV");
    let actual = RustIdentity::parse(&capture(&mut c)?)?;
    if !identity.compatible(&actual) {
        return Err(error(
            "compiler probe and resolved sysroot rustc identities differ",
        ));
    }
    let mut c = readonly_command(&executable);
    c.args(["--print", "target-libdir", "--target", &identity.host]);
    let libdir = PathBuf::from(capture(&mut c)?.trim());
    if !libdir.is_absolute() || !libdir.is_dir() {
        return Err(error(
            "matching compiler target standard library is missing",
        ));
    }
    let std_ready = fs::read_dir(&libdir)
        .map_err(|e| error(e.to_string()))?
        .take(4096)
        .filter_map(Result::ok)
        .any(|f| {
            f.file_name().to_string_lossy().starts_with("libstd-")
                && f.path().extension().is_some_and(|e| e == "rlib")
        });
    if !std_ready {
        return Err(error("matching compiler has no target libstd rlib"));
    }
    Ok(Compiler {
        executable,
        identity,
        source,
    })
}

pub fn run_doctor(args: &[String]) -> KuResult<()> {
    let mut as_json = false;
    let mut backend = None;
    let mut install = false;
    let mut it = args.iter().skip(2);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" if !as_json => as_json = true,
            "--backend" if backend.is_none() => {
                let value = it
                    .next()
                    .ok_or_else(|| error("doctor --backend requires runner or c"))?;
                if !matches!(value.as_str(), "runner" | "c") {
                    return Err(error("doctor backend must be runner or c"));
                }
                backend = Some(value.as_str());
            }
            "--install-runner-toolchain" if !install => install = true,
            _ => {
                return Err(error(format!(
                    "unknown or repeated doctor option '{arg}'; use ku doctor --help"
                )))
            }
        }
    }
    if install && (as_json || backend == Some("c")) {
        return Err(error(
            "toolchain installation cannot be combined with --json or --backend c",
        ));
    }
    let bundle = RuntimeBundle::load(None);
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let mut fields = BTreeMap::<String, Value>::new();
    fields.insert("schema".into(), Value::String("ku-doctor-v1".into()));
    fields.insert(
        "ku_version".into(),
        Value::String(env!("CARGO_PKG_VERSION").into()),
    );
    fields.insert(
        "source_commit".into(),
        Value::String(env!("KU_BUILD_SOURCE_COMMIT").into()),
    );
    fields.insert(
        "build_source_sha256".into(),
        Value::String(env!("KU_BUILD_SOURCE_SHA256").into()),
    );
    fields.insert(
        "build_identity_sha256".into(),
        Value::String(build_identity().into()),
    );
    fields.insert(
        "msrv".into(),
        Value::String(env!("CARGO_PKG_RUST_VERSION").into()),
    );
    fields.insert(
        "executable".into(),
        Value::String(
            env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
    );
    let installed = installed_toolchains();
    fields.insert(
        "system_rustc".into(),
        match find_program("rustc") {
            Some(path) => match probe_compiler(&path, "system rustc diagnostic") {
                Ok(c) => object([
                    ("path", Value::String(c.executable.display().to_string())),
                    ("identity", c.identity.value()),
                ]),
                Err(e) => object([("probe_error", Value::String(e.message))]),
            },
            None => Value::Null,
        },
    );
    if let Some(path) = find_program("rustup") {
        let mut c = readonly_command(&path);
        c.arg("--version");
        let version = capture(&mut c).unwrap_or_else(|e| e.message);
        // active-toolchain can consult a missing cwd override; disable automatic
        // installation and never run it on older proxies without that contract.
        let supported = version
            .split_whitespace()
            .nth(1)
            .and_then(|v| {
                let mut numbers = v.split('.').filter_map(|n| n.parse::<u32>().ok());
                Some((numbers.next()?, numbers.next()?) >= (1, 28))
            })
            .unwrap_or(false);
        let active = if supported {
            let mut c = readonly_command(&path);
            c.args(["show", "active-toolchain"]);
            capture(&mut c)
                .ok()
                .map(Value::String)
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        fields.insert(
            "rustup".into(),
            object([
                ("path", Value::String(path.display().to_string())),
                ("version", Value::String(version.trim().into())),
                ("active_toolchain", active),
            ]),
        );
    } else {
        fields.insert("rustup".into(), Value::Null);
    }
    fields.insert(
        "installed_toolchains".into(),
        Value::Array(installed.into_iter().map(Value::String).collect()),
    );
    if install {
        let b = bundle.as_ref().map_err(|e| {
            error(format!(
                "cannot install tools before validating the runtime bundle: {e}"
            ))
        })?;
        let rustup=find_program("rustup").ok_or_else(||error("rustup is not installed; install Rustup from https://rustup.rs/ or provide a matching standalone compiler with KU_RUNNER_RUSTC"))?;
        if b.rustc.toolchain != format!("{}-{}", b.rustc.release, b.rustc.host) {
            return Err(error(
                "bundle does not identify a supported exact Rustup release toolchain",
            ));
        }
        eprintln!(
            "Installing {} (minimal); global default/overrides are unchanged.",
            b.rustc.toolchain
        );
        let mut c = Command::new(rustup);
        c.args([
            "toolchain",
            "install",
            &b.rustc.toolchain,
            "--profile",
            "minimal",
            "--no-self-update",
        ]);
        let status = run_build_process_bounded(&mut c, Duration::from_secs(600))
            .map_err(|e| error(format!("bounded toolchain installation failed: {e}")))?;
        if !status.success() {
            return Err(error(format!(
                "Rustup installation failed: {status}; no ready marker was written"
            )));
        }
    }
    match bundle {
        Ok(b) => {
            fields.insert("installation_mode".into(), Value::String(b.mode.into()));
            fields.insert(
                "installation_root".into(),
                Value::String(b.root.display().to_string()),
            );
            fields.insert("bundle_id".into(), Value::String(b.bundle_id.clone()));
            fields.insert("runtime_target".into(), Value::String(b.target.clone()));
            fields.insert(
                "runtime_rlib".into(),
                Value::String(b.root.join(&b.runtime).display().to_string()),
            );
            fields.insert(
                "runtime_source_commit".into(),
                Value::String(b.source_commit.clone()),
            );
            fields.insert("required_rustc".into(), b.rustc.value());
            fields.insert("dependency_files".into(), Value::Int(b.files.len() as i64));
            match b.compiler() {
                Ok(c) => {
                    let probe = if backend == Some("runner") || install {
                        b.link_probe(&c)
                    } else {
                        Ok(())
                    };
                    fields.insert(
                        "selected_compiler".into(),
                        object([
                            ("path", Value::String(c.executable.display().to_string())),
                            ("source", Value::String(c.source.into())),
                            ("identity", c.identity.value()),
                            (
                                "link_probe_passed",
                                if backend == Some("runner") || install {
                                    Value::Bool(probe.is_ok())
                                } else {
                                    Value::Null
                                },
                            ),
                        ]),
                    );
                    if let Err(e) = probe {
                        problems.push(e.message);
                    }
                }
                Err(e) => {
                    if backend == Some("runner") || install {
                        problems.push(e.message)
                    } else {
                        warnings.push(e.message)
                    }
                }
            }
        }
        Err(e) => {
            fields.insert(
                "installation_mode".into(),
                Value::String("unverified/standalone-interpreter".into()),
            );
            if backend == Some("runner") || install {
                problems.push(e.message)
            } else {
                warnings.push(e.message)
            }
        }
    }
    match crate::cli::doctor_c_toolchain() {
        Ok(text) => {
            fields.insert("c_toolchain".into(), Value::String(text));
        }
        Err(e) => {
            if backend == Some("c") {
                problems.push(e.message)
            } else {
                warnings.push(format!("optional C backend: {}", e.message))
            }
        }
    }
    let ready = problems.is_empty();
    fields.insert("ready".into(), Value::Bool(ready));
    fields.insert(
        "requested_backend".into(),
        backend.map_or(Value::Null, |b| Value::String(b.into())),
    );
    fields.insert(
        "warnings".into(),
        Value::Array(warnings.iter().cloned().map(Value::String).collect()),
    );
    fields.insert(
        "errors".into(),
        Value::Array(problems.iter().cloned().map(Value::String).collect()),
    );
    if as_json {
        println!(
            "{}",
            json::stringify_value(
                &Value::Object(fields.into_iter().collect()),
                Span::default()
            )?
        );
    } else {
        println!(
            "Ku doctor {}\nExecutable: {}",
            env!("CARGO_PKG_VERSION"),
            env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
        for (key, value) in fields {
            if !matches!(key.as_str(), "warnings" | "errors") {
                println!("{key}: {}", json::stringify_value(&value, Span::default())?);
            }
        }
        for w in warnings {
            println!("WARN: {w}");
        }
        for p in &problems {
            println!("ERROR: {p}");
        }
    }
    if ready {
        Ok(())
    } else {
        Err(error("doctor checks failed; see reported errors"))
    }
}

#[cfg(test)]
#[path = "../build_support/build_identity.rs"]
mod configuration_identity;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, json::stringify_value(value, Span::default()).unwrap()).unwrap();
    }
    fn fixture(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join("deps")).unwrap();
        let name = if cfg!(windows) { "ku.exe" } else { "ku" };
        fs::write(root.join(name), b"test-cli").unwrap();
        fs::write(root.join("libku.rlib"), b"test-runtime").unwrap();
        fs::write(root.join("deps/libdep.rlib"), b"test-dep").unwrap();
        let entry = |name: &str| {
            let (bytes, digest) = hash_file(&root.join(name)).unwrap();
            object([
                ("path", Value::String(name.into())),
                ("bytes", Value::Int(bytes as i64)),
                ("sha256", Value::String(digest)),
            ])
        };
        let id = "a".repeat(64);
        let identity = RustIdentity::built();
        let deps = object([
            ("schema", Value::String("ku-runtime-dependencies-v1".into())),
            ("bundle_id", Value::String(id.clone())),
            ("rustc", identity.value()),
            (
                "runtime_target",
                Value::String(env!("KU_BUILD_TARGET").into()),
            ),
            (
                "files",
                Value::Array(vec![entry("libku.rlib"), entry("deps/libdep.rlib")]),
            ),
        ]);
        write_json(&root.join("ku-runtime-deps.json"), &deps);
        let manifest = object([
            ("schema", Value::String("ku-runtime-bundle-v1".into())),
            (
                "ku_version",
                Value::String(env!("CARGO_PKG_VERSION").into()),
            ),
            (
                "source_commit",
                Value::String(env!("KU_BUILD_SOURCE_COMMIT").into()),
            ),
            ("bundle_id", Value::String(id)),
            ("host", Value::String(env!("KU_BUILD_HOST").into())),
            (
                "runtime_target",
                Value::String(env!("KU_BUILD_TARGET").into()),
            ),
            ("rustc", identity.value()),
            ("profile", Value::String(env!("KU_BUILD_PROFILE").into())),
            ("features", Value::Array(vec![])),
            ("runtime_rlib", Value::String("libku.rlib".into())),
            (
                "dependency_manifest",
                Value::String("ku-runtime-deps.json".into()),
            ),
            (
                "dependency_manifest_sha256",
                Value::String(hash_file(&root.join("ku-runtime-deps.json")).unwrap().1),
            ),
            ("cli", entry(name)),
        ]);
        write_json(&root.join(MANIFEST), &manifest);
        fs::canonicalize(root.join(name)).unwrap()
    }
    #[test]
    fn exact_manifest_is_portable_and_ignores_unlisted_newer_libraries() {
        let temp = TempBuildDir::create_private("bundle-portable-test").unwrap();
        let initial = temp.path().join("first");
        let exe = fixture(&initial);
        assert_eq!(RuntimeBundle::at(&exe, None).unwrap().runtime, "libku.rlib");
        let moved = temp.path().join("中文 空格 relocated");
        fs::rename(&initial, &moved).unwrap();
        fs::write(
            moved.join("deps/libku-ffffffffffffffff.rlib"),
            b"wrong runtime with newer mtime",
        )
        .unwrap();
        let exe = fs::canonicalize(moved.join(exe.file_name().unwrap())).unwrap();
        let bundle = RuntimeBundle::at(&exe, None).unwrap();
        let selected = temp.path().join("selected");
        let prepared = bundle.prepare(&selected).unwrap();
        assert_eq!(fs::read(prepared.runtime).unwrap(), b"test-runtime");
        assert!(!selected.join("deps/libku-ffffffffffffffff.rlib").exists());
        assert_eq!(
            fs::read(selected.join("deps/libdep.rlib")).unwrap(),
            b"test-dep"
        );
    }
    #[test]
    fn corruption_missing_files_and_mixed_identity_fail_before_compilation() {
        let temp = TempBuildDir::create_private("bundle-corrupt-test").unwrap();
        let root = temp.path().join("bundle");
        let exe = fixture(&root);
        fs::write(root.join("deps/libdep.rlib"), b"tampered").unwrap();
        assert!(RuntimeBundle::at(&exe, None)
            .unwrap_err()
            .message
            .contains("mismatch"));
        fs::remove_file(root.join("deps/libdep.rlib")).unwrap();
        assert!(RuntimeBundle::at(&exe, None).is_err());
        fs::write(root.join("deps/libdep.rlib"), b"test-dep").unwrap();
        let mut manifest = parse_json_file(&root.join(MANIFEST)).unwrap();
        if let Value::Object(ref mut m) = manifest {
            m.insert("source_commit".into(), Value::String("0".repeat(40)));
        }
        write_json(&root.join(MANIFEST), &manifest);
        assert!(RuntimeBundle::at(&exe, None)
            .unwrap_err()
            .message
            .contains("source commit"));
    }
    #[test]
    fn bundle_paths_reject_traversal_windows_prefixes_and_link_components() {
        for name in [
            "../libku.rlib",
            "/tmp/libku.rlib",
            "deps/../libku.rlib",
            "deps//lib.rlib",
            "./libku.rlib",
            "C:/libku.rlib",
            r"deps\libku.rlib",
            "deps/./lib.rlib",
        ] {
            assert!(relative(name).is_err(), "{name}");
        }
        assert!(relative("deps/中文 空格.rlib").is_ok());
        #[cfg(unix)]
        {
            let temp = TempBuildDir::create_private("bundle-symlink-test").unwrap();
            let root = temp.path().join("bundle");
            let exe = fixture(&root);
            fs::remove_file(root.join("deps/libdep.rlib")).unwrap();
            fs::write(temp.path().join("outside"), b"test-dep").unwrap();
            std::os::unix::fs::symlink(temp.path().join("outside"), root.join("deps/libdep.rlib"))
                .unwrap();
            assert!(RuntimeBundle::at(&exe, None).is_err());
        }
    }
    #[test]
    fn compiler_release_alone_does_not_imply_compatibility() {
        let a = RustIdentity::built();
        let mut b = a.clone();
        b.commit_hash = "f".repeat(40);
        assert!(!a.compatible(&b));
        b = a.clone();
        b.host = "x86_64-pc-windows-gnu".into();
        assert!(!a.compatible(&b));
        b = a.clone();
        b.release = "1.96.0".into();
        assert!(!a.compatible(&b));
        assert!(a.compatible(&a));
    }
    #[test]
    fn malformed_oversized_and_foreign_target_manifests_are_not_development_fallbacks() {
        let temp = TempBuildDir::create_private("bundle-schema-test").unwrap();
        let root = temp.path().join("bundle");
        let exe = fixture(&root);
        assert!(RuntimeBundle::at(&exe, Some("different-target")).is_err());
        fs::write(root.join(MANIFEST), b"{broken json").unwrap();
        assert!(RuntimeBundle::at(&exe, None)
            .unwrap_err()
            .message
            .contains("invalid JSON"));
        fs::write(root.join(MANIFEST), vec![b' '; LIMIT as usize + 1]).unwrap();
        assert!(RuntimeBundle::at(&exe, None)
            .unwrap_err()
            .message
            .contains("exceeds"));
    }
    #[test]
    fn development_build_identity_includes_configuration_not_only_source() {
        let mut inputs = BTreeMap::new();
        inputs.insert("PROFILE".into(), "debug".into());
        inputs.insert("TARGET".into(), "aarch64-apple-darwin".into());
        inputs.insert("OPT_LEVEL".into(), "0".into());
        let baseline = configuration_identity::calculate("same-source", "same-full-rustc", &inputs);
        for (key, value) in [
            ("PROFILE", "release"),
            ("TARGET", "x86_64-pc-windows-msvc"),
            ("OPT_LEVEL", "3"),
            ("CARGO_ENCODED_RUSTFLAGS", "-C\u{1f}panic=abort"),
            ("CARGO_FEATURE_EXAMPLE", "1"),
            ("CARGO_PROFILE_RELEASE_LTO", "thin"),
        ] {
            let mut changed = inputs.clone();
            changed.insert(key.into(), value.into());
            assert_ne!(
                baseline,
                configuration_identity::calculate("same-source", "same-full-rustc", &changed),
                "missing identity input {key}"
            );
        }
        inputs.insert("UNRELATED_USER_ENV".into(), "not exported".into());
        assert_eq!(
            baseline,
            configuration_identity::calculate("same-source", "same-full-rustc", &inputs)
        );
    }
    #[test]
    fn hardlinked_and_copied_rustup_proxies_are_recognized_by_content() {
        let temporary = TempBuildDir::create_private("rustup-proxy-identity").unwrap();
        let manager = temporary.path().join(if cfg!(windows) {
            "rustup.exe"
        } else {
            "rustup"
        });
        fs::write(&manager, b"fixture Rustup binary bytes").unwrap();
        let rustc = temporary
            .path()
            .join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
        fs::hard_link(&manager, &rustc).unwrap();
        assert_eq!(
            rustup_proxy_manager(&rustc, &fs::canonicalize(&rustc).unwrap()).unwrap(),
            Some(manager.clone())
        );
        fs::remove_file(&rustc).unwrap();
        fs::copy(&manager, &rustc).unwrap();
        assert_eq!(
            rustup_proxy_manager(&rustc, &fs::canonicalize(&rustc).unwrap()).unwrap(),
            Some(manager)
        );
        assert!(!supported_readonly_rustup("rustup 1.27.1 (fixture)"));
        assert!(supported_readonly_rustup("rustup 1.28.1 (fixture)"));
    }
    #[cfg(unix)]
    #[test]
    fn old_hardlinked_proxy_is_not_probed_or_allowed_to_install() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = TempBuildDir::create_private("old-rustup-readonly").unwrap();
        let manager = temporary.path().join("rustup");
        fs::write(&manager,b"#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'rustup 1.27.1 (fixture)\\n'; exit 0; fi\n: >\"${0%/*}/unexpected-probe\"\nexit 1\n").unwrap();
        fs::set_permissions(&manager, fs::Permissions::from_mode(0o700)).unwrap();
        let rustc = temporary.path().join("rustc");
        fs::hard_link(&manager, &rustc).unwrap();
        assert!(probe_compiler(&rustc, "test old proxy")
            .unwrap_err()
            .message
            .contains("Rustup 1.28"));
        assert!(!temporary.path().join("unexpected-probe").exists());
    }
}
