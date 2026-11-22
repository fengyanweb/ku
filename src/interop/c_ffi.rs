//! Native package authoring. Clang reads declarations without running the library;
//! a verified, narrow companion implements the single Ku interop ABI.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::{
    cli::{run_build_process_bounded, run_build_process_capture_stdout},
    error::{KuError, KuResult},
    package::LEGAL_FILES,
    span::Span,
};

#[path = "native/codegen.rs"]
mod codegen;
#[path = "native/signatures.rs"]
mod signatures;

use signatures::load_exports;

use super::gnu_paths;

pub(crate) const ABI_HEADER: &str = include_str!("abi.h");
const DESCRIPTOR_LIMIT: usize = 64 * 1024;
const SOURCE_LIMIT: u64 = 32 * 1024 * 1024;
const MAX_SOURCES: usize = 128;
const MAX_EXPORTS: usize = 128;
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    C,
    Cpp,
}

impl Language {
    fn compiler(self) -> OsString {
        let (specific, standard, default) = match self {
            Self::C => ("KU_INTEROP_CC", "CC", "clang"),
            Self::Cpp => ("KU_INTEROP_CXX", "CXX", "clang++"),
        };
        std::env::var_os(specific)
            .or_else(|| std::env::var_os(standard))
            .unwrap_or_else(|| OsString::from(default))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct NativeDescriptor {
    language: Language,
    header: PathBuf,
    module: String,
    sources: Vec<PathBuf>,
    archives: Vec<PathBuf>,
    target: Option<String>,
    include: Vec<PathBuf>,
    system: Vec<String>,
    exports: BTreeMap<String, String>,
    returns: BTreeMap<String, String>,
    borrows: BTreeMap<String, String>,
    buffers: BTreeMap<String, (usize, usize)>,
    class: Option<String>,
    constructor: Option<String>,
    destructor: Option<String>,
    methods: BTreeMap<String, String>,
}

fn err(code: &str, message: impl Into<String>) -> KuError {
    KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "interop",
        code,
        message,
        Span::default(),
    )
}

fn io_failure(operation: &str, path: &Path, error: std::io::Error) -> KuError {
    err(
        "native_file",
        format!("{operation} '{}': {error}", path.display()),
    )
}

fn identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
        && text.len() <= 128
}

fn export_identifier(text: &str) -> bool {
    identifier(text) && text.as_bytes()[0].is_ascii_uppercase()
}

fn qualified_identifier(text: &str) -> bool {
    !text.is_empty() && text.len() <= 256 && text.split("::").all(identifier)
}

fn portable_path(text: &str) -> KuResult<PathBuf> {
    if text.len() > 512
        || text.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
    {
        return Err(err(
            "native_descriptor",
            format!("expected a package-relative native path, got '{text}'"),
        ));
    }
    Ok(PathBuf::from(text))
}

impl NativeDescriptor {
    fn read(root: &Path) -> KuResult<Self> {
        let path = root.join("ku.interop");
        let src = read_capped(&path, DESCRIPTOR_LIMIT)?;
        let src = std::str::from_utf8(&src)
            .map_err(|_| err("native_descriptor", "ku.interop must be UTF-8"))?;
        Self::parse(src)
    }

    fn parse(src: &str) -> KuResult<Self> {
        let mut fields = BTreeMap::new();
        for (index, line) in src.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, raw)) = line.split_once('=') else {
                return Err(err(
                    "native_descriptor",
                    format!("ku.interop line {} requires key = \"value\"", index + 1),
                ));
            };
            let key = key.trim();
            let raw = raw.trim();
            let value = raw
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .filter(|value| !value.contains(['"', '\0', '\n', '\r']))
                .ok_or_else(|| {
                    err(
                        "native_descriptor",
                        format!("ku.interop line {} requires a quoted value", index + 1),
                    )
                })?;
            if fields.insert(key, value).is_some() {
                return Err(err(
                    "native_descriptor",
                    format!("duplicate ku.interop key '{key}'"),
                ));
            }
        }
        if fields.remove("kind") != Some("native") {
            return Err(err(
                "native_descriptor",
                "native ku.interop requires kind = \"native\"",
            ));
        }
        let language = match fields.remove("language") {
            Some("c") => Language::C,
            Some("cpp") => Language::Cpp,
            _ => {
                return Err(err(
                    "native_descriptor",
                    "ku.interop language must be \"c\" or \"cpp\"",
                ))
            }
        };
        let header = portable_path(
            fields
                .remove("header")
                .ok_or_else(|| err("native_descriptor", "ku.interop requires header"))?,
        )?;
        let module = fields.remove("module").unwrap_or("native");
        let target = fields.remove("target").map(str::to_owned);
        if !identifier(module) {
            return Err(err(
                "native_descriptor",
                "ku.interop module must be an identifier",
            ));
        }
        let class = fields.remove("class").map(str::to_owned);
        let constructor = fields.remove("constructor").map(str::to_owned);
        let destructor = fields.remove("destructor").map(str::to_owned);
        let mut descriptor = Self {
            language,
            header,
            module: module.to_owned(),
            class,
            constructor,
            destructor,
            sources: Vec::new(),
            archives: Vec::new(),
            target,
            include: Vec::new(),
            system: Vec::new(),
            exports: BTreeMap::new(),
            returns: BTreeMap::new(),
            borrows: BTreeMap::new(),
            buffers: BTreeMap::new(),
            methods: BTreeMap::new(),
        };
        for (key, value) in fields {
            let (group, name) = key.split_once('.').ok_or_else(|| {
                err(
                    "native_descriptor",
                    format!("unknown ku.interop key '{key}'"),
                )
            })?;
            match group {
                "source" | "archive" | "include" | "system"
                    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    match group {
                        "source" => descriptor.sources.push(portable_path(value)?),
                        "include" => descriptor.include.push(portable_path(value)?),
                        "archive" => descriptor.archives.push(portable_path(value)?),
                        _ if matches!(value, "dl" | "m" | "pthread") => {
                            descriptor.system.push(value.to_owned())
                        }
                        _ => {
                            return Err(err(
                                "native_descriptor",
                                format!("unapproved native system library '{value}'"),
                            ))
                        }
                    }
                }
                "export" if export_identifier(name) && identifier(value) => {
                    descriptor.exports.insert(name.to_owned(), value.to_owned());
                }
                "borrow" if export_identifier(name) && value == "cstr" => {
                    descriptor.borrows.insert(name.to_owned(), value.to_owned());
                }
                "buffer" if export_identifier(name) => {
                    let indices = value
                        .split_once(':')
                        .and_then(|(address, length)| {
                            Some((
                                address.parse::<usize>().ok()?,
                                length.parse::<usize>().ok()?,
                            ))
                        })
                        .filter(|(address, length)| {
                            address != length && *address < 32 && *length < 32
                        });
                    let Some(indices) = indices else {
                        return Err(err("native_descriptor", "buffer must select distinct zero-based address and length positions, for example 0:1"));
                    };
                    descriptor.buffers.insert(name.to_owned(), indices);
                }
                "return" if export_identifier(name) => {
                    descriptor.returns.insert(name.to_owned(), value.to_owned());
                }
                "method" if export_identifier(name) => {
                    descriptor.methods.insert(name.to_owned(), value.to_owned());
                }
                _ => {
                    return Err(err(
                        "native_descriptor",
                        format!("invalid or unsupported ku.interop key '{key}'"),
                    ))
                }
            }
        }
        descriptor.validate()?;
        Ok(descriptor)
    }

    fn validate(&self) -> KuResult<()> {
        let files = self.sources.len() + self.archives.len();
        if files == 0 || files > MAX_SOURCES || self.include.len() > MAX_SOURCES {
            return Err(err(
                "native_descriptor",
                format!("ku.interop requires 1–{MAX_SOURCES} source or static archive files and at most {MAX_SOURCES} include directories"),
            ));
        }
        if !self.archives.is_empty()
            && self.target.as_ref().is_none_or(|target| {
                target.len() > 120
                    || target.is_empty()
                    || !target
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            })
        {
            return Err(err("native_descriptor", "native static archives require a target matching the compiler's -dumpmachine output"));
        }
        if self.archives.iter().any(|path| {
            !matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("a" | "lib")
            )
        }) {
            return Err(err(
                "native_descriptor",
                "only regular self-contained .a or .lib archives can be linked",
            ));
        }
        if self.exports.len() + self.methods.len() > MAX_EXPORTS {
            return Err(err(
                "native_descriptor",
                "ku.interop declares too many exports",
            ));
        }
        self.extensions()?;
        match (self.language, self.class.as_deref(), self.constructor.as_deref(), self.destructor.as_deref()) {
            (Language::C, None, None, None) if self.methods.is_empty() => Ok(()),
            (Language::Cpp, Some(class), Some(create), Some(drop))
                if qualified_identifier(class) && export_identifier(create) && export_identifier(drop)
                    && create != drop && !self.methods.contains_key(create) && !self.methods.contains_key(drop)
                    && self.exports.is_empty() && self.returns.is_empty() && self.borrows.is_empty() && self.buffers.is_empty() => Ok(()),
            _ => Err(err("native_descriptor", "C++ class interop requires a qualified class, distinct constructor/destructor exports, and explicit method signatures; free C ABI functions use the C provider")),
        }
    }

    fn extensions(&self) -> KuResult<()> {
        let allowed = match self.language {
            Language::C => &["c"][..],
            Language::Cpp => &["cc", "cpp", "cxx"][..],
        };
        if self.sources.iter().any(|source| {
            !source
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| allowed.contains(&ext))
        }) {
            return Err(err(
                "native_descriptor",
                "source extension must match the selected compiler language",
            ));
        }
        Ok(())
    }
}

pub(crate) fn compiler_identity(import_root: &Path) -> KuResult<String> {
    let descriptor = NativeDescriptor::read(import_root)?;
    let compiler = descriptor.language.compiler();
    let version = compiler_probe(&compiler, "--version")?;
    let target = compiler_probe(&compiler, "-dumpmachine")?;
    Ok(format!("{compiler:?}\n{version}\n{target}"))
}

fn compiler_probe(compiler: &OsString, flag: &str) -> KuResult<String> {
    let (status, stdout) = run_build_process_capture_stdout(
        Command::new(compiler).arg(flag),
        Duration::from_secs(15),
        16 * 1024,
    )
    .map_err(|error| {
        err(
            "native_compiler",
            format!("could not identify native compiler {compiler:?}: {error}"),
        )
    })?;
    if !status.success() {
        return Err(err(
            "native_compiler",
            format!("native compiler {compiler:?} {flag} failed"),
        ));
    }
    String::from_utf8(stdout).map_err(|_| err("native_compiler", "compiler identity is not UTF-8"))
}

pub(crate) fn prepare_bridge(import_root: &Path, staging: &Path) -> KuResult<PathBuf> {
    let root = fs::canonicalize(import_root)
        .map_err(|error| io_failure("resolve import root", import_root, error))?;
    let staging = fs::canonicalize(staging)
        .map_err(|error| io_failure("resolve build directory", staging, error))?;
    let descriptor = NativeDescriptor::read(&root)?;
    let compiler = descriptor.language.compiler();
    let target = if cfg!(windows) || descriptor.target.is_some() {
        Some(compiler_probe(&compiler, "-dumpmachine")?)
    } else {
        None
    };
    if let (Some(expected), Some(actual)) = (&descriptor.target, &target) {
        if actual.trim() != expected {
            return Err(err(
                "native_target",
                format!(
                    "native package requires compiler target '{expected}', found '{}'",
                    actual.trim()
                ),
            ));
        }
    }
    let windows_gnu = cfg!(windows) && target.as_deref().is_some_and(gnu_paths::is_target);
    let workspace = windows_gnu.then(gnu_paths::Workspace::create).transpose()?;
    let relative_source = Path::new("inputs");
    let (tool_source, tool_root) = match &workspace {
        Some(workspace) => {
            workspace.copy_tree(&root, relative_source)?;
            (workspace.path().join(relative_source), workspace.path())
        }
        None => (root, staging.as_path()),
    };
    let input_argument = |member: &Path, directory: bool| {
        let checked = path_in(&tool_source, member, directory)?;
        Ok::<_, KuError>(if workspace.is_some() {
            relative_source.join(member)
        } else {
            checked
        })
    };
    let exports = load_exports(&tool_source, &descriptor)?;
    let header = tool_root.join("ku_interop_abi.h");
    write_new(&header, ABI_HEADER.as_bytes())?;
    let bridge_leaf = if descriptor.language == Language::C {
        "ku_interop_bridge.c"
    } else {
        "ku_interop_bridge.cpp"
    };
    let bridge = tool_root.join(bridge_leaf);
    let bridge_source = codegen::producer_source(&descriptor, &exports)?;
    write_new(&bridge, bridge_source.as_bytes())?;
    let output_leaf = if cfg!(windows) {
        "ku_bridge.dll"
    } else if cfg!(target_os = "macos") {
        "libku_bridge.dylib"
    } else {
        "libku_bridge.so"
    };
    let output = staging.join(output_leaf);
    let mut command = Command::new(&compiler);
    if let Some(workspace) = &workspace {
        workspace.configure(&mut command);
    } else {
        command.current_dir(&staging);
    }
    command
        .arg(match descriptor.language {
            Language::C => "-std=gnu11",
            Language::Cpp => "-std=c++17",
        })
        .args(["-O2", "-fvisibility=hidden"]);
    if !cfg!(windows) {
        command.arg("-fPIC");
    }
    command.arg(if cfg!(target_os = "macos") {
        "-dynamiclib"
    } else {
        "-shared"
    });
    if cfg!(target_os = "linux") {
        command.arg("-Wl,--no-undefined");
    }
    if windows_gnu {
        // Retain a shared bridge and use the driver's static GNU runtimes,
        // including its choice of thread support, inside the bridge.
        command.args(["-static", "-static-libgcc"]);
        if descriptor.language == Language::Cpp {
            command.arg("-static-libstdc++");
        }
    }
    if descriptor.language == Language::Cpp && (!cfg!(windows) || windows_gnu) {
        command.arg("-pthread");
    }
    command.arg("-I");
    if workspace.is_some() {
        command.arg(relative_source).arg(bridge_leaf);
    } else {
        command.arg(&tool_source).arg(&bridge);
    }
    for path in &descriptor.include {
        command.arg("-I").arg(input_argument(path, true)?);
    }
    for path in &descriptor.sources {
        command.arg(input_argument(path, false)?);
    }
    for library in &descriptor.archives {
        let archive = path_in(&tool_source, library, false)?;
        let mut header = [0u8; 8];
        File::open(&archive)
            .and_then(|mut file| file.read_exact(&mut header))
            .map_err(|error| io_failure("inspect static archive", &archive, error))?;
        if &header != b"!<arch>\n" {
            return Err(err(
                "native_target",
                format!(
                    "native archive '{}' is not a self-contained archive",
                    library.display()
                ),
            ));
        }
        command.arg(input_argument(library, false)?);
    }
    for library in &descriptor.system {
        if !cfg!(windows) && !(cfg!(target_os = "macos") && library == "dl") {
            command.arg(format!("-l{library}"));
        }
    }
    command.arg("-o");
    if workspace.is_some() {
        command.arg(output_leaf);
    } else {
        command.arg(&output);
    }
    let status = run_build_process_bounded(&mut command, BUILD_TIMEOUT).map_err(|error| {
        err(
            "native_build",
            format!("could not run native compiler {compiler:?}: {error}"),
        )
    })?;
    if !status.success() || !tool_root.join(output_leaf).is_file() {
        return Err(err(
            "native_build",
            format!(
                "could not build the '{}' interop companion using {compiler:?}",
                descriptor.module
            ),
        ));
    }
    if let Some(workspace) = &workspace {
        workspace.copy_output(Path::new(output_leaf), &output, 512 * 1024 * 1024)?;
        let notices = staging.join(gnu_paths::NOTICE_DIRECTORY);
        fs::create_dir(&notices)
            .map_err(|error| io_failure("create toolchain notices", &notices, error))?;
        write_new(&notices.join(gnu_paths::NOTICE_LEAF), gnu_paths::NOTICE)?;
    } else {
        fs::remove_file(&bridge)
            .map_err(|error| io_failure("remove bridge source staging", &bridge, error))?;
        fs::remove_file(&header)
            .map_err(|error| io_failure("remove header staging", &header, error))?;
    }
    Ok(output)
}

pub(crate) fn prepare_install(project: &Path, source: &Path) -> KuResult<PathBuf> {
    let project =
        fs::canonicalize(project).map_err(|error| io_failure("resolve project", project, error))?;
    let source = fs::canonicalize(source)
        .map_err(|error| io_failure("resolve native source", source, error))?;
    if !source.is_dir() || source == project || !source.starts_with(&project) {
        return Err(err(
            "native_source",
            "native SDK must be a directory inside the consumer project",
        ));
    }
    if source.join("ku.mod").is_file() {
        return Ok(source);
    }

    // Only infer the flat one-header, one-C-source layout. A vendor tree that
    // needs include ordering, compile configuration or a shim is authored explicitly.
    let mut headers = Vec::new();
    let mut sources = Vec::new();
    for (index, entry) in fs::read_dir(&source)
        .map_err(|error| io_failure("read native SDK", &source, error))?
        .enumerate()
    {
        if index >= MAX_SOURCES {
            return Err(err("native_source", "SDK has too many top-level files for automatic discovery; provide ku.mod and src/ku.interop"));
        }
        let entry = entry.map_err(|error| io_failure("read native SDK entry", &source, error))?;
        if !entry
            .file_type()
            .map_err(|error| io_failure("read SDK file type", &entry.path(), error))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("h") => headers.push(path),
            Some("c") => sources.push(path),
            _ => {}
        }
    }
    let ([header], [implementation]) = (headers.as_slice(), sources.as_slice()) else {
        return Err(err("native_source", "SDK needs one .h and one .c for automatic discovery, or a ku.mod and explicit src/ku.interop with its headers, sources and exports"));
    };
    let file_name = |path: &Path| -> KuResult<String> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| err("native_source", "SDK file has no portable UTF-8 file name"))?;
        portable_path(name)?;
        Ok(name.to_owned())
    };
    let source_name = file_name(&source)?;
    let mut name = source_name.to_ascii_lowercase().replace(['-', '.'], "_");
    if !identifier(&name) || !name.as_bytes()[0].is_ascii_alphabetic() {
        name.insert_str(0, "native_");
    }
    if !identifier(&name) {
        return Err(err(
            "native_source",
            "SDK folder name cannot be used as a portable Ku package name",
        ));
    }
    let output_root = project.join("ku-libraries");
    fs::create_dir_all(&output_root).map_err(|error| {
        io_failure(
            "create native package output directory",
            &output_root,
            error,
        )
    })?;
    if fs::symlink_metadata(&output_root)
        .map_err(|error| io_failure("inspect native package output", &output_root, error))?
        .file_type()
        .is_symlink()
    {
        return Err(err("native_source", "ku-libraries must not be a symlink"));
    }
    let output = output_root.join(&name);
    if output.exists() {
        return Err(err("native_source", format!("generated package already exists at {}; install that package directly to preserve its authored files", output.display())));
    }
    let pending = TempDirectory::new(&output_root)?;
    let src = pending.path.join("src");
    fs::create_dir(&src)
        .map_err(|error| io_failure("create generated source root", &src, error))?;
    for original in [header, implementation] {
        let destination = src.join(file_name(original)?);
        let bytes = read_capped(original, SOURCE_LIMIT as usize)?;
        write_new(&destination, &bytes)?;
    }
    for &notice in LEGAL_FILES {
        let original = source.join(notice);
        match fs::symlink_metadata(&original) {
            Ok(_) => {
                let bytes = read_capped(&original, DESCRIPTOR_LIMIT)?;
                write_new(&pending.path.join(notice), &bytes)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_failure("inspect notice", &original, error)),
        }
    }
    let declaration = format!(
        "kind = \"native\"\nlanguage = \"c\"\nmodule = \"native\"\nheader = {:?}\nsource.1 = {:?}\n",
        file_name(header)?, file_name(implementation)?
    );
    let descriptor = NativeDescriptor::parse(&declaration)?;
    let exports = load_exports(&src, &descriptor)?;
    let wrappers = codegen::wrapper_module(&descriptor, &exports)?;
    write_new(&src.join("ku.interop"), declaration.as_bytes())?;
    write_new(&src.join("native.ku"), wrappers.as_bytes())?;
    write_new(
        &pending.path.join("ku.mod"),
        format!("name = {name:?}\nversion = \"0.1.0\"\nroot = \"src\"\ninterop.version = \"1\"\n")
            .as_bytes(),
    )?;
    if output.exists() {
        return Err(err(
            "native_source",
            "another install already created the generated package",
        ));
    }
    fs::rename(&pending.path, &output)
        .map_err(|error| io_failure("publish generated native package", &output, error))?;
    Ok(output)
}

fn path_in(root: &Path, relative: &Path, directory: bool) -> KuResult<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(err(
                "native_source",
                "native input must stay inside the package",
            ));
        };
        path.push(component);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io_failure("inspect native input", &path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(err(
                "native_source",
                format!("native input '{}' is a symlink", path.display()),
            ));
        }
    }
    let metadata =
        fs::metadata(&path).map_err(|error| io_failure("inspect native input", &path, error))?;
    if (directory && metadata.is_dir())
        || (!directory && metadata.is_file() && metadata.len() <= SOURCE_LIMIT)
    {
        Ok(path)
    } else {
        Err(err(
            "native_source",
            format!(
                "native input '{}' is not a supported file or include directory",
                path.display()
            ),
        ))
    }
}

fn read_capped(path: &Path, limit: usize) -> KuResult<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_failure("inspect source", path, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return Err(err(
            "native_source",
            format!("'{}' is not a bounded regular source file", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| io_failure("read source", path, error))?;
    if bytes.len() > limit {
        return Err(err(
            "native_source",
            format!("'{}' exceeds the source read limit", path.display()),
        ));
    }
    Ok(bytes)
}

fn write_new(path: &Path, content: &[u8]) -> KuResult<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_failure("create generated native file", path, error))?;
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(|error| io_failure("write generated native file", path, error))
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(parent: &Path) -> KuResult<Self> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|error| {
            err(
                "native_source",
                format!("could not allocate staging identity: {error}"),
            )
        })?;
        let name = format!(".pending-{:032x}", u128::from_le_bytes(nonce));
        let path = parent.join(name);
        fs::create_dir(&path)
            .map_err(|error| io_failure("create private native staging directory", &path, error))?;
        Ok(Self { path })
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        // Only this process's exclusively created staging path is removed.
        let _ = fs::remove_dir_all(&self.path);
    }
}
