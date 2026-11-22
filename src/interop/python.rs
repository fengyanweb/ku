//! Python package preparation. Only a running Ku program imports package code.

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use sha2::{Digest, Sha256};

use crate::{
    cli::{
        acquire_interop_artifact_lock, run_build_process_bounded, run_build_process_capture_stdout,
    },
    error::{KuError, KuResult},
    package::{self, DependencyResolveMode},
    span::Span,
    stdlib::json,
    value::Value,
};

#[path = "python_portable.rs"]
mod portable;

const HELPER: &str = include_str!("python_prepare.py");
const BRIDGE: &str = include_str!("python_bridge.c");
const ABI: &str = include_str!("abi.h");
const DESCRIPTOR: &str = "ku.python.json";
const SCHEMA: &str = "ku-python-v1";
const MAX_SOURCE: usize = 4 * 1024 * 1024;
const MAX_DESCRIPTOR: usize = 880_000;
const MAX_RUNTIME_LIBRARY: u64 = 128 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> KuError {
    KuError::package("python_interop", message, Span::default())
}

fn fs_error(path: &Path, error: io::Error) -> KuError {
    invalid(format!(
        "Cannot access Python package file '{}': {error}",
        path.display()
    ))
}

fn plain(path: &Path, max_bytes: u64) -> KuResult<File> {
    let meta = fs::symlink_metadata(path).map_err(|error| fs_error(path, error))?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > max_bytes {
        return Err(invalid(format!(
            "'{}' must be a plain file no larger than {max_bytes} bytes",
            path.display()
        )));
    }
    File::open(path).map_err(|error| fs_error(path, error))
}

fn read(path: &Path, max_bytes: usize) -> KuResult<Vec<u8>> {
    let mut value = Vec::new();
    plain(path, max_bytes as u64)?
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut value)
        .map_err(|error| fs_error(path, error))?;
    if value.len() > max_bytes {
        return Err(invalid(
            "Python package source changed beyond its size limit",
        ));
    }
    Ok(value)
}

fn parse_json(bytes: &[u8]) -> KuResult<Value> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| invalid("Python descriptor must be UTF-8"))?;
    let value = json::eval("decode", &[Value::String(text.to_owned())], Span::default())?;
    match value {
        Some(Value::Result { ok: true, value }) => Ok(*value),
        Some(Value::Result { ok: false, value }) => {
            Err(invalid(format!("Invalid Python descriptor JSON: {value}")))
        }
        _ => Err(invalid("Missing decoded Python descriptor")),
    }
}

fn object(value: Value) -> KuResult<HashMap<String, Value>> {
    let Value::Object(fields) = value else {
        return Err(invalid("Python descriptor must be an object"));
    };
    Ok(fields)
}

fn string<'a>(fields: &'a HashMap<String, Value>, key: &str) -> KuResult<&'a str> {
    match fields.get(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(invalid(format!(
            "Python descriptor field '{key}' must be a string"
        ))),
    }
}

fn encode(fields: &HashMap<String, Value>) -> KuResult<String> {
    json::stringify_value(&Value::Object(fields.clone()), Span::default())
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn python_tool(explicit: Option<&Path>) -> KuResult<OsString> {
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("KU_PYTHON").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }));
    if path.is_absolute() || path.components().count() > 1 || path.is_file() {
        return fs::canonicalize(&path)
            .map(PathBuf::into_os_string)
            .map_err(|error| fs_error(&path, error));
    }
    Ok(path.into_os_string())
}

fn compiler_tool() -> OsString {
    std::env::var_os("KU_INTEROP_CC").unwrap_or_else(|| {
        if cfg!(all(windows, target_env = "msvc")) {
            "clang".into()
        } else if cfg!(windows) {
            "gcc".into()
        } else {
            "cc".into()
        }
    })
}

fn uses_msvc_driver(tool: &OsStr) -> bool {
    cfg!(all(windows, target_env = "msvc"))
        && Path::new(tool)
            .file_stem()
            .and_then(OsStr::to_str)
            .is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "cl" | "clang-cl"))
}

fn run_helper(
    python: &OsString,
    working_dir: &Path,
    args: &[OsString],
    timeout: Duration,
) -> KuResult<HashMap<String, Value>> {
    let mut child = Command::new(python);
    child
        .current_dir(working_dir)
        .args(["-I", "-S", "-c", HELPER])
        .args(args);
    let (status, output) = run_build_process_capture_stdout(&mut child, timeout, MAX_DESCRIPTOR)
        .map_err(|error| invalid(format!(
            "Cannot use the selected CPython interpreter ({:?}); set KU_PYTHON or pass --python to ku i",
            error.kind()
        )))?;
    let fields = object(parse_json(&output).map_err(|_| {
        invalid(format!(
            "CPython preparation produced an invalid response (status {status})"
        ))
    })?)?;
    if let Ok(reason) = string(&fields, "error") {
        return Err(invalid(reason));
    }
    if !status.success() {
        return Err(invalid(format!("CPython preparation failed with {status}")));
    }
    Ok(fields)
}

fn directory(path: &Path) -> KuResult<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            return Ok(());
        }
        return Err(invalid(format!(
            "Expected a plain directory at '{}'",
            path.display()
        )));
    }
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path).map_err(|error| fs_error(path, error))?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                Ok(())
            } else {
                Err(invalid(format!(
                    "Expected a plain directory at '{}'",
                    path.display()
                )))
            }
        }
        Err(error) => Err(fs_error(path, error)),
    }
}

fn wheel_cache(package_cache: &Path) -> KuResult<PathBuf> {
    if !package_cache.exists() {
        fs::create_dir_all(package_cache).map_err(|error| fs_error(package_cache, error))?;
    }
    let path = package_cache.join("python-wheels");
    directory(&path)?;
    Ok(path)
}

fn is_identifier(s: &str) -> bool {
    s.len() <= 160
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn python_module_name(name: &str) -> KuResult<&str> {
    if name.len() <= 160
        && (name.is_empty() || (!name.starts_with('_') && name.split('.').all(is_identifier)))
    {
        Ok(name)
    } else {
        Err(invalid(
            "Use a dotted Python import name such as PIL.Image for --module",
        ))
    }
}

fn exported_name(name: &str) -> KuResult<String> {
    if !is_identifier(name) {
        return Err(invalid(format!("Unsupported Python export name '{name}'")));
    }
    let mut exported = String::with_capacity(name.len());
    for word in name.split('_').filter(|word| !word.is_empty()) {
        let (first, rest) = word.split_at(1);
        exported.push_str(&first.to_ascii_uppercase());
        exported.push_str(rest);
    }
    if !exported
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_uppercase())
    {
        return Err(invalid(format!(
            "Python name '{name}' needs an explicit Ku export name"
        )));
    }
    Ok(exported)
}

fn package_name(source_name: &str) -> KuResult<String> {
    let mut name = source_name.to_ascii_lowercase().replace(['.', '_'], "-");
    if !name.bytes().next().is_some_and(|c| c.is_ascii_lowercase()) {
        name = format!("python-{name}");
    }
    package::validate_package_name(&name, Span::default())?;
    Ok(name)
}

fn render_exports(package: &str, fields: &HashMap<String, Value>) -> KuResult<String> {
    let Some(Value::Array(exports)) = fields.get("exports") else {
        return Err(invalid("Python descriptor is missing its exports list"));
    };
    if exports.len() > 1024 {
        return Err(invalid("Python package defines too many automatic exports"));
    }
    let mut seen = HashSet::new();
    let mut wrapper = format!("// Python adapter for {package}\n");
    for export in exports {
        let Value::Object(export) = export else {
            return Err(invalid("Python exports must be objects"));
        };
        let name = exported_name(string(export, "name")?)?;
        let callable = string(export, "callable")?;
        if !is_identifier(callable) || callable.starts_with("json__") || !seen.insert(name.clone())
        {
            return Err(invalid(format!(
                "Invalid or conflicting Python export '{name}'"
            )));
        }
        let returns = string(export, "returns")?;
        if !matches!(returns, "int" | "str" | "bool" | "null") {
            return Err(invalid(format!(
                "Unsupported Python result '{returns}' for {name}"
            )));
        }
        let Some(Value::Array(parameters)) = export.get("parameters") else {
            return Err(invalid("Python export parameters must be an array"));
        };
        if parameters.len() > super::MAX_ARGUMENTS {
            return Err(invalid(format!(
                "Python export {name} exceeds 32 Ku arguments"
            )));
        }
        let parameters = parameters
            .iter()
            .enumerate()
            .map(|(i, parameter)| {
                let Value::Array(parts) = parameter else {
                    return Err(invalid("Python parameter must declare its type"));
                };
                let [Value::String(_), Value::String(ty)] = parts.as_slice() else {
                    return Err(invalid("Python parameter must contain a name and type"));
                };
                if !matches!(ty.as_str(), "str" | "int" | "bool") {
                    return Err(invalid(format!(
                        "Unsupported Python parameter '{ty}' for {name}"
                    )));
                }
                let view = if ty == "str" { "&" } else { "" };
                Ok(format!("{view}arg{i}: {ty}"))
            })
            .collect::<KuResult<Vec<_>>>()?;
        let operation = if matches!(export.get("dynamic"), Some(Value::Bool(true))) {
            format!("json__{callable}")
        } else {
            callable.to_owned()
        };
        let args = (0..parameters.len())
            .map(|i| format!(", arg{i}"))
            .collect::<String>();
        wrapper.push_str(&format!(
            "\nfn {name}({}): {returns}! {{\n    return __ku_interop_{returns}(\"self\", \"{operation}\"{args})\n}}\n",
            parameters.join(", ")
        ));
    }
    Ok(wrapper)
}

struct NewAdapter(PathBuf);

impl Drop for NewAdapter {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn new_adapter_staging(vendor: &Path) -> KuResult<NewAdapter> {
    for _ in 0..16 {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce)
            .map_err(|_| invalid("Cannot reserve private Python adapter staging"))?;
        let suffix = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let private = vendor.join(format!(".ku-py-{suffix}"));
        match fs::create_dir(&private) {
            Ok(()) => return Ok(NewAdapter(private)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(fs_error(&private, error)),
        }
    }
    Err(invalid(
        "Cannot create a unique Python adapter staging directory",
    ))
}

fn write_new(path: &Path, bytes: &[u8]) -> KuResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| fs_error(path, error))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| fs_error(path, error))
}

fn descriptor(import_root: &Path) -> KuResult<HashMap<String, Value>> {
    let fields = object(parse_json(&read(
        &import_root.join(DESCRIPTOR),
        MAX_DESCRIPTOR,
    )?)?)?;
    if string(&fields, "schema")? != SCHEMA {
        return Err(invalid("Unsupported Python package descriptor version"));
    }
    python_module_name(string(&fields, "module")?)?;
    if !matches!(fields.get("local"), Some(Value::Bool(_)))
        || !matches!(fields.get("artifacts"), Some(Value::Array(_)))
    {
        return Err(invalid(
            "Python descriptor must declare its adapter kind and pinned wheel list",
        ));
    }
    Ok(fields)
}

fn digest_file(path: &Path, limit: u64) -> KuResult<String> {
    let mut hash = Sha256::new();
    let n = io::copy(&mut plain(path, limit)?.take(limit + 1), &mut hash)
        .map_err(|error| fs_error(path, error))?;
    if n > limit {
        return Err(invalid("CPython library changed beyond its size limit"));
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn prepare_install(
    project: &Path,
    source: &str,
    python: Option<&Path>,
    module: Option<&str>,
) -> KuResult<PathBuf> {
    let project = fs::canonicalize(project).map_err(|error| fs_error(project, error))?;
    let python = python_tool(python)?;
    let module = python_module_name(module.unwrap_or(""))?;
    let vendor = project.join("vendor");
    directory(&vendor)?;
    let (name, origin, local_file, request) = if let Some(request) = source.strip_prefix("pypi:") {
        let name = request.split_once("==").map_or(request, |(name, _)| name);
        if name.is_empty()
            || !name
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"_.-".contains(&ch))
            || request.len() > 200
            || !request
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"_.-+=!".contains(&ch))
        {
            return Err(invalid(
                "Use pypi:distribution or pypi:distribution==version",
            ));
        }
        (package_name(name)?, source.to_owned(), None, Some(request))
    } else {
        let path = Path::new(source);
        let path = fs::canonicalize(path).map_err(|error| fs_error(path, error))?;
        let relative = path
            .strip_prefix(&project)
            .map_err(|_| invalid("A local Python source must be inside the consuming project"))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| invalid("Python source path must be UTF-8"))?;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| invalid("Python source must have a portable module name"))?;
        (
            package_name(stem)?,
            format!("path:{}", relative.replace('\\', "/")),
            Some(path),
            None,
        )
    };
    let destination = vendor.join(format!("ku-python-{name}"));
    let project_cache = package::discover_from_dir(&project)?
        .ok_or_else(|| invalid("Python install requires a Ku project"))?
        .cache_dir;
    if !project_cache.exists() {
        fs::create_dir_all(&project_cache).map_err(|error| fs_error(&project_cache, error))?;
    }
    let _lock =
        acquire_interop_artifact_lock(&project_cache.join(format!("python-adapter-{name}.lock")))?;
    if destination.exists() {
        if fs::symlink_metadata(&destination)
            .map_err(|error| fs_error(&destination, error))?
            .file_type()
            .is_symlink()
        {
            return Err(invalid(
                "Existing Python adapter directory must not be a symlink",
            ));
        }
        let mut existing = descriptor(&destination.join("src"))?;
        let matches_source = string(&existing, "origin")? == origin;
        let matches_module = module.is_empty() || string(&existing, "module")? == module;
        let matches_local = match &local_file {
            Some(path) => {
                string(&existing, "source_sha256")? == digest_file(path, MAX_SOURCE as u64)?
            }
            None => true,
        };
        if !matches_source || !matches_module || !matches_local {
            return Err(invalid(format!(
                "Python adapter '{}' already exists for different inputs; update the checked-in adapter explicitly",
                destination.display()
            )));
        }
        if let Some(request) = request {
            ensure_python_target(
                &mut existing,
                &destination,
                &project_cache,
                &python,
                request,
            )?;
        }
        return Ok(destination);
    }

    let staging = new_adapter_staging(&vendor)?;
    let src = staging.0.join("src");
    directory(&src)?;
    let local = local_file.is_some();
    let mut fields = if let Some(source) = &local_file {
        let bytes = read(source, MAX_SOURCE)?;
        let file = src.join("bridge.py");
        write_new(&file, &bytes)?;
        let mut description = run_helper(
            &python,
            &staging.0,
            &["discover".into(), file.into_os_string()],
            Duration::from_secs(20),
        )?;
        description.insert(
            "source_sha256".into(),
            text(&format!("{:x}", Sha256::digest(&bytes))),
        );
        description.insert("artifacts".into(), Value::Array(Vec::new()));
        description
    } else {
        let cache = wheel_cache(&project_cache)?;
        run_helper(
            &python,
            &staging.0,
            &[
                "select".into(),
                request.unwrap_or_default().into(),
                cache.into_os_string(),
                module.into(),
            ],
            Duration::from_secs(180),
        )?
    };
    fields.insert("schema".into(), text(SCHEMA));
    fields.insert("local".into(), Value::Bool(local));
    fields.insert(
        "module".into(),
        text(if module.is_empty() && local {
            "bridge"
        } else {
            module
        }),
    );
    fields.insert("origin".into(), text(&origin));
    let module_file = if module.is_empty() {
        "python"
    } else {
        module.rsplit('.').next().unwrap_or("python")
    };
    let wrapper = render_exports(&name, &fields)?;
    write_new(&src.join(format!("{module_file}.ku")), wrapper.as_bytes())?;
    write_new(&src.join("ku.interop"), b"kind = \"python\"\n")?;
    let text = encode(&fields)?;
    if text.len() > MAX_DESCRIPTOR {
        return Err(invalid(
            "Python dependency metadata exceeds the descriptor size limit",
        ));
    }
    write_new(&src.join(DESCRIPTOR), text.as_bytes())?;
    write_new(
        &staging.0.join("ku.mod"),
        format!(
            "name = \"{name}\"\nversion = \"0.0.0\"\nroot = \"src\"\ninterop.version = \"1\"\n"
        )
        .as_bytes(),
    )?;
    fs::rename(&staging.0, &destination).map_err(|error| fs_error(&destination, error))?;
    Ok(destination)
}

fn selected_wheels<'a>(
    fields: &'a HashMap<String, Value>,
    target: &Value,
) -> KuResult<Option<&'a [Value]>> {
    let Some(Value::Array(primary)) = fields.get("artifacts") else {
        return Err(invalid("Python descriptor has no primary wheel set"));
    };
    if primary.is_empty() || fields.get("target") == Some(target) {
        return Ok(Some(primary));
    }
    let Some(additional) = fields.get("additional_targets") else {
        return Ok(None);
    };
    let Value::Array(additional) = additional else {
        return Err(invalid(
            "Additional Python target selections must be an array",
        ));
    };
    if additional.len() > 16 {
        return Err(invalid(
            "Python descriptor exceeds 16 extra target selections",
        ));
    }
    for selection in additional {
        let Value::Object(selection) = selection else {
            return Err(invalid("Invalid additional Python target selection"));
        };
        if selection.get("target") == Some(target) {
            let Some(Value::Array(artifacts)) = selection.get("artifacts") else {
                return Err(invalid("Python target has no pinned wheel set"));
            };
            return Ok(Some(artifacts));
        }
    }
    Ok(None)
}

fn ensure_python_target(
    descriptor: &mut HashMap<String, Value>,
    destination: &Path,
    project_cache: &Path,
    python: &OsString,
    request: &str,
) -> KuResult<()> {
    let target = Value::Object(run_helper(
        python,
        destination,
        &["target".into()],
        Duration::from_secs(20),
    )?);
    if selected_wheels(descriptor, &target)?.is_some() {
        return Ok(());
    }
    let mut fixed = HashMap::new();
    let mut save_pins = |target: &HashMap<String, Value>| -> KuResult<()> {
        let Some(Value::Array(artifacts)) = target.get("artifacts") else {
            return Err(invalid("Python target has no pinned wheel set"));
        };
        for artifact in artifacts {
            let Value::Object(artifact) = artifact else {
                return Err(invalid("Invalid locked Python wheel"));
            };
            let name = string(artifact, "name")?;
            let version = string(artifact, "version")?;
            if let Some(previous) = fixed.insert(name.to_owned(), text(version)) {
                if previous != text(version) {
                    return Err(invalid(format!(
                        "Existing Python targets disagree on version of {name}"
                    )));
                }
            }
        }
        Ok(())
    };
    save_pins(descriptor)?;
    if let Some(Value::Array(additional)) = descriptor.get("additional_targets") {
        for selection in additional {
            let Value::Object(selection) = selection else {
                return Err(invalid("Invalid additional Python target selection"));
            };
            save_pins(selection)?;
        }
    }
    let cache = wheel_cache(project_cache)?;
    let mut selected = run_helper(
        python,
        destination,
        &[
            "select".into(),
            request.into(),
            cache.into_os_string(),
            "".into(),
            encode(&fixed)?.into(),
        ],
        Duration::from_secs(180),
    )?;
    if selected.get("target") != Some(&target) {
        return Err(invalid(
            "Python interpreter target changed during wheel selection",
        ));
    }
    let artifacts = selected
        .remove("artifacts")
        .ok_or_else(|| invalid("Python resolution returned no artifacts"))?;
    let selections = descriptor
        .entry("additional_targets".into())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(selections) = selections else {
        return Err(invalid(
            "Additional Python target selections must be an array",
        ));
    };
    if selections.len() >= 16 {
        return Err(invalid(
            "Python descriptor exceeds 16 extra target selections",
        ));
    }
    selections.push(Value::Object(HashMap::from([
        ("target".into(), target),
        ("artifacts".into(), artifacts),
    ])));
    let staged = new_adapter_staging(
        destination
            .parent()
            .ok_or_else(|| invalid("Missing adapter parent directory"))?,
    )?;
    let replacement = staged.0.join(DESCRIPTOR);
    let data = encode(descriptor)?;
    if data.len() > MAX_DESCRIPTOR {
        return Err(invalid(
            "Python dependency metadata exceeds the descriptor size limit",
        ));
    }
    write_new(&replacement, data.as_bytes())?;
    let destination = destination.join("src").join(DESCRIPTOR);
    fs::rename(&replacement, &destination).map_err(|error| fs_error(&destination, error))?;
    Ok(())
}

struct PythonToolchain {
    python: OsString,
    home: PathBuf,
    library: PathBuf,
    executable: PathBuf,
    include: PathBuf,
    release: String,
    minor: String,
    target: String,
    digest: String,
}

impl PythonToolchain {
    fn probe(working_dir: &Path, explicit: Option<&Path>) -> KuResult<Self> {
        let python = python_tool(explicit)?;
        let fields = run_helper(
            &python,
            working_dir,
            &["probe".into()],
            Duration::from_secs(20),
        )?;
        let library = PathBuf::from(string(&fields, "library")?);
        let digest = digest_file(&library, MAX_RUNTIME_LIBRARY)?;
        let target = fields
            .get("target")
            .ok_or_else(|| invalid("CPython reported no ABI target"))?;
        let Value::Object(target_fields) = target else {
            return Err(invalid("CPython target must be structured metadata"));
        };
        Ok(Self {
            python,
            home: PathBuf::from(string(&fields, "home")?),
            library,
            executable: PathBuf::from(string(&fields, "executable")?),
            include: PathBuf::from(string(&fields, "include")?),
            release: string(&fields, "release")?.to_owned(),
            minor: string(target_fields, "version")?.to_owned(),
            target: json::stringify_value(target, Span::default())?,
            digest,
        })
    }

    fn compiler_identity(&self, working_dir: &Path) -> KuResult<String> {
        let compiler = compiler_tool();
        let mut command = Command::new(&compiler);
        command
            .current_dir(working_dir)
            .arg(if uses_msvc_driver(&compiler) {
                "/Bv"
            } else {
                "--version"
            });
        let (_, version) =
            run_build_process_capture_stdout(&mut command, Duration::from_secs(10), 64_000)
                .map_err(|_| {
                    invalid("No supported host C compiler for the Python bridge; set KU_INTEROP_CC")
                })?;
        let identity = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.executable.display(),
            self.library.display(),
            self.include.display(),
            self.home.display(),
            self.release,
            self.target,
            self.digest,
            compiler.to_string_lossy()
        );
        let mut hash = Sha256::new();
        hash.update(identity.as_bytes());
        hash.update(version);
        if let Some(runtime) = portable::Runtime::selected(self)? {
            hash.update(runtime.identity()?.as_bytes());
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

pub(crate) fn compiler_identity(import_root: &Path, python: Option<&Path>) -> KuResult<String> {
    let toolchain = PythonToolchain::probe(import_root, python)?;
    toolchain.compiler_identity(import_root)
}

fn render_modules(resolved: &HashMap<String, Value>) -> KuResult<String> {
    let Some(Value::Object(modules)) = resolved.get("modules") else {
        return Err(invalid(
            "Python preparation did not report module ownership",
        ));
    };
    let mut records = modules.iter().collect::<Vec<_>>();
    records.sort_unstable_by_key(|&(name, _)| name);
    let formatted = records
        .into_iter()
        .map(|(module, owner)| {
            let Value::String(owner) = owner else {
                return Err(invalid("Invalid Python module owner"));
            };
            if !is_identifier(module) || owner.len() > 200 {
                return Err(invalid("Python module identity exceeds its bounds"));
            }
            Ok(format!("{{{}, {}}}, ", quote_c(module), quote_c(owner)))
        })
        .collect::<KuResult<String>>()?;
    Ok(format!("{formatted}{{NULL, NULL}}"))
}

fn quote_c(input: &str) -> String {
    let mut value = String::from("\"");
    for byte in input.bytes() {
        if byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\') || byte == b' ' {
            value.push(byte as char);
        } else {
            value.push_str(&format!("\\{byte:03o}"));
        }
    }
    value.push('"');
    value
}

fn path_str(path: &Path) -> KuResult<&str> {
    path.to_str()
        .ok_or_else(|| invalid("CPython toolchain paths must be UTF-8"))
}

fn library_rpath(library: &Path) -> KuResult<&Path> {
    library
        .ancestors()
        .find(|part| part.extension().is_some_and(|ext| ext == "framework"))
        .unwrap_or(library)
        .parent()
        .ok_or_else(|| invalid("CPython library is missing its containing directory"))
}

pub(crate) fn prepare_bridge(
    import_root: &Path,
    staging: &Path,
    mode: DependencyResolveMode,
    project_cache: &Path,
    python: Option<&Path>,
) -> KuResult<PathBuf> {
    let fields = descriptor(import_root)?;
    let local = matches!(fields.get("local"), Some(Value::Bool(true)));
    let toolchain = PythonToolchain::probe(staging, python)?;
    let target = parse_json(toolchain.target.as_bytes())?;
    let wheels = selected_wheels(&fields, &target)?.ok_or_else(|| invalid(format!(
        "No Python wheel lock covers this target {}; rerun ku i for the same PyPI package using this interpreter",
        toolchain.target
    )))?;
    let portable = portable::Runtime::selected(&toolchain)?;
    let library_path = path_str(&toolchain.library)?;
    let environment = format!(
        "{:x}",
        Sha256::digest(format!("{}\n{}", toolchain.digest, toolchain.target))
    );
    let declarations = wheels
        .iter()
        .map(|artifact| {
            let Value::Object(fields) = artifact else {
                return Err(invalid("Python wheel declaration must be an object"));
            };
            let name = string(fields, "name")?;
            let version = string(fields, "version")?;
            let sha = string(fields, "sha256")?;
            if name.len() > 200 || version.len() > 160 || sha.len() != 64 {
                return Err(invalid("Python wheel identity exceeds its bounds"));
            }
            Ok(format!(
                "{{{}, {}}}",
                quote_c(name),
                quote_c(&format!("{version} ({sha})"))
            ))
        })
        .collect::<KuResult<Vec<_>>>()?
        .join(", ");
    let module = string(&fields, "module")?;

    let src = staging.join("python-bridge.c");
    let abi = staging.join("abi.h");
    let sidecar = staging.join("bridge.py");
    let site = staging.join("site");
    let extension = std::env::consts::DLL_EXTENSION;
    let plugin = staging.join(format!("python-bridge.{extension}"));
    let mut link_library = toolchain.library.clone();
    let runtime_library = if cfg!(windows) {
        let filename = toolchain
            .library
            .file_name()
            .ok_or_else(|| invalid("Invalid CPython DLL name"))?;
        let dll = staging.join(filename);
        fs::copy(&toolchain.library, &dll).map_err(|error| fs_error(&dll, error))?;
        link_library = toolchain.home.join("libs").join(format!(
            "{}.lib",
            toolchain
                .library
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| invalid("CPython DLL name must be UTF-8"))?
        ));
        plain(&link_library, MAX_RUNTIME_LIBRARY)?;
        filename
            .to_str()
            .ok_or_else(|| invalid("CPython DLL name must be UTF-8"))?
    } else if let Some(runtime) = &portable {
        runtime.stage(staging)?;
        path_str(&runtime.library)?
    } else {
        library_path
    };
    let runtime_home = if portable.is_some() {
        "python"
    } else {
        path_str(&toolchain.home)?
    };
    let runtime_executable = if let Some(runtime) = &portable {
        path_str(&runtime.executable)?
    } else {
        path_str(&toolchain.executable)?
    };

    if local {
        let script = import_root.join("bridge.py");
        write_new(&sidecar, &read(&script, MAX_SOURCE)?)?;
    }
    let modules = if !wheels.is_empty() {
        directory(&site)?;
        let cache = wheel_cache(project_cache)?;
        let resolved = run_helper(
            &toolchain.python,
            staging,
            &[
                "unpack".into(),
                import_root.join(DESCRIPTOR).into_os_string(),
                cache.into_os_string(),
                site.into_os_string(),
                if mode == DependencyResolveMode::Offline {
                    "offline"
                } else {
                    "online"
                }
                .into(),
            ],
            Duration::from_secs(180),
        )?;
        render_modules(&resolved)?
    } else {
        "{NULL, NULL}".to_owned()
    };
    let origin = string(&fields, "origin")?;
    if origin.len() > 480 {
        return Err(invalid("Python provider origin is too long"));
    }

    let source_key = if local {
        digest_file(&sidecar, MAX_SOURCE as u64)?
    } else {
        String::new()
    };
    let definitions = format!(
        "#define KU_PY_HOME {}\n#define KU_PY_EXECUTABLE {}\n#define KU_PY_LIBRARY {}\n#define KU_PY_RELEASE {}\n#define KU_PY_ENV {}\n#define KU_PY_MODULE {}\n#define KU_PY_MODULE_KEY {}\n#define KU_PY_LOCAL {}\n#define KU_PY_WHEELS {}\n#define KU_PY_DISTS {}\n#define KU_PY_PORTABLE {}\n#define KU_PY_MODULE_OWNERS {}\n#define KU_PY_OWNER {}\n#define KU_PY_TARGET {}\n",
        quote_c(runtime_home), quote_c(runtime_executable), quote_c(runtime_library),
        quote_c(&toolchain.release), quote_c(&environment), quote_c(module),
        quote_c(&format!("_ku_python_{source_key}")), i32::from(local),
        i32::from(!wheels.is_empty()), declarations, i32::from(portable.is_some()),
        modules, quote_c(origin), quote_c(&toolchain.target)
    );
    write_new(&src, format!("{definitions}{BRIDGE}").as_bytes())?;
    write_new(&abi, ABI.as_bytes())?;

    let compiler_tool = compiler_tool();
    let mut compiler = Command::new(&compiler_tool);
    compiler.current_dir(staging);
    #[cfg(all(windows, target_env = "gnu"))]
    let workspace = {
        use super::gnu_paths::Workspace;
        let workspace = Workspace::create()?;
        workspace.copy_file(&src, Path::new("python-bridge.c"), MAX_SOURCE as u64)?;
        workspace.copy_file(&abi, Path::new("abi.h"), MAX_SOURCE as u64)?;
        workspace.copy_tree(&toolchain.include, Path::new("include"))?;
        workspace.copy_file(&link_library, Path::new("python.lib"), MAX_RUNTIME_LIBRARY)?;
        workspace.configure(&mut compiler);
        workspace
    };
    if uses_msvc_driver(&compiler_tool) {
        compiler
            .args(["/nologo", "/LD", "/O2", "/std:c11", "/I"])
            .arg(&toolchain.include)
            .arg(&src)
            .arg(format!("/Fe:{}", plugin.display()))
            .arg("/link")
            .arg(&link_library);
    } else {
        if cfg!(all(windows, target_env = "msvc")) {
            compiler.arg(format!("--target={}", env!("KU_BUILD_TARGET")));
        }
        compiler.args(["-std=c11", "-O2", "-fvisibility=hidden", "-I"]);
        #[cfg(all(windows, target_env = "gnu"))]
        compiler.arg("include");
        #[cfg(not(all(windows, target_env = "gnu")))]
        compiler.arg(&toolchain.include);
        compiler.arg(if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        });
        if !cfg!(windows) {
            compiler
                .args(["-fPIC", "-pthread", "-Xlinker", "-rpath", "-Xlinker"])
                .arg(if portable.is_some() {
                    Path::new(if cfg!(target_os = "macos") {
                        "@loader_path/python/lib"
                    } else {
                        "$ORIGIN/python/lib"
                    })
                } else {
                    library_rpath(&toolchain.library)?
                });
        }
        #[cfg(all(windows, target_env = "gnu"))]
        compiler.args(["python-bridge.c", "python.lib", "-o", "python-bridge.dll"]);
        #[cfg(not(all(windows, target_env = "gnu")))]
        {
            compiler.arg(&src).arg(&link_library);
            if cfg!(target_os = "linux") {
                compiler.arg("-ldl");
            }
            compiler.args(["-o"]).arg(&plugin);
        }
    }
    let status =
        run_build_process_bounded(&mut compiler, Duration::from_secs(60)).map_err(|error| {
            invalid(format!(
                "Unable to run the host Python bridge compiler: {error}"
            ))
        })?;
    if !status.success() {
        return Err(invalid(format!(
            "Python bridge compiler failed with {status}"
        )));
    }
    #[cfg(all(windows, target_env = "gnu"))]
    workspace.copy_output(Path::new("python-bridge.dll"), &plugin, MAX_RUNTIME_LIBRARY)?;
    plain(&plugin, MAX_RUNTIME_LIBRARY)?;
    for file in [
        src,
        abi,
        staging.join("python-bridge.obj"),
        staging.join("python-bridge.lib"),
        staging.join("python-bridge.exp"),
    ] {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(fs_error(&file, error)),
        }
    }
    Ok(plugin)
}
