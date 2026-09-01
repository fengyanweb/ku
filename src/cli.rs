use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

use crate::{
    ast::*,
    backend,
    checker::Checker,
    error::{KuError, KuResult},
    interpreter::Interpreter,
    ir,
    lexer::Lexer,
    package::{self, DependencyResolveMode, PackageContext},
    parser::Parser,
    span::Span,
    stdlib,
};

const KU_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SOURCE_BYTES: u64 = 1_000_000;
const MAX_IMPORT_MODULES: usize = 4_096;
const MAX_IMPORT_DEPTH: usize = 32;
const MAX_IMPORT_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMPORT_EXPANDED_ITEMS: usize = 65_536;
const MAX_IMPORT_CLONED_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMPORT_EDGES: usize = 16_384;
const MAX_IMPORT_BINDINGS: usize = 16_384;
const MAX_RLIB_DIRECTORY_ENTRIES: usize = 16_384;
const BUILD_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
// Large enough that the MAX_CALL_DEPTH=512 guard trips (a clean Ku runtime
// error) before the interpreter's own Rust eval recursion overflows the OS
// thread stack. ~16KB of Rust stack per Ku call frame; 64MB clears 512 with
// wide margin. Reserved address space on Windows, not committed memory.
const INTERPRETER_STACK_SIZE: usize = 64 * 1024 * 1024;
const HELP: &str = "\
ku - simple, small, fast language tool

Usage:
  ku <file.ku>          Run a Ku source file
  ku create <name>      Create a new Ku project directory
  ku create <name> --template <template>
                        Create a project from a built-in template
  ku create --list      List built-in project templates
  ku init               Initialize the current directory as a Ku project
  ku init --template <template>
                        Initialize the current directory from a template
  ku template list      List built-in project templates
  ku run [--locked|--offline] [file.ku]
                        Run a package entry or Ku source file
  ku check [--locked|--offline] [file.ku]
                        Check a package entry or Ku source file without running it
  ku check --deny-unused [file.ku]
                        Treat unused local bindings as errors
  ku check --json       Check nearest ku.mod package and emit JSON Lines diagnostics
  ku check --json [--deny-unused] <file.ku>
                        Check and emit JSON Lines diagnostics
  ku ir <file.ku>       Print checked Ku IR draft
  ku llvm <file.ku>     Emit prototype LLVM text IR
  ku build [--locked|--offline] [file.ku]
                        Build a runnable executable package
  ku build .            Build the nearest ku.mod package
  ku build -o <path> [file.ku]
                        Build to an explicit executable path
  ku build --release [file.ku]
                        Build with release profile
  ku build --profile <debug|release|small|fast> [file.ku]
  ku build --emit-c [file.ku]
                        Also emit prototype native C source under .ku/build
  ku build --emit-ir [file.ku]
                        Also emit checked Ku IR draft under .ku/build
  ku build --backend c [--target <target>] [file.ku]
                        Build one native binary for host, x86_64-linux,
                        x86_64-windows, or aarch64-darwin
  ku build --native [--locked|--offline] <file.ku>
                        Compatibility form: emit prototype native C source beside file
  ku package gc [path]
                        Remove unused package cache entries for a package
  ku package pack [path]
                        Create a deterministic source package artifact
  ku package publish [path]
                        Publish through the configured HTTPS registry
  ku package yank [path]
                        Withdraw one published version without deleting its artifact
  ku package resolve [path] [--locked|--offline]
                        Resolve and cache the complete dependency graph
  ku version            Print version
  ku -v                 Print version
  ku -h                 Print this help
  ku -help              Print this help
  ku --help             Print this help
  ku help               Print this help

Examples:
  ku create hello
  ku create HelloWorld --template http
  ku init --template cli
  ku template list
  ku run
  ku run examples\\hello.ku
  ku check
  ku check examples\\error.ku
  ku ir examples\\function.ku
  ku llvm examples\\function.ku
  ku build examples\\hello.ku
  ku build --release -o dist\\hello.exe examples\\hello.ku
  ku build --backend c --release --target x86_64-linux .
  ku build --native examples\\function.ku
  ku package gc .
  ku package pack .
  ku package publish .
  ku package yank .
  ku package resolve . --locked
";

pub fn run_cli(args: Vec<String>) -> Result<(), KuError> {
    match args.get(1).map(String::as_str) {
        Some("create") => run_create_command(&args),
        Some("init") => run_init_command(&args),
        Some("template") => run_template_command(&args),
        Some("run") => {
            if args.get(2).is_some_and(|arg| arg == "build") {
                return Err(command_error(
                    "`ku run build` was removed; use the single build command `ku build`",
                ));
            }
            let (path, source, dependency_mode) =
                source_arg_or_project_with_dependency_mode(&args, "run")?;
            run_source_with_dependency_mode(&path_string(&path), &source, dependency_mode)
        }
        Some("check") => run_check_command(&args),
        Some("ir") => {
            let path = exact_path(&args, "ir")?;
            let source = read_ku_file(path)?;
            let program = parse_and_check(path, &source)?;
            let lowered = ir::lower_program(&program)?;
            print!("{}", ir::optimize_program(&lowered));
            Ok(())
        }
        Some("llvm") => {
            let path = exact_path(&args, "llvm")?;
            let source = read_ku_file(path)?;
            let output = build_llvm_ir(path, &source)?;
            println!("llvm ir ok: {}", output.display());
            Ok(())
        }
        Some("build") => {
            if args.get(2).is_some_and(|arg| arg == "--native") {
                // With -o/--output, `ku build --native <file> -o <out>` produces a
                // standalone native binary (identical to `--backend c`). Without an
                // output path it stays the compatibility form that only emits C.
                let wants_binary = args
                    .iter()
                    .skip(3)
                    .any(|arg| arg == "-o" || arg == "--output");
                if wants_binary {
                    let mut rewritten = vec![
                        args[0].clone(),
                        "build".to_string(),
                        "--backend".to_string(),
                        "c".to_string(),
                    ];
                    rewritten.extend(args.iter().skip(3).cloned());
                    run_build_command(&rewritten)?;
                } else {
                    let (path, dependency_mode) = parse_native_compat_args(&args)?;
                    let source = read_ku_path(&path)?;
                    let path = path_string(&path);
                    let output =
                        build_native_c_with_dependency_mode(&path, &source, dependency_mode)?;
                    println!("native c ok: {}", output.display());
                }
            } else {
                run_build_command(&args)?;
            }
            Ok(())
        }
        Some("package") => {
            let subcommand = args
                .get(2)
                .map(String::as_str)
                .ok_or_else(|| command_error("missing package command"))?;
            match subcommand {
                "gc" => {
                    let package = package_context_arg(&args, "package gc")?;
                    let removed = package::gc_cache(&package, 64)?;
                    println!("package gc ok: removed {removed} cache entries");
                    Ok(())
                }
                "pack" => {
                    let package = package_context_arg(&args, "package pack")?;
                    let artifact = package::pack_package(&package)?;
                    println!(
                        "package pack ok: {} {} {} bytes",
                        artifact.path.display(),
                        artifact.checksum,
                        artifact.size
                    );
                    Ok(())
                }
                "publish" => {
                    let package = package_context_arg(&args, "package publish")?;
                    let token = registry_token()?;
                    let receipt = package::publish_package(&package, &token)?;
                    println!("{}", package_publish_success_message(&receipt));
                    Ok(())
                }
                "yank" => {
                    let package = package_context_arg(&args, "package yank")?;
                    let token = registry_token()?;
                    let receipt = package::yank_package(&package, &token)?;
                    println!("{}", package_yank_success_message(&receipt));
                    Ok(())
                }
                "resolve" => {
                    let mut path = None::<PathBuf>;
                    let mut mode = package::DependencyResolveMode::Refresh;
                    for arg in args.iter().skip(3) {
                        match arg.as_str() {
                            "--locked" if mode == package::DependencyResolveMode::Refresh => {
                                mode = package::DependencyResolveMode::Locked;
                            }
                            "--offline" if mode == package::DependencyResolveMode::Refresh => {
                                mode = package::DependencyResolveMode::Offline;
                            }
                            value if value.starts_with('-') => {
                                return Err(command_error(format!(
                                    "unknown or conflicting package resolve option '{value}'"
                                )));
                            }
                            value if path.is_none() => path = Some(PathBuf::from(value)),
                            _ => {
                                return Err(command_error(
                                    "too many paths for 'ku package resolve'",
                                ));
                            }
                        }
                    }
                    let mut package = package_context_from_path(
                        path.as_deref().unwrap_or_else(|| Path::new(".")),
                    )?;
                    let deadline = package::package_operation_deadline();
                    let _usage_lease =
                        package::acquire_package_usage_lease_until(&package, deadline)?;
                    package::resolve_remote_dependencies_with_mode_until(
                        &mut package,
                        mode,
                        deadline,
                    )?;
                    if mode == package::DependencyResolveMode::Refresh {
                        package::write_lock(&package)?;
                    }
                    println!(
                        "package resolve ok: {} registry packages",
                        package.resolved_registry_dependencies.len()
                    );
                    Ok(())
                }
                _ => Err(command_error(format!(
                    "unknown package command '{subcommand}'"
                ))),
            }
        }
        Some("version") | Some("--version") | Some("-V") | Some("-v") => {
            reject_extra_args(&args, 2, "version")?;
            println!("ku {KU_VERSION}");
            Ok(())
        }
        Some("-h") | Some("-help") | Some("--help") | Some("help") => {
            reject_extra_args(&args, 2, "help")?;
            println!("{HELP}");
            Ok(())
        }
        Some(path) if is_ku_file(path) => {
            reject_extra_args(&args, 2, "run")?;
            let source = read_ku_file(path)?;
            run_source(path, &source)
        }
        Some(path) if looks_like_file_path(path) => {
            Err(command_error(expected_ku_file_message(path)))
        }
        Some(command) => Err(command_error(format!("unknown command '{command}'"))),
        None => Err(command_error("missing command")),
    }
}

pub(crate) fn package_publish_success_message(receipt: &package::PackagePublishReceipt) -> String {
    format!(
        "package publish ok: {}@{} {} {}",
        receipt.name, receipt.version, receipt.checksum, receipt.registry
    )
}

pub(crate) fn package_yank_success_message(receipt: &package::PackageYankReceipt) -> String {
    format!(
        "package yank ok: {}@{} {}",
        receipt.name, receipt.version, receipt.registry
    )
}

pub fn help_text() -> &'static str {
    HELP
}

fn registry_token() -> KuResult<String> {
    env::var(package::REGISTRY_TOKEN_ENV).map_err(|_| {
        command_error(format!(
            "missing or non-UTF-8 {} environment variable",
            package::REGISTRY_TOKEN_ENV
        ))
    })
}

fn package_context_arg(args: &[String], command: &str) -> KuResult<PackageContext> {
    if args.len() > 4 {
        return Err(command_error(format!(
            "too many arguments for 'ku {command}'"
        )));
    }
    let path = args
        .get(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    package_context_from_path(&path)
}

fn package_context_from_path(path: &Path) -> KuResult<PackageContext> {
    if !path.exists() {
        return Err(command_error(format!(
            "package path does not exist: '{}'",
            path.display()
        )));
    }
    let package = if path.is_dir() {
        package::discover_from_dir(path)?
    } else {
        package::discover_for_file(path)?
    };
    package.ok_or_else(|| KuError::message(format!("no ku.mod found for '{}'", path.display())))
}

#[derive(Debug, Clone, Copy)]
struct CheckOptions {
    deny_unused: bool,
    dependency_mode: DependencyResolveMode,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            deny_unused: false,
            dependency_mode: DependencyResolveMode::Update,
        }
    }
}

fn run_check_command(args: &[String]) -> Result<(), KuError> {
    let mut json = false;
    let mut options = CheckOptions::default();
    let mut selected_dependency_mode = None;
    let mut path = None::<String>;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--json" => json = true,
            "--deny-unused" => options.deny_unused = true,
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, "check")?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!("unknown check option '{value}'")));
            }
            value => {
                if path.is_some() {
                    return Err(command_error("too many arguments for 'ku check'"));
                }
                path = Some(value.to_string());
            }
        }
    }
    options.dependency_mode = selected_dependency_mode.unwrap_or(DependencyResolveMode::Update);

    let (path, source) = match path {
        Some(path) => {
            let path = PathBuf::from(path);
            let source = read_ku_path(&path).map_err(|err| {
                if json {
                    KuError::message(diagnostic_json_line(&err, &path_string(&path), ""))
                } else {
                    err
                }
            })?;
            (path, source)
        }
        None => project_entry_source(if json { "check --json" } else { "check" })?,
    };
    let path_text = path_string(&path);
    if json {
        parse_and_check_with_options(&path_text, &source, options)
            .map(|_| ())
            .map_err(|err| KuError::message(diagnostic_json_line(&err, &path_text, &source)))
    } else {
        check_source_with_options(&path_text, &source, options)?;
        println!("check ok: {path_text}");
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ProjectTemplate {
    name: &'static str,
    description: &'static str,
    is_lib: bool,
}

const PROJECT_TEMPLATES: &[ProjectTemplate] = &[
    ProjectTemplate {
        name: "basic",
        description: "minimal Ku project",
        is_lib: false,
    },
    ProjectTemplate {
        name: "cli",
        description: "command line tool",
        is_lib: false,
    },
    ProjectTemplate {
        name: "http",
        description: "HTTP server",
        is_lib: false,
    },
    ProjectTemplate {
        name: "json",
        description: "JSON processing example",
        is_lib: false,
    },
    ProjectTemplate {
        name: "fs",
        description: "file processing example",
        is_lib: false,
    },
    ProjectTemplate {
        name: "lib",
        description: "library project",
        is_lib: true,
    },
];

fn run_create_command(args: &[String]) -> Result<(), KuError> {
    if args.len() == 3 && args[2] == "--list" {
        return list_project_templates();
    }
    if args.len() < 3 {
        return Err(project_command_error(
            "create needs a project name",
            "help: use `ku create hello` or `ku create my-api --template http`",
        ));
    }
    let name = &args[2];
    let template = parse_template_option(args, 3, "create")?;
    validate_project_name(name)?;
    let path = PathBuf::from(name);
    if path.exists() {
        return Err(project_command_error(
            format!(
                "error[E1001]: project directory already exists\n   |\n   | ku create {name}\n   |           {}\n   |",
                "^".repeat(name.len().max(1))
            ),
            "help: choose another name, or use `ku init` inside the existing directory",
        ));
    }
    write_project_template(&path, name, template)?;
    println!("create ok: {}", path.display());
    println!("next: cd {name} && ku run");
    Ok(())
}

fn run_init_command(args: &[String]) -> Result<(), KuError> {
    let template = parse_template_option(args, 2, "init")?;
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let manifest = cwd.join("ku.mod");
    if manifest.exists() {
        return Err(project_command_error(
            "error[E1002]: Ku project already exists\n   |\nnote: found `ku.mod` in current directory",
            "help: use `ku run`, `ku build`, or remove `ku.mod` before running `ku init`",
        ));
    }
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_valid_project_name(name))
        .unwrap_or("ku_app");
    write_project_template(&cwd, name, template)?;
    println!("init ok: {}", cwd.display());
    println!("next: ku run");
    Ok(())
}

fn run_template_command(args: &[String]) -> Result<(), KuError> {
    match args.get(2).map(String::as_str) {
        Some("list") => {
            reject_extra_args(args, 3, "template list")?;
            list_project_templates()
        }
        Some(other) => Err(project_command_error(
            format!("unknown template command '{other}'"),
            "help: use `ku template list`",
        )),
        None => Err(project_command_error(
            "missing template command",
            "help: use `ku template list`",
        )),
    }
}

fn parse_template_option<'a>(
    args: &'a [String],
    mut index: usize,
    command: &str,
) -> Result<&'a ProjectTemplate, KuError> {
    let mut template = "basic";
    while index < args.len() {
        match args[index].as_str() {
            "--template" | "-t" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(project_command_error(
                        format!("missing template after {}", args[index - 1]),
                        format!("help: use `ku {command} --template http`"),
                    ));
                };
                template = value;
            }
            value if command == "create" && value == "--list" => {
                return Err(project_command_error(
                    "`ku create --list` does not take a project name",
                    "help: use `ku create --list` or `ku create <name> --template http`",
                ));
            }
            value => {
                return Err(project_command_error(
                    format!("unknown {command} option '{value}'"),
                    format!("help: use `ku {command} --template http`"),
                ));
            }
        }
        index += 1;
    }
    find_project_template(template).ok_or_else(|| {
        project_command_error(
            format!("error[E1003]: unknown template `{template}`\n   |"),
            "help: available templates: basic, cli, http, json, fs, lib",
        )
    })
}

fn find_project_template(name: &str) -> Option<&'static ProjectTemplate> {
    PROJECT_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}

fn list_project_templates() -> Result<(), KuError> {
    for template in PROJECT_TEMPLATES {
        println!("{:<8} {}", template.name, template.description);
    }
    Ok(())
}

fn write_project_template(
    path: &Path,
    name: &str,
    template: &ProjectTemplate,
) -> Result<(), KuError> {
    let manifest_path = path.join("ku.mod");
    let main_path = path.join("src").join("main.ku");
    if manifest_path.exists() || main_path.exists() {
        return Err(project_command_error(
            "project template target already exists",
            "help: move existing ku.mod/src/main.ku aside, or choose another project directory",
        ));
    }
    fs::create_dir_all(path.join("src")).map_err(|err| {
        KuError::message(format!(
            "failed to create project directory '{}': {err}",
            path.display()
        ))
    })?;
    let package_name = package_name_from_project_name(name);
    let manifest = project_manifest(&package_name, template);
    let main = project_main_source(template);
    fs::write(&manifest_path, manifest).map_err(|err| {
        KuError::message(format!(
            "failed to write '{}': {err}",
            manifest_path.display()
        ))
    })?;
    fs::write(&main_path, main).map_err(|err| {
        KuError::message(format!("failed to write '{}': {err}", main_path.display()))
    })?;
    Ok(())
}

fn project_manifest(name: &str, template: &ProjectTemplate) -> String {
    let mut manifest = format!(
        "name = \"{name}\"\nversion = \"0.1.0\"\nroot = \"src\"\ncache = \".ku/cache\"\nout = \".ku/build\"\n"
    );
    manifest.push_str("main = \"main.ku\"\n");
    manifest.push_str(&format!("template = \"{}\"\n", template.name));
    if template.is_lib {
        manifest.push_str("type = \"lib\"\n");
    }
    manifest
}

fn project_main_source(template: &ProjectTemplate) -> &'static str {
    match template.name {
        "basic" => {
            r#"fn main() {
    // `println` prints one line.
    println("Hello Ku")
}
"#
        }
        "cli" => {
            r#"fn main() {
    // Command line arguments will get a dedicated std API later.
    println("Ku CLI tool")
}
"#
        }
        "http" => {
            r#"import { http, time } from "std"

fn health() {
    return http.text("Ku HTTP OK")
}

fn index() {
    return http.text("Ku HTTP 123")
}

fn main(): null! {
    app = http.service()

    app.get("/", health)
    app.get("/index", index)
    app.get("/json", fn(req) {
        return http.json({
            code: 0,
            msg: "ok",
            data: {
                path: req.path.clone(),
                now_ms: time.millis()
            }
        })
    })
    app.get("/user/{id}", fn(req) {
        return http.json({
            code: 0,
            msg: "ok",
            data: {
                id: req.params.id.clone(),
                q: req.query.get_or("q", ""),
                method: req.method.clone()
            }
        })
    })
    app.post("/echo", fn(req) {
        return http.text(req.body.clone())
    })

    println("Ku HTTP server listening on http://127.0.0.1:8080")
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
"#
        }
        "json" => {
            r#"import { json } from "std"

fn main(): null! {
    data = {
        code: 0,
        msg: "ok",
        data: { name: "Ku" }
    }
    println(json.stringify(data)?)
    return ok(null)
}
"#
        }
        "fs" => {
            r#"import { fs } from "std"

fn main(): null! {
    fs.write("hello.txt", "Hello Ku")?
    text = fs.read("hello.txt")?
    println(text)
    return ok(null)
}
"#
        }
        "lib" => {
            r#"fn Add(a:int, b:int): int {
    return a + b
}

fn main() {
    println(Add(1, 2))
}
"#
        }
        _ => unreachable!("unknown built-in template"),
    }
}

fn validate_project_name(name: &str) -> Result<(), KuError> {
    if is_valid_project_name(name) {
        Ok(())
    } else {
        Err(project_command_error(
            format!("invalid project name '{name}'"),
            "help: use names like `hello`, `HelloWorld`, `my-api`, or `data_tool`",
        ))
    }
}

fn is_valid_project_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn package_name_from_project_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn project_command_error(message: impl Into<String>, help: impl Into<String>) -> KuError {
    KuError::message(format!("{}\n{}", message.into(), help.into()))
}

fn source_arg_or_project_with_dependency_mode(
    args: &[String],
    command: &str,
) -> Result<(PathBuf, String, DependencyResolveMode), KuError> {
    let mut path = None::<PathBuf>;
    let mut selected_dependency_mode = None;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, command)?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!("unknown {command} option '{value}'")));
            }
            value => {
                if path.is_some() {
                    return Err(command_error(format!(
                        "too many arguments for 'ku {command}'"
                    )));
                }
                path = Some(PathBuf::from(value));
            }
        }
    }

    let (path, source) = match path {
        Some(path) => {
            let source = read_ku_path(&path)?;
            (path, source)
        }
        None => project_entry_source(command)?,
    };
    Ok((
        path,
        source,
        selected_dependency_mode.unwrap_or(DependencyResolveMode::Update),
    ))
}

fn parse_native_compat_args(args: &[String]) -> Result<(PathBuf, DependencyResolveMode), KuError> {
    let mut path = None::<PathBuf>;
    let mut selected_dependency_mode = None;
    for arg in args.iter().skip(3) {
        match arg.as_str() {
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, "build --native")?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!(
                    "unknown build --native option '{value}'"
                )));
            }
            value => {
                if path.is_some() {
                    return Err(command_error(
                        "ku build --native accepts exactly one .ku file",
                    ));
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path =
        path.ok_or_else(|| command_error("missing .ku file path for 'ku build --native'"))?;
    Ok((
        path,
        selected_dependency_mode.unwrap_or(DependencyResolveMode::Update),
    ))
}

fn select_dependency_mode(
    selected: &mut Option<DependencyResolveMode>,
    flag: &str,
    command: &str,
) -> Result<(), KuError> {
    if selected.is_some() {
        return Err(command_error(format!(
            "ku {command} accepts only one of --locked or --offline"
        )));
    }
    *selected = Some(match flag {
        "--locked" => DependencyResolveMode::Locked,
        "--offline" => DependencyResolveMode::Offline,
        _ => unreachable!("dependency mode is selected only from known flags"),
    });
    Ok(())
}

fn project_entry_source(command: &str) -> Result<(PathBuf, String), KuError> {
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let package = package::discover_from_dir(&cwd)?.ok_or_else(|| {
        command_error(format!(
            "ku {command} needs a .ku file or a ku.mod package in the current directory"
        ))
    })?;
    let entry = package_entry_path(&package);
    let source = read_ku_path(&entry)?;
    Ok((entry, source))
}

fn exact_path<'a>(args: &'a [String], command: &str) -> Result<&'a str, KuError> {
    if args.len() < 3 {
        return Err(command_error(format!(
            "missing .ku file path for 'ku {command}'"
        )));
    }
    reject_extra_args(args, 3, command)?;
    Ok(args[2].as_str())
}

fn reject_extra_args(args: &[String], expected_len: usize, command: &str) -> Result<(), KuError> {
    if args.len() > expected_len {
        Err(command_error(format!(
            "too many arguments for 'ku {command}'"
        )))
    } else {
        Ok(())
    }
}

fn read_ku_file(path: &str) -> Result<String, KuError> {
    read_ku_path(Path::new(path))
}

fn read_ku_path(path: &Path) -> Result<String, KuError> {
    if !is_ku_path(path) {
        return Err(expected_ku_file(&path_string(path)));
    }
    reject_large_file(path, Span::default())?;
    fs::read_to_string(path)
        .map_err(|e| KuError::message(format!("failed to read {}: {e}", path.display())))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildProfile {
    Debug,
    Release,
    Small,
    Fast,
}

impl BuildProfile {
    fn parse(value: &str) -> Result<Self, KuError> {
        match value {
            "debug" => Ok(Self::Debug),
            "release" => Ok(Self::Release),
            "small" => Ok(Self::Small),
            "fast" => Ok(Self::Fast),
            _ => Err(command_error(format!(
                "unknown build profile '{value}'; expected debug, release, small, or fast"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
            Self::Small => "small",
            Self::Fast => "fast",
        }
    }

    fn rustc_opt_level(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("2"),
            Self::Small => Some("s"),
            Self::Fast => Some("3"),
        }
    }

    fn msvc_opt_flag(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("/O2"),
            Self::Small => Some("/O1"),
            Self::Fast => Some("/O2"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildBackend {
    Runner,
    C,
    Llvm,
}

impl BuildBackend {
    fn parse(value: &str) -> Result<Self, KuError> {
        match value {
            "runner" | "interp" | "interpreter" => Ok(Self::Runner),
            "c" | "native-c" => Ok(Self::C),
            "llvm" | "ll" => Ok(Self::Llvm),
            _ => Err(command_error(format!(
                "unknown build backend '{value}'; expected runner, c, or llvm"
            ))),
        }
    }
}

#[derive(Debug)]
struct BuildOptions {
    entry: Option<PathBuf>,
    output: Option<PathBuf>,
    profile: BuildProfile,
    target: Option<String>,
    backend: BuildBackend,
    emit_c: bool,
    emit_ir: bool,
    emit_llvm: bool,
    clean: bool,
    verbose: bool,
    lto: bool,
    strip: bool,
    static_link: bool,
    dependency_mode: DependencyResolveMode,
}

#[derive(Debug)]
struct BuildPlan {
    entry: PathBuf,
    source: String,
    out_root: PathBuf,
    build_dir: PathBuf,
    output: PathBuf,
    ir_output: PathBuf,
    native_c_output: PathBuf,
    llvm_output: PathBuf,
    root_lock_path: PathBuf,
    output_lock_path: PathBuf,
    target: Option<BuildTarget>,
}

#[derive(Clone, Copy)]
enum BuildLockMode {
    Shared,
    Exclusive,
}

struct BuildFileLock {
    file: fs::File,
}

impl Drop for BuildFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn acquire_build_file_lock_until(
    path: &Path,
    mode: BuildLockMode,
    deadline: Instant,
) -> Result<BuildFileLock, KuError> {
    let file = package::open_validated_package_operation_lock_file(path)?;
    loop {
        let result = match mode {
            BuildLockMode::Shared => file.try_lock_shared(),
            BuildLockMode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => return Ok(BuildFileLock { file }),
            Err(fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(command_error(format!(
                        "build output remained busy for {} seconds\nhelp: wait for the other build using '{}' to finish, then retry",
                        BUILD_LOCK_TIMEOUT.as_secs(),
                        path.display()
                    )));
                }
                thread::sleep(
                    BUILD_LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
                );
            }
            Err(fs::TryLockError::Error(err)) => {
                return Err(KuError::message(format!(
                    "failed to lock build output '{}': {err}",
                    path.display()
                )));
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RunnerBuildConfig<'a> {
    profile: BuildProfile,
    target: Option<&'a str>,
    lto: bool,
    strip: bool,
    verbose: bool,
    dependency_mode: DependencyResolveMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BuildTarget {
    slug: String,
    rust_triple: &'static str,
    c_triple: &'static str,
    is_windows: bool,
    binary_format: NativeBinaryFormat,
}

impl BuildTarget {
    fn matches_host(&self) -> bool {
        match self.binary_format {
            NativeBinaryFormat::ElfX86_64 => {
                cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")
            }
            NativeBinaryFormat::PeX86_64 => cfg!(windows) && cfg!(target_arch = "x86_64"),
            NativeBinaryFormat::MachOArm64 => {
                cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CCompilerCandidate {
    label: String,
    program: String,
    args: Vec<String>,
    kind: CCompilerKind,
    explicitly_configured: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CCompilerKind {
    ZigCc,
    Clang,
    Preconfigured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeBinaryFormat {
    ElfX86_64,
    PeX86_64,
    MachOArm64,
}

fn run_build_command(args: &[String]) -> Result<(), KuError> {
    let options = parse_build_options(args)?;
    let plan = resolve_build_plan(&options)?;
    let lock_deadline = Instant::now() + BUILD_LOCK_TIMEOUT;
    let _root_lock = acquire_build_file_lock_until(
        &plan.root_lock_path,
        if options.clean {
            BuildLockMode::Exclusive
        } else {
            BuildLockMode::Shared
        },
        lock_deadline,
    )?;
    if options.clean && plan.out_root.exists() {
        fs::remove_dir_all(&plan.out_root).map_err(|err| {
            KuError::message(format!(
                "failed to clean build directory '{}': {err}",
                plan.out_root.display()
            ))
        })?;
    }
    fs::create_dir_all(&plan.build_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create build directory '{}': {err}",
            plan.build_dir.display()
        ))
    })?;
    let _output_lock = acquire_build_file_lock_until(
        &plan.output_lock_path,
        BuildLockMode::Exclusive,
        lock_deadline,
    )?;

    if options.verbose {
        println!("build entry: {}", plan.entry.display());
        println!("build profile: {}", options.profile.as_str());
        println!("build directory: {}", plan.build_dir.display());
    }

    if options.emit_ir {
        let output = write_checked_ir_artifact(&plan, options.dependency_mode)?;
        println!("ir ok: {}", output.display());
    }
    if options.emit_llvm {
        let output = write_llvm_ir_artifact(&plan, options.dependency_mode)?;
        println!("llvm ir ok: {}", output.display());
    }

    match options.backend {
        BuildBackend::Runner => {
            if options.emit_c {
                let output = write_native_c_artifact(&plan, options.dependency_mode)?;
                println!("native c ok: {}", output.display());
            }
            if options.static_link && options.verbose {
                println!("note: --static is reserved for native backends; runner backend embeds Ku source in a Rust wrapper");
            }
            let entry = path_string(&plan.entry);
            let output = build_executable_to(
                &entry,
                &plan.source,
                &plan.output,
                RunnerBuildConfig {
                    profile: options.profile,
                    target: plan.target.as_ref().map(|target| target.rust_triple),
                    lto: options.lto,
                    strip: options.strip,
                    verbose: options.verbose,
                    dependency_mode: options.dependency_mode,
                },
            )?;
            println!("build ok: {}", output.display());
        }
        BuildBackend::C => {
            let c_output = write_native_c_artifact(&plan, options.dependency_mode)?;
            println!("native c ok: {}", c_output.display());
            compile_c_source(
                &c_output,
                &plan.output,
                plan.target.as_ref(),
                options.profile,
                options.static_link,
                options.verbose,
            )?;
            println!("build ok: {}", plan.output.display());
        }
        BuildBackend::Llvm => {
            let llvm_output = write_llvm_ir_artifact(&plan, options.dependency_mode)?;
            println!("llvm ir ok: {}", llvm_output.display());
            return Err(KuError::message(format!(
                "LLVM backend does not link executables yet; wrote {}\nhelp: use `ku build` for a runnable wrapper, or `ku build --emit-llvm` when you only need text IR",
                llvm_output.display()
            )));
        }
    }

    Ok(())
}

fn parse_build_options(args: &[String]) -> Result<BuildOptions, KuError> {
    let mut options = BuildOptions {
        entry: None,
        output: None,
        profile: BuildProfile::Debug,
        target: None,
        backend: BuildBackend::Runner,
        emit_c: false,
        emit_ir: false,
        emit_llvm: false,
        clean: false,
        verbose: false,
        lto: false,
        strip: false,
        static_link: false,
        dependency_mode: DependencyResolveMode::Update,
    };
    let mut selected_dependency_mode = None;

    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-o" | "--output" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing output path after -o/--output"));
                };
                options.output = Some(PathBuf::from(value));
            }
            "--profile" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing profile after --profile"));
                };
                options.profile = BuildProfile::parse(value)?;
            }
            "--release" => options.profile = BuildProfile::Release,
            "--debug" => options.profile = BuildProfile::Debug,
            "--small" => options.profile = BuildProfile::Small,
            "--fast" => options.profile = BuildProfile::Fast,
            "--target" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing target after --target"));
                };
                options.target = Some(value.clone());
            }
            "--backend" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(command_error("missing backend after --backend"));
                };
                options.backend = BuildBackend::parse(value)?;
            }
            "--emit-c" => options.emit_c = true,
            "--emit-ir" => options.emit_ir = true,
            "--emit-llvm" | "--emit-ll" => options.emit_llvm = true,
            "--clean" => options.clean = true,
            "--verbose" | "-v" => options.verbose = true,
            "--lto" => options.lto = true,
            "--strip" => options.strip = true,
            "--static" => options.static_link = true,
            "--locked" | "--offline" => {
                select_dependency_mode(&mut selected_dependency_mode, arg, "build")?;
            }
            value if value.starts_with('-') => {
                return Err(command_error(format!("unknown build option '{value}'")));
            }
            value => {
                if options.entry.is_some() {
                    return Err(command_error(
                        "ku build accepts at most one file or project path",
                    ));
                }
                options.entry = Some(PathBuf::from(value));
            }
        }
        index += 1;
    }

    options.dependency_mode = selected_dependency_mode.unwrap_or(DependencyResolveMode::Update);

    Ok(options)
}

fn resolve_build_plan(options: &BuildOptions) -> Result<BuildPlan, KuError> {
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let (entry, package, explicit_file_entry) = match &options.entry {
        Some(path) if path.is_dir() => {
            let package = package::discover_from_dir(path)?.ok_or_else(|| {
                KuError::message(format!(
                    "no ku.mod found for project '{}'\nhelp: run `ku build <file.ku>` or add ku.mod with name/root/main",
                    path.display()
                ))
            })?;
            (package_entry_path(&package), Some(package), false)
        }
        Some(path) => {
            if !is_ku_path(path) {
                return Err(KuError::message(format!(
                    "expected a .ku source file or package directory for ku build, got '{}'\nhelp: use `ku build src/main.ku` or `ku build .`",
                    path.display()
                )));
            }
            let entry = fs::canonicalize(path).map_err(|err| {
                KuError::message(format!(
                    "failed to resolve build entry '{}': {err}",
                    path.display()
                ))
            })?;
            let package = package::discover_for_file(&entry)?;
            (entry, package, true)
        }
        None => {
            let package = package::discover_from_dir(&cwd)?.ok_or_else(|| {
                KuError::message(
                    "ku build needs a .ku file or a ku.mod package in the current directory\nhelp: use `ku build <file.ku>`, or create ku.mod with name/root/main",
                )
            })?;
            (package_entry_path(&package), Some(package), false)
        }
    };

    if !entry.exists() {
        return Err(KuError::message(format!(
            "build entry '{}' does not exist\nhelp: check ku.mod main/root, or pass an explicit .ku file",
            entry.display()
        )));
    }
    if !is_ku_path(&entry) {
        return Err(KuError::message(format!(
            "build entry '{}' is not a .ku source file\nhelp: set ku.mod main to a .ku file",
            entry.display()
        )));
    }
    reject_large_file(&entry, Span::default())?;
    let source = fs::read_to_string(&entry).map_err(|err| {
        KuError::message(format!(
            "failed to read build entry '{}': {err}",
            entry.display()
        ))
    })?;

    if explicit_file_entry && options.output.is_none() {
        if let Some(package) = package.as_ref() {
            let package_entry = package_entry_path(package);
            let package_entry = fs::canonicalize(&package_entry).unwrap_or(package_entry);
            if entry != package_entry {
                return Err(command_error(format!(
                    "building a non-main package entry requires an explicit output path\nhelp: use `ku build -o <output> {}`; use `ku build {}` for the package main entry",
                    entry.display(),
                    package.package_dir.display()
                )));
            }
        }
    }

    let package_name = package
        .as_ref()
        .map(|package| package.manifest.name.clone())
        .unwrap_or_else(|| {
            entry
                .file_stem()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("ku_app")
                .to_string()
        });
    let out_root = package
        .as_ref()
        .map(|package| {
            package.package_dir.join(
                package
                    .manifest
                    .out
                    .as_deref()
                    .unwrap_or(package::DEFAULT_BUILD_DIR),
            )
        })
        .unwrap_or_else(|| {
            entry
                .parent()
                .map(|parent| parent.join(package::DEFAULT_BUILD_DIR))
                .unwrap_or_else(|| cwd.join(package::DEFAULT_BUILD_DIR))
        });
    let target = resolve_build_target(options.target.as_deref())?;
    let build_dir = build_profile_dir(&out_root, options.profile, target.as_ref());
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| build_dir.join(&package_name));
    let output = with_executable_extension(output, target.as_ref());
    let output_digest = native_output_path_digest(&output, &cwd);
    let explicit_output = options.output.is_some();
    let ir_output = build_intermediate_artifact_path(
        &build_dir,
        "ir",
        &output,
        "ir",
        explicit_output,
        &output_digest,
    );
    let native_c_output = build_intermediate_artifact_path(
        &build_dir,
        "c",
        &output,
        "c",
        explicit_output,
        &output_digest,
    );
    let llvm_output = build_intermediate_artifact_path(
        &build_dir,
        "llvm",
        &output,
        "ll",
        explicit_output,
        &output_digest,
    );
    // Keep locks outside every build tree: `--clean` must never unlink a lock
    // that another process still relies on. A process-wide temporary root also
    // makes two projects targeting the same absolute output coordinate on the
    // same lock file.
    let lock_dir = env::temp_dir().join("ku-build-locks-v1");
    let root_lock_path = lock_dir.join(format!(
        "root-{}.lock",
        native_output_path_digest(&out_root, &cwd)
    ));
    let output_lock_path = lock_dir.join(format!("output-{output_digest}.lock"));

    Ok(BuildPlan {
        entry,
        source,
        out_root,
        build_dir,
        output,
        ir_output,
        native_c_output,
        llvm_output,
        root_lock_path,
        output_lock_path,
        target,
    })
}

fn package_entry_path(package: &PackageContext) -> PathBuf {
    let mut entry = package.import_root.join(
        package
            .manifest
            .main
            .as_deref()
            .unwrap_or(package::DEFAULT_MAIN_FILE),
    );
    if entry.extension().is_none() {
        entry.set_extension("ku");
    }
    entry
}

fn build_profile_dir(
    out_root: &Path,
    profile: BuildProfile,
    target: Option<&BuildTarget>,
) -> PathBuf {
    if let Some(target) = target {
        out_root.join(&target.slug).join(profile.as_str())
    } else {
        out_root.join(profile.as_str())
    }
}

fn with_executable_extension(mut path: PathBuf, target: Option<&BuildTarget>) -> PathBuf {
    let needs_exe = target
        .map(|target| target.is_windows)
        .unwrap_or_else(|| cfg!(windows));
    if needs_exe && path.extension().is_none() {
        path.set_extension("exe");
    }
    path
}

fn resolve_build_target(target: Option<&str>) -> Result<Option<BuildTarget>, KuError> {
    let Some(raw) = target else {
        return Ok(None);
    };
    let value = raw.trim();
    if value == "host" {
        return Ok(None);
    }
    if value.is_empty()
        || value.contains(['/', '\\', ':'])
        || value.split('-').any(|part| part == "." || part == "..")
    {
        return Err(command_error(format!(
            "invalid build target '{raw}'\nhelp: use host, x86_64-linux, x86_64-windows, or aarch64-darwin"
        )));
    }
    let target = match value {
        "x86_64-linux" => BuildTarget {
            slug: value.to_string(),
            rust_triple: "x86_64-unknown-linux-gnu",
            c_triple: "x86_64-linux-gnu",
            is_windows: false,
            binary_format: NativeBinaryFormat::ElfX86_64,
        },
        "x86_64-windows" => BuildTarget {
            slug: value.to_string(),
            rust_triple: "x86_64-pc-windows-msvc",
            c_triple: "x86_64-windows-gnu",
            is_windows: true,
            binary_format: NativeBinaryFormat::PeX86_64,
        },
        "aarch64-darwin" => BuildTarget {
            slug: value.to_string(),
            rust_triple: "aarch64-apple-darwin",
            c_triple: "aarch64-macos",
            is_windows: false,
            binary_format: NativeBinaryFormat::MachOArm64,
        },
        _ => {
            return Err(command_error(format!(
                "unsupported build target '{raw}'\nhelp: this stage supports host, x86_64-linux, x86_64-windows, and aarch64-darwin"
            )))
        }
    };
    Ok(Some(target))
}

fn is_ku_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write_checked_ir_artifact(
    plan: &BuildPlan,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let entry = path_string(&plan.entry);
    let program = parse_and_check_with_dependency_mode(&entry, &plan.source, dependency_mode)?;
    let output = plan.ir_output.clone();
    let lowered = ir::lower_program(&program)?;
    write_text_artifact(&output, format!("{}", ir::optimize_program(&lowered)))
}

fn write_native_c_artifact(
    plan: &BuildPlan,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let output = plan.native_c_output.clone();
    let fs_base = native_fs_base_for_output(&plan.entry, &plan.output)?;
    write_native_c_to(
        &path_string(&plan.entry),
        &plan.source,
        &output,
        fs_base,
        dependency_mode,
    )
}

fn intermediate_artifact_filename(output: &Path, extension: &str) -> OsString {
    let mut binary_name = output
        .file_name()
        .filter(|name| !name.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("main"));
    if binary_name
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        binary_name.set_extension("");
    }
    let mut filename = binary_name.into_os_string();
    filename.push(".");
    filename.push(extension);
    filename
}

fn build_intermediate_artifact_path(
    build_dir: &Path,
    kind: &str,
    output: &Path,
    extension: &str,
    explicit_output: bool,
    output_digest: &str,
) -> PathBuf {
    let root = build_dir.join(kind);
    let root = if explicit_output {
        root.join(output_digest)
    } else {
        root
    };
    root.join(intermediate_artifact_filename(output, extension))
}

fn native_output_path_digest(output: &Path, cwd: &Path) -> String {
    let absolute = if output.is_absolute() {
        output.to_path_buf()
    } else {
        cwd.join(output)
    };
    let absolute = stable_path_identity(&absolute);
    let mut hasher = Sha256::new();
    hasher.update(b"ku-native-output-path-v1\0");

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(absolute.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in absolute.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(absolute.to_string_lossy().as_bytes());

    let digest = hasher.finalize();
    encode_base64url_no_pad(&digest)
}

fn stable_path_identity(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    let mut missing = Vec::<OsString>::new();
    let mut existing = path;
    while !existing.as_os_str().is_empty() {
        if let Ok(mut canonical) = fs::canonicalize(existing) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }

    // Absolute paths always have an existing filesystem root in normal use.
    // Keep the original spelling only for unusual virtual paths where even the
    // root cannot be canonicalized; hashing still remains deterministic.
    path.to_path_buf()
}

fn encode_base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        match chunk {
            [_, second, third] => {
                encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
                encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            }
            [_, second] => {
                encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
                encoded.push(ALPHABET[((second & 0x0f) << 2) as usize] as char);
            }
            [_] => {
                encoded.push(ALPHABET[((first & 0x03) << 4) as usize] as char);
            }
            [] => unreachable!("chunks never yields an empty slice"),
            _ => unreachable!("chunks are bounded to three bytes"),
        }
    }
    encoded
}

fn native_fs_base_for_output(
    entry: &Path,
    executable: &Path,
) -> Result<backend::c::NativeFsBase, KuError> {
    let executable_dir = executable
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(executable_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create output directory '{}': {err}",
            executable_dir.display()
        ))
    })?;

    let result = (|| {
        let source_dir = entry
            .parent()
            .ok_or_else(|| "build entry has no source directory".to_string())?;
        let source_dir = fs::canonicalize(source_dir)
            .map_err(|err| format!("failed to resolve source directory: {err}"))?;
        let executable_dir = fs::canonicalize(executable_dir)
            .map_err(|err| format!("failed to resolve output directory: {err}"))?;
        executable_relative_locator(&executable_dir, &source_dir)
    })();

    Ok(match result {
        Ok(locator) => backend::c::NativeFsBase::ExecutableRelative(locator),
        Err(reason) => backend::c::NativeFsBase::Unavailable(reason),
    })
}

/// Produce a slash-separated locator from the executable directory to the
/// source directory without retaining either absolute build-machine path.
fn executable_relative_locator(executable_dir: &Path, source_dir: &Path) -> Result<String, String> {
    let mut common = executable_dir;
    let mut parent_count = 0usize;
    loop {
        if let Ok(suffix) = source_dir.strip_prefix(common) {
            let mut parts = vec!["..".to_string(); parent_count];
            for component in suffix.components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(value) => parts.push(
                        value
                            .to_str()
                            .ok_or_else(|| {
                                "source/output relative locator is not valid UTF-8".to_string()
                            })?
                            .to_string(),
                    ),
                    Component::ParentDir => parts.push("..".to_string()),
                    Component::Prefix(_) | Component::RootDir => {
                        return Err("source/output paths do not share a filesystem root".to_string())
                    }
                }
            }
            let locator = if parts.is_empty() {
                ".".to_string()
            } else {
                parts.join("/")
            };
            if locator.len() > 32 * 1024 {
                return Err("source/output relative locator exceeds 32 KiB".to_string());
            }
            return Ok(locator);
        }

        let Some(parent) = common.parent() else {
            return Err("source/output paths do not share a filesystem root".to_string());
        };
        if parent == common || parent_count >= 32 * 1024 {
            return Err("source/output relative locator cannot make bounded progress".to_string());
        }
        common = parent;
        parent_count += 1;
    }
}

fn write_llvm_ir_artifact(
    plan: &BuildPlan,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let output = plan.llvm_output.clone();
    write_llvm_ir_to(
        &path_string(&plan.entry),
        &plan.source,
        &output,
        dependency_mode,
    )
}

fn write_text_artifact(output: &Path, text: String) -> Result<PathBuf, KuError> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            KuError::message(format!(
                "failed to create artifact directory '{}': {err}",
                parent.display()
            ))
        })?;
    }
    fs::write(output, text).map_err(|err| {
        KuError::message(format!(
            "failed to write artifact '{}': {err}",
            output.display()
        ))
    })?;
    Ok(output.to_path_buf())
}

fn build_executable_to(
    path: &str,
    source: &str,
    output: &Path,
    config: RunnerBuildConfig<'_>,
) -> Result<PathBuf, KuError> {
    check_source_with_dependency_mode(path, source, config.dependency_mode)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            KuError::message(format!(
                "failed to create output directory '{}': {err}",
                parent.display()
            ))
        })?;
    }
    let embedded_path = fs::canonicalize(path)
        .unwrap_or_else(|_| Path::new(path).to_path_buf())
        .to_string_lossy()
        .to_string();
    let rust_source = build_runner_source(&embedded_path, source, config.dependency_mode);
    let temp_dir = env::temp_dir().join(format!(
        "ku-build-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| KuError::message(format!("failed to create build timestamp: {err}")))?
            .as_nanos()
    ));
    let temp_guard = TempBuildDir::new(temp_dir.clone());
    fs::create_dir_all(&temp_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create build directory {}: {err}",
            temp_dir.display()
        ))
    })?;
    let runner = temp_dir.join("runner.rs");
    fs::write(&runner, rust_source).map_err(|err| {
        KuError::message(format!(
            "failed to write build runner {}: {err}",
            runner.display()
        ))
    })?;

    let exe_dir = env::current_exe()
        .map_err(|err| KuError::message(format!("failed to locate ku executable: {err}")))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| KuError::message("failed to locate ku executable directory"))?;
    let target_dir = if exe_dir.file_name().is_some_and(|name| name == "deps") {
        exe_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| KuError::message("failed to locate ku target directory"))?
    } else {
        exe_dir
    };
    let lib = find_ku_rlib(&target_dir)?;
    let dependency_dirs = find_dependency_dirs(&target_dir);
    let mut command = Command::new("rustc");
    command
        .arg("--edition=2021")
        .arg(&runner)
        .arg("--extern")
        .arg(format!("ku={}", lib.display()))
        .arg("-o")
        .arg(output);
    for deps in &dependency_dirs {
        command
            .arg("-L")
            .arg(format!("dependency={}", deps.display()));
    }
    if let Some(target) = config.target {
        command.arg("--target").arg(target);
    }
    if let Some(opt_level) = config.profile.rustc_opt_level() {
        command.arg("-C").arg(format!("opt-level={opt_level}"));
        command.arg("-C").arg("debuginfo=0");
    }
    if config.lto {
        command.arg("-C").arg("lto=fat");
    }
    if config.strip {
        command.arg("-C").arg("strip=symbols");
    }
    if config.verbose {
        println!("rustc command: {command:?}");
    }
    let status = command
        .status()
        .map_err(|err| KuError::message(format!("failed to run rustc for ku build: {err}")))?;
    temp_guard.cleanup();
    if !status.success() {
        return Err(KuError::message(format!(
            "ku build failed: rustc exited with {status}\nhelp: make sure Rust is installed and libku.rlib plus its dependency directory match the selected target"
        )));
    }
    Ok(output.to_path_buf())
}

fn build_native_c_with_dependency_mode(
    path: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let output = Path::new(path).with_extension("c");
    write_native_c_to(
        path,
        source,
        &output,
        backend::c::NativeFsBase::ExecutableRelative(".".to_string()),
        dependency_mode,
    )
}

fn write_native_c_to(
    path: &str,
    source: &str,
    output: &Path,
    fs_base: backend::c::NativeFsBase,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let program = parse_and_expand_with_dependency_mode(path, source, dependency_mode)?;
    reject_native_async(&program)?;
    Checker::new().check(&program)?;
    let lowered = ir::lower_program(&program)?;
    let optimized = ir::optimize_program(&lowered);
    let c_source = backend::c::generate_c_source_with_options(
        &optimized,
        &backend::c::CBackendOptions {
            fs_base,
            // Test-only, generation-time opt-in. This environment is read by
            // the isolated `ku build` child used by native OOM tests; the
            // backend API itself remains deterministic and defaults to false.
            object_oom_fault_injection: env::var("KU_NATIVE_TEST_OBJECT_OOM_ENABLE").as_deref()
                == Ok("1"),
        },
    )?;
    write_text_artifact(output, c_source)
}

fn build_llvm_ir(path: &str, source: &str) -> Result<PathBuf, KuError> {
    let output = Path::new(path).with_extension("ll");
    write_llvm_ir_to(path, source, &output, DependencyResolveMode::Update)
}

fn write_llvm_ir_to(
    path: &str,
    source: &str,
    output: &Path,
    dependency_mode: DependencyResolveMode,
) -> Result<PathBuf, KuError> {
    let program = parse_and_expand_with_dependency_mode(path, source, dependency_mode)?;
    reject_compiled_async(
        &program,
        "LLVM text prototype does not support async/await yet; use the interpreter runtime",
    )?;
    Checker::new().check(&program)?;
    let lowered = ir::lower_program(&program)?;
    let optimized = ir::optimize_program(&lowered);
    let llvm_ir = backend::llvm::generate_llvm_ir(&optimized)?;
    write_text_artifact(output, llvm_ir)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CSourceFeatures {
    winsock: bool,
    pthreads: bool,
    libpq: bool,
    libmysql: bool,
}

impl CSourceFeatures {
    fn inspect(source: &Path) -> Result<Self, KuError> {
        let text = fs::read_to_string(source).map_err(|err| {
            KuError::message(format!(
                "failed to inspect generated native C '{}': {err}",
                source.display()
            ))
        })?;
        Ok(Self {
            winsock: text.contains("#include <winsock2.h>"),
            pthreads: text.contains("#include <pthread.h>"),
            libpq: text.contains("#pragma comment(lib, \"libpq.lib\")"),
            libmysql: text.contains("#pragma comment(lib, \"libmysql.lib\")"),
        })
    }
}

fn validate_c_target_features(
    features: CSourceFeatures,
    target: Option<&BuildTarget>,
) -> Result<(), KuError> {
    let Some(target) = target else {
        return Ok(());
    };
    if features.libmysql && !target.matches_host() {
        return Err(KuError::message(format!(
            "native target '{}' cannot automatically link std.mysql: Ku has no portable target-library contract for libmysqlclient yet\nhelp: use a host build with KU_MYSQL_LIB, or link the target-specific emitted C yourself with the matching client library",
            target.slug
        )));
    }
    Ok(())
}

fn libmysql_library_in(dir: &Path) -> Option<PathBuf> {
    [
        "libmysql.lib",
        "libmysqlclient.so",
        "libmysqlclient.dylib",
        "libmysqlclient.a",
        "libmariadb.so",
        "libmariadb.dylib",
        "libmariadb.a",
    ]
    .into_iter()
    .map(|name| dir.join(name))
    .find(|path| path.is_file())
}

/// Locate a host libmysqlclient/MariaDB client library. `KU_MYSQL_LIB` is the
/// explicit portable contract; Windows also discovers conventional installs.
fn detect_libmysql_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("KU_MYSQL_LIB") {
        let dir = PathBuf::from(dir);
        if libmysql_library_in(&dir).is_some() {
            return Some(dir);
        }
    }
    for base in [r"C:\Program Files\MySQL", r"D:\Program Files\MySQL"] {
        if let Ok(entries) = fs::read_dir(base) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("lib"))
                .filter(|lib| lib.join("libmysql.lib").exists())
                .collect();
            sort_install_dirs_by_version(&mut dirs);
            if let Some(dir) = dirs.pop() {
                return Some(dir);
            }
        }
    }
    None
}

fn mysql_header_in(dir: &Path) -> bool {
    dir.join("mysql.h").is_file()
        || dir.join("mysql").join("mysql.h").is_file()
        || dir.join("mariadb").join("mysql.h").is_file()
}

/// MYSQL_BIND is a versioned public struct and must come from the matching
/// development header. Never synthesize its layout in generated C.
fn detect_libmysql_include_dir(library_dir: Option<&Path>) -> Option<PathBuf> {
    if let Ok(dir) = env::var("KU_MYSQL_INCLUDE") {
        let dir = PathBuf::from(dir);
        if mysql_header_in(&dir) {
            return Some(dir);
        }
    }
    if let Some(root) = library_dir.and_then(Path::parent) {
        for candidate in [root.join("include"), root.join("include").join("mysql")] {
            if mysql_header_in(&candidate) {
                return Some(candidate);
            }
        }
    }
    [
        PathBuf::from("/usr/include/mysql"),
        PathBuf::from("/usr/include/mariadb"),
        PathBuf::from("/usr/local/include/mysql"),
        PathBuf::from("/usr/local/include/mariadb"),
    ]
    .into_iter()
    .find(|candidate| mysql_header_in(candidate))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpqLibraryPlatform {
    Windows,
    Linux,
    Darwin,
}

impl LibpqLibraryPlatform {
    fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Darwin
        } else {
            Self::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpqArchitecture {
    X86_64,
    Aarch64,
    Other,
}

impl LibpqArchitecture {
    fn host() -> Self {
        match env::consts::ARCH {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            _ => Self::Other,
        }
    }

    fn from_rust_triple(triple: &str) -> Self {
        if triple.starts_with("x86_64-") {
            Self::X86_64
        } else if triple.starts_with("aarch64-") {
            Self::Aarch64
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpqLibraryFormat {
    WindowsMsvc,
    WindowsMingw,
    Linux,
    Darwin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LibpqLinkTarget {
    platform: LibpqLibraryPlatform,
    architecture: LibpqArchitecture,
    allow_host_discovery: bool,
}

fn libpq_link_target(target: Option<&BuildTarget>) -> LibpqLinkTarget {
    let host_platform = LibpqLibraryPlatform::host();
    let host_architecture = LibpqArchitecture::host();
    let platform = match target {
        None => host_platform,
        Some(target) if target.is_windows => LibpqLibraryPlatform::Windows,
        Some(target) if target.rust_triple.ends_with("-apple-darwin") => {
            LibpqLibraryPlatform::Darwin
        }
        Some(_) => LibpqLibraryPlatform::Linux,
    };
    let architecture = target
        .map(|target| LibpqArchitecture::from_rust_triple(target.rust_triple))
        .unwrap_or(host_architecture);
    LibpqLinkTarget {
        platform,
        architecture,
        // pg_config and conventional installation directories describe both the
        // host OS and host architecture. Any mismatch must use an explicitly
        // supplied KU_PG_LIB directory or let the target compiler resolve `-lpq`
        // from its sysroot.
        allow_host_discovery: target.is_none()
            || (platform == host_platform && architecture == host_architecture),
    }
}

fn compiler_uses_windows_mingw_abi(candidate: &CCompilerCandidate) -> bool {
    if candidate.kind == CCompilerKind::ZigCc {
        return true;
    }
    let program_name = Path::new(&candidate.program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&candidate.program)
        .to_ascii_lowercase();
    let configured_command = std::iter::once(candidate.program.as_str())
        .chain(candidate.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    program_name == "cc"
        || program_name == "cc.exe"
        || program_name.contains("gcc")
        || configured_command.contains("mingw")
        || configured_command.contains("windows-gnu")
}

fn libpq_library_format(
    platform: LibpqLibraryPlatform,
    candidate: &CCompilerCandidate,
) -> LibpqLibraryFormat {
    match platform {
        LibpqLibraryPlatform::Windows if compiler_uses_windows_mingw_abi(candidate) => {
            LibpqLibraryFormat::WindowsMingw
        }
        LibpqLibraryPlatform::Windows => LibpqLibraryFormat::WindowsMsvc,
        LibpqLibraryPlatform::Linux => LibpqLibraryFormat::Linux,
        LibpqLibraryPlatform::Darwin => LibpqLibraryFormat::Darwin,
    }
}

fn numeric_library_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Rank linkable libpq filenames for deterministic directory discovery. Unix
/// installations may expose only the runtime SONAME (for example `libpq.so.5`),
/// while development packages normally add the unversioned `libpq.so` symlink.
fn libpq_library_name_priority(name: &str, format: LibpqLibraryFormat) -> Option<usize> {
    match format {
        LibpqLibraryFormat::WindowsMsvc => name.eq_ignore_ascii_case("libpq.lib").then_some(0),
        LibpqLibraryFormat::WindowsMingw => {
            if name.eq_ignore_ascii_case("libpq.dll.a") {
                Some(0)
            } else if name.eq_ignore_ascii_case("libpq.lib") {
                // COFF import libraries are accepted by lld and modern MinGW
                // linkers. Prefer the GNU-named archive when both are present.
                Some(1)
            } else {
                None
            }
        }
        LibpqLibraryFormat::Linux => match name {
            "libpq.so" => Some(0),
            _ if name
                .strip_prefix("libpq.so.")
                .is_some_and(numeric_library_version) =>
            {
                Some(1)
            }
            _ => None,
        },
        LibpqLibraryFormat::Darwin => match name {
            "libpq.dylib" => Some(0),
            _ if name
                .strip_prefix("libpq.")
                .and_then(|value| value.strip_suffix(".dylib"))
                .is_some_and(numeric_library_version) =>
            {
                Some(1)
            }
            _ => None,
        },
    }
}

/// Return an existing shared/import library (or a symlink to one), never merely
/// a caller-provided directory. Passing the selected file directly to Unix
/// linkers also supports runtime-only directories that contain `libpq.so.N`
/// without an unversioned `libpq.so` linker name. Static archives are
/// deliberately excluded because their transitive dependency closure is not
/// portable across libpq builds.
fn find_libpq_library(dir: &Path, format: LibpqLibraryFormat) -> Option<PathBuf> {
    let mut candidates = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let name = entry.file_name();
            let name = name.to_str()?;
            let priority = libpq_library_name_priority(name, format)?;
            Some((priority, name.len(), name.to_string(), path))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| (left.0, left.1, &left.2).cmp(&(right.0, right.1, &right.2)));
    candidates.into_iter().next().map(|(_, _, _, path)| path)
}

fn libpq_dir_has_supported_library(
    dir: &Path,
    format: LibpqLibraryFormat,
) -> Result<bool, KuError> {
    if find_libpq_library(dir, format).is_some() {
        return Ok(true);
    }
    let static_archive = dir.join("libpq.a");
    if static_archive.is_file() {
        return Err(KuError::message(format!(
            "cannot automatically link static libpq archive '{}': its target-specific transitive libraries cannot be inferred portably\nhelp: install a target-compatible shared libpq in KU_PG_LIB, or link the emitted C yourself with the complete dependency list reported by your libpq installation",
            static_archive.display()
        )));
    }
    Ok(false)
}

/// Locate a directory containing a libpq library for `format`. `KU_PG_LIB` is
/// always considered; host-derived `pg_config` and conventional install paths
/// are considered only when `allow_host_discovery` is true. Missing paths and
/// directories without an actual target-format library are ignored.
fn detect_libpq_dir(
    format: LibpqLibraryFormat,
    allow_host_discovery: bool,
) -> Result<Option<PathBuf>, KuError> {
    if let Ok(dir) = env::var("KU_PG_LIB") {
        let dir = PathBuf::from(dir);
        if libpq_dir_has_supported_library(&dir, format)? {
            return Ok(Some(dir));
        }
    }
    if !allow_host_discovery {
        return Ok(None);
    }
    if let Ok(output) = Command::new("pg_config").arg("--libdir").output() {
        if output.status.success() {
            let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !dir.is_empty() {
                let dir = PathBuf::from(dir);
                if libpq_dir_has_supported_library(&dir, format)? {
                    return Ok(Some(dir));
                }
            }
        }
    }
    for candidate in [
        r"C:\Program Files\PostgreSQL",
        r"D:\Program Files\PostgreSQL",
    ] {
        if let Ok(entries) = fs::read_dir(candidate) {
            // Prefer the highest version directory that actually has libpq.lib.
            let mut dirs = Vec::new();
            for entry in entries.flatten() {
                let lib = entry.path().join("lib");
                if libpq_dir_has_supported_library(&lib, format)? {
                    dirs.push(lib);
                }
            }
            sort_install_dirs_by_version(&mut dirs);
            if let Some(dir) = dirs.pop() {
                return Ok(Some(dir));
            }
        }
    }
    Ok(None)
}

/// Sort installation library directories by the numeric components in their
/// parent directory name. Plain lexical sorting makes PostgreSQL 9.6 appear
/// newer than PostgreSQL 17 and can link an ABI-incompatible import library.
fn sort_install_dirs_by_version(dirs: &mut [PathBuf]) {
    dirs.sort_by(|left, right| {
        let key = |path: &Path| {
            path.parent()
                .and_then(Path::file_name)
                .map(|name| numeric_version_key(&name.to_string_lossy()))
                .unwrap_or_default()
        };
        key(left).cmp(&key(right)).then_with(|| left.cmp(right))
    });
}

fn numeric_version_key(name: &str) -> Vec<u64> {
    name.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn validate_libpq_link_mode(needs_libpq: bool, static_link: bool) -> Result<(), KuError> {
    if needs_libpq && static_link {
        return Err(KuError::message(
            "native C build cannot safely link std.pg with --static: libpq static archives require target-specific transitive libraries that Ku cannot infer portably\nhelp: omit --static and provide a target-compatible shared libpq through KU_PG_LIB, or link the emitted C yourself with the complete dependency list reported by your libpq installation",
        ));
    }
    Ok(())
}

static NEXT_LINK_OUTPUT: AtomicU64 = AtomicU64::new(0);
const LINK_STAGING_PREFIX: &str = ".ku-link-";
const LINK_STAGING_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_LINK_STAGING_SCAN_ENTRIES: usize = 256;
const MAX_LINK_STAGING_DELETE_FILES: usize = 16;

fn temporary_link_output(output: &Path) -> PathBuf {
    let sequence = NEXT_LINK_OUTPUT.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut name = format!(
        "{LINK_STAGING_PREFIX}{}-{sequence}-{timestamp}",
        std::process::id()
    );
    if let Some(extension) = output.extension().and_then(|value| value.to_str()) {
        name.push('.');
        name.push_str(extension);
    }
    output.with_file_name(name)
}

fn is_native_link_staging_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(LINK_STAGING_PREFIX) else {
        return false;
    };
    let mut components = rest.splitn(3, '-');
    let Some(pid) = components.next() else {
        return false;
    };
    let Some(sequence) = components.next() else {
        return false;
    };
    let Some(timestamp_and_extension) = components.next() else {
        return false;
    };
    if pid.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || sequence.is_empty()
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let mut suffix = timestamp_and_extension.splitn(2, '.');
    let timestamp = suffix.next().unwrap_or_default();
    let extension_is_valid = suffix.next().is_none_or(|extension| !extension.is_empty());
    !timestamp.is_empty()
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && extension_is_valid
}

fn cleanup_stale_link_outputs_with_policy(
    directory: &Path,
    now: SystemTime,
    stale_after: Duration,
    max_scan: usize,
    max_delete: usize,
) -> Result<usize, KuError> {
    if max_scan == 0 || max_delete == 0 {
        return Ok(0);
    }
    let entries = fs::read_dir(directory).map_err(|err| {
        KuError::message(format!(
            "failed to scan native output directory '{}': {err}",
            directory.display()
        ))
    })?;
    let mut deleted = 0usize;
    for entry in entries.take(max_scan) {
        let entry = entry.map_err(|err| {
            KuError::message(format!(
                "failed to inspect native output directory '{}': {err}",
                directory.display()
            ))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_native_link_staging_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            KuError::message(format!(
                "failed to inspect native link staging '{}': {err}",
                path.display()
            ))
        })?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < stale_after {
            continue;
        }
        fs::remove_file(&path).map_err(|err| {
            KuError::message(format!(
                "failed to remove stale native link staging '{}': {err}",
                path.display()
            ))
        })?;
        deleted += 1;
        if deleted == max_delete {
            break;
        }
    }
    Ok(deleted)
}

fn cleanup_stale_link_outputs(directory: &Path) -> Result<usize, KuError> {
    cleanup_stale_link_outputs_with_policy(
        directory,
        SystemTime::now(),
        LINK_STAGING_STALE_AFTER,
        MAX_LINK_STAGING_SCAN_ENTRIES,
        MAX_LINK_STAGING_DELETE_FILES,
    )
}

fn prepare_link_output_staging(path: &Path) -> Result<(), KuError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(path).map_err(|err| {
            KuError::message(format!(
                "failed to remove stale native link staging '{}': {err}",
                path.display()
            ))
        }),
        Ok(_) => Err(KuError::message(format!(
            "native link staging path '{}' is not a regular file",
            path.display()
        ))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(KuError::message(format!(
            "failed to inspect native link staging '{}': {err}",
            path.display()
        ))),
    }
}

fn install_link_output(temporary: &Path, output: &Path) -> Result<(), KuError> {
    match fs::rename(temporary, output) {
        Ok(()) => Ok(()),
        Err(_first_error) if cfg!(windows) && output.is_file() => {
            fs::remove_file(output).map_err(|err| {
                KuError::message(format!(
                    "failed to replace previous native output '{}': {err}",
                    output.display()
                ))
            })?;
            fs::rename(temporary, output).map_err(|err| {
                KuError::message(format!(
                    "failed to install verified native output '{}': {err}",
                    output.display()
                ))
            })
        }
        Err(err) => Err(KuError::message(format!(
            "failed to install verified native output '{}': {err}",
            output.display()
        ))),
    }
}

fn binary_range_end(offset: u64, size: u64, file_len: u64, what: &str) -> Result<u64, String> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| format!("{what} range overflows"))?;
    if end > file_len {
        return Err(format!("{what} extends past the linked output"));
    }
    Ok(end)
}

fn read_binary_at(
    file: &mut fs::File,
    offset: u64,
    buffer: &mut [u8],
    what: &str,
) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|err| format!("failed to seek to {what}: {err}"))?;
    file.read_exact(buffer)
        .map_err(|err| format!("{what} is truncated: {err}"))
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn verify_native_binary_target(output: &Path, target: &BuildTarget) -> Result<(), String> {
    let mut file =
        fs::File::open(output).map_err(|err| format!("failed to open linked output: {err}"))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("failed to inspect linked output: {err}"))?;
    if !metadata.is_file() {
        return Err("linked output is not a regular file".to_string());
    }

    let file_len = metadata.len();
    match target.binary_format {
        NativeBinaryFormat::ElfX86_64 => verify_elf_x86_64(&mut file, file_len)?,
        NativeBinaryFormat::PeX86_64 => verify_pe_x86_64(&mut file, file_len)?,
        NativeBinaryFormat::MachOArm64 => verify_macho_arm64_macos(&mut file, file_len)?,
    }
    Ok(())
}

fn verify_elf_x86_64(file: &mut fs::File, file_len: u64) -> Result<(), String> {
    const ELF_HEADER_SIZE: usize = 64;
    const ELF_PROGRAM_HEADER_SIZE: u64 = 56;
    const MAX_PROGRAM_HEADERS: u16 = 4_096;
    let mut header = [0u8; ELF_HEADER_SIZE];
    read_binary_at(file, 0, &mut header, "ELF64 header")?;
    let os_abi = header[7];
    let file_type = le_u16(&header, 16);
    let machine = le_u16(&header, 18);
    let version = le_u32(&header, 20);
    let entry = le_u64(&header, 24);
    let program_offset = le_u64(&header, 32);
    let header_size = le_u16(&header, 52);
    let program_entry_size = le_u16(&header, 54);
    let program_count = le_u16(&header, 56);
    if header[..4] != *b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || !matches!(os_abi, 0 | 3)
        || header[8] != 0
        || !matches!(file_type, 2 | 3)
        || machine != 62
        || version != 1
        || entry == 0
        || header_size as usize != ELF_HEADER_SIZE
        || program_entry_size as u64 != ELF_PROGRAM_HEADER_SIZE
        || program_count == 0
        || program_count > MAX_PROGRAM_HEADERS
        || program_offset < ELF_HEADER_SIZE as u64
    {
        return Err(
            "expected a Linux-compatible little-endian x86_64 ELF64 executable".to_string(),
        );
    }
    let table_size = ELF_PROGRAM_HEADER_SIZE
        .checked_mul(program_count as u64)
        .ok_or_else(|| "ELF program table size overflows".to_string())?;
    binary_range_end(program_offset, table_size, file_len, "ELF program table")?;
    let mut has_load = false;
    for index in 0..program_count as u64 {
        let offset = program_offset + index * ELF_PROGRAM_HEADER_SIZE;
        let mut program = [0u8; ELF_PROGRAM_HEADER_SIZE as usize];
        read_binary_at(file, offset, &mut program, "ELF program header")?;
        if le_u32(&program, 0) != 1 {
            continue;
        }
        let segment_offset = le_u64(&program, 8);
        let file_size = le_u64(&program, 32);
        let memory_size = le_u64(&program, 40);
        if memory_size < file_size {
            return Err("ELF PT_LOAD memory size is smaller than its file size".to_string());
        }
        binary_range_end(segment_offset, file_size, file_len, "ELF PT_LOAD segment")?;
        has_load = true;
    }
    if !has_load {
        return Err("ELF executable has no PT_LOAD segment".to_string());
    }
    Ok(())
}

fn verify_pe_x86_64(file: &mut fs::File, file_len: u64) -> Result<(), String> {
    const COFF_HEADER_SIZE: u64 = 24;
    const MIN_PE32_PLUS_SIZE: usize = 112;
    const MAX_OPTIONAL_HEADER_SIZE: u16 = 4_096;
    const SECTION_HEADER_SIZE: u64 = 40;
    const MAX_SECTIONS: u16 = 1_024;
    let mut dos = [0u8; 64];
    read_binary_at(file, 0, &mut dos, "PE DOS header")?;
    if dos[..2] != *b"MZ" {
        return Err("expected an x86_64 PE executable (missing MZ header)".to_string());
    }
    let pe_offset = le_u32(&dos, 60) as u64;
    if pe_offset < dos.len() as u64 || pe_offset > 16 * 1024 * 1024 {
        return Err("PE header offset is outside the supported range".to_string());
    }
    binary_range_end(pe_offset, COFF_HEADER_SIZE, file_len, "PE COFF header")?;
    let mut coff = [0u8; COFF_HEADER_SIZE as usize];
    read_binary_at(file, pe_offset, &mut coff, "PE COFF header")?;
    let section_count = le_u16(&coff, 6);
    let optional_size = le_u16(&coff, 20);
    let characteristics = le_u16(&coff, 22);
    if coff[..4] != *b"PE\0\0"
        || le_u16(&coff, 4) != 0x8664
        || section_count == 0
        || section_count > MAX_SECTIONS
        || (optional_size as usize) < MIN_PE32_PLUS_SIZE
        || optional_size > MAX_OPTIONAL_HEADER_SIZE
        || characteristics & 0x0002 == 0
        || characteristics & 0x2000 != 0
    {
        return Err("expected an x86_64 PE32+ executable image".to_string());
    }
    let optional_offset = pe_offset + COFF_HEADER_SIZE;
    binary_range_end(
        optional_offset,
        optional_size as u64,
        file_len,
        "PE32+ optional header",
    )?;
    let mut optional = vec![0u8; optional_size as usize];
    read_binary_at(
        file,
        optional_offset,
        &mut optional,
        "PE32+ optional header",
    )?;
    let directory_count = le_u32(&optional, 108) as u64;
    let required_optional_size = 112u64
        .checked_add(
            directory_count
                .checked_mul(8)
                .ok_or_else(|| "PE data-directory size overflows".to_string())?,
        )
        .ok_or_else(|| "PE optional-header size overflows".to_string())?;
    if le_u16(&optional, 0) != 0x020b
        || le_u32(&optional, 16) == 0
        || le_u32(&optional, 32) == 0
        || le_u32(&optional, 36) == 0
        || le_u32(&optional, 56) == 0
        || le_u32(&optional, 60) == 0
        || required_optional_size > optional_size as u64
    {
        return Err("PE32+ optional header is incomplete or invalid".to_string());
    }
    let section_offset = optional_offset + optional_size as u64;
    let section_table_size = SECTION_HEADER_SIZE
        .checked_mul(section_count as u64)
        .ok_or_else(|| "PE section-table size overflows".to_string())?;
    binary_range_end(
        section_offset,
        section_table_size,
        file_len,
        "PE section table",
    )?;
    let mut has_executable_section = false;
    for index in 0..section_count as u64 {
        let offset = section_offset + index * SECTION_HEADER_SIZE;
        let mut section = [0u8; SECTION_HEADER_SIZE as usize];
        read_binary_at(file, offset, &mut section, "PE section header")?;
        let virtual_size = le_u32(&section, 8) as u64;
        let raw_size = le_u32(&section, 16) as u64;
        let raw_offset = le_u32(&section, 20) as u64;
        if raw_size != 0 {
            binary_range_end(raw_offset, raw_size, file_len, "PE section data")?;
        }
        if virtual_size != 0 && le_u32(&section, 36) & 0x2000_0000 != 0 {
            has_executable_section = true;
        }
    }
    if !has_executable_section {
        return Err("PE executable has no executable section".to_string());
    }
    Ok(())
}

fn verify_macho_arm64_macos(file: &mut fs::File, file_len: u64) -> Result<(), String> {
    const MACH_HEADER_SIZE: u64 = 32;
    const MAX_LOAD_COMMANDS: u32 = 4_096;
    const MAX_LOAD_COMMAND_BYTES: u32 = 16 * 1024 * 1024;
    const LC_SEGMENT_64: u32 = 0x19;
    const LC_VERSION_MIN_MACOSX: u32 = 0x24;
    const LC_BUILD_VERSION: u32 = 0x32;
    let mut header = [0u8; MACH_HEADER_SIZE as usize];
    read_binary_at(file, 0, &mut header, "Mach-O 64-bit header")?;
    let command_count = le_u32(&header, 16);
    let command_bytes = le_u32(&header, 20);
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || le_u32(&header, 4) != 0x0100_000c
        || le_u32(&header, 12) != 2
        || command_count == 0
        || command_count > MAX_LOAD_COMMANDS
        || command_bytes == 0
        || command_bytes > MAX_LOAD_COMMAND_BYTES
    {
        return Err("expected a little-endian arm64 Mach-O executable".to_string());
    }
    let commands_end = binary_range_end(
        MACH_HEADER_SIZE,
        command_bytes as u64,
        file_len,
        "Mach-O load-command table",
    )?;
    let mut cursor = MACH_HEADER_SIZE;
    let mut has_loadable_segment = false;
    let mut has_macos_platform = false;
    for _ in 0..command_count {
        let mut command_header = [0u8; 8];
        read_binary_at(file, cursor, &mut command_header, "Mach-O load command")?;
        let command = le_u32(&command_header, 0);
        let command_size = le_u32(&command_header, 4) as u64;
        if command_size < 8 || !command_size.is_multiple_of(8) {
            return Err("Mach-O load command has an invalid size".to_string());
        }
        let next = binary_range_end(cursor, command_size, commands_end, "Mach-O load command")?;
        match command {
            LC_SEGMENT_64 => {
                if command_size < 72 {
                    return Err("Mach-O LC_SEGMENT_64 command is truncated".to_string());
                }
                let mut segment = [0u8; 72];
                read_binary_at(file, cursor, &mut segment, "Mach-O LC_SEGMENT_64")?;
                let file_offset = le_u64(&segment, 40);
                let segment_file_size = le_u64(&segment, 48);
                let segment_memory_size = le_u64(&segment, 32);
                let section_count = le_u32(&segment, 64) as u64;
                let section_bytes = section_count
                    .checked_mul(80)
                    .and_then(|size| size.checked_add(72))
                    .ok_or_else(|| "Mach-O section-table size overflows".to_string())?;
                if section_bytes > command_size || segment_memory_size < segment_file_size {
                    return Err("Mach-O segment or section table is invalid".to_string());
                }
                binary_range_end(
                    file_offset,
                    segment_file_size,
                    file_len,
                    "Mach-O segment data",
                )?;
                if segment_memory_size != 0 && le_u32(&segment, 60) & 0x4 != 0 {
                    has_loadable_segment = true;
                }
            }
            LC_BUILD_VERSION => {
                if command_size < 24 {
                    return Err("Mach-O LC_BUILD_VERSION command is truncated".to_string());
                }
                let mut build = [0u8; 24];
                read_binary_at(file, cursor, &mut build, "Mach-O LC_BUILD_VERSION")?;
                let tool_count = le_u32(&build, 20) as u64;
                let required = tool_count
                    .checked_mul(8)
                    .and_then(|size| size.checked_add(24))
                    .ok_or_else(|| "Mach-O build-tool table size overflows".to_string())?;
                if required > command_size {
                    return Err("Mach-O LC_BUILD_VERSION tool table is truncated".to_string());
                }
                if le_u32(&build, 8) == 1 {
                    has_macos_platform = true;
                }
            }
            LC_VERSION_MIN_MACOSX => {
                if command_size < 16 {
                    return Err("Mach-O LC_VERSION_MIN_MACOSX command is truncated".to_string());
                }
                has_macos_platform = true;
            }
            _ => {}
        }
        cursor = next;
    }
    if cursor != commands_end {
        return Err("Mach-O load-command count does not consume sizeofcmds".to_string());
    }
    if !has_loadable_segment {
        return Err("Mach-O executable has no executable LC_SEGMENT_64".to_string());
    }
    if !has_macos_platform {
        return Err("Mach-O executable is not marked for macOS".to_string());
    }
    Ok(())
}

fn finalize_explicit_target_output(
    temporary: &Path,
    output: &Path,
    source: &Path,
    target: &BuildTarget,
    compiler: &str,
) -> Result<(), KuError> {
    if let Err(reason) = verify_native_binary_target(temporary, target) {
        let _ = fs::remove_file(temporary);
        return Err(KuError::message(format!(
            "native C compiler '{compiler}' produced an invalid '{}' artifact: {reason}\nhelp: configure a compiler/sysroot for {}, or use the target-specific C artifact at {}",
            target.slug,
            target.rust_triple,
            source.display()
        )));
    }
    if let Err(error) = install_link_output(temporary, output) {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn compile_c_source(
    source: &Path,
    output: &Path,
    target: Option<&BuildTarget>,
    profile: BuildProfile,
    static_link: bool,
    verbose: bool,
) -> Result<(), KuError> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            KuError::message(format!(
                "failed to create output directory '{}': {err}",
                parent.display()
            ))
        })?;
    }
    let features = CSourceFeatures::inspect(source)?;
    validate_c_target_features(features, target)?;
    // Native HTTP/Redis use a portable socket layer. Its Windows branch needs
    // Winsock: MSVC auto-links through the emitted pragma, while gcc/clang/zig
    // need `-lws2_32`. Base the decision on the output target, not the build host,
    // so POSIX builds do not resolve a library hidden behind `#if _WIN32`.
    let target_is_windows = target
        .map(|value| value.is_windows)
        .unwrap_or(cfg!(windows));
    let needs_winsock = target_is_windows && features.winsock;
    let needs_pthreads = !target_is_windows && features.pthreads;
    // `#pragma comment(lib, ...)` is MSVC-specific.  The generic clang/gcc/zig
    // path must pass database libraries explicitly or a valid std.pg/std.mysql
    // program compiles to C and then fails at the final link step.
    let needs_libpq = features.libpq;
    validate_libpq_link_mode(needs_libpq, static_link)?;
    let needs_libmysql = features.libmysql && target.is_none_or(BuildTarget::matches_host);
    let libmysql_dir = needs_libmysql.then(detect_libmysql_dir).flatten();
    let libmysql_include = needs_libmysql
        .then(|| detect_libmysql_include_dir(libmysql_dir.as_deref()))
        .flatten();
    let libpq_target = libpq_link_target(target);
    let mut tried = Vec::new();
    let env_cc = env::var("KU_CC").ok();
    let temporary_output = target.map(|_| temporary_link_output(output));
    if let Some(temporary) = &temporary_output {
        if let Some(parent) = temporary.parent() {
            cleanup_stale_link_outputs(parent)?;
        }
        prepare_link_output_staging(temporary)?;
    }
    for candidate in c_compiler_candidates(env_cc.as_deref()) {
        if let Some(target) = target {
            if !c_compiler_supports_explicit_target(&candidate, target) {
                continue;
            }
        }
        tried.push(candidate.label.clone());
        let libpq_format = libpq_library_format(libpq_target.platform, &candidate);
        let libpq_dir = if needs_libpq {
            detect_libpq_dir(libpq_format, libpq_target.allow_host_discovery)?
        } else {
            None
        };
        let mut command = Command::new(&candidate.program);
        for arg in &candidate.args {
            command.arg(arg);
        }
        if let Some(target) = target {
            match candidate.kind {
                CCompilerKind::ZigCc => {
                    command.arg("-target").arg(target.c_triple);
                }
                CCompilerKind::Clang => {
                    command.arg("--target").arg(target.rust_triple);
                }
                CCompilerKind::Preconfigured => {}
            }
        }
        let compiler_output = temporary_output.as_deref().unwrap_or(output);
        command
            .arg(source)
            .arg("-std=c11")
            .arg("-o")
            .arg(compiler_output);
        if let Some(include) = &libmysql_include {
            command.arg(format!("-I{}", include.display()));
        }
        if let Some(opt_level) = profile.rustc_opt_level() {
            command.arg(format!("-O{opt_level}"));
        }
        if static_link {
            command.arg("-static");
        }
        if needs_winsock {
            command.arg("-lws2_32");
        }
        if needs_pthreads {
            command.arg("-pthread");
        }
        if needs_libpq {
            if let Some(dir) = &libpq_dir {
                if matches!(
                    libpq_format,
                    LibpqLibraryFormat::WindowsMsvc | LibpqLibraryFormat::WindowsMingw
                ) {
                    if let Some(library) = find_libpq_library(dir, libpq_format) {
                        command.arg(library);
                    } else {
                        // The library can disappear between discovery and command
                        // construction. Let the linker report that race normally.
                        command.arg("-lpq");
                    }
                } else {
                    command.arg(format!("-L{}", dir.display()));
                    if let Some(library) = find_libpq_library(dir, libpq_format) {
                        command.arg(library);
                    } else {
                        // The library can disappear between discovery and command
                        // construction. Let the linker report that race normally.
                        command.arg("-lpq");
                    }
                }
            } else {
                command.arg("-lpq");
            }
        }
        if needs_libmysql {
            if let Some(dir) = &libmysql_dir {
                if cfg!(windows) {
                    command.arg(dir.join("libmysql.lib"));
                } else if let Some(library) = libmysql_library_in(dir) {
                    command.arg(library);
                } else {
                    command
                        .arg(format!("-L{}", dir.display()))
                        .arg("-lmysqlclient");
                }
            } else {
                command.arg("-lmysqlclient");
            }
        }
        if verbose {
            println!("c compiler command: {command:?}");
        }
        match command.status() {
            Ok(status) if status.success() => {
                if let Some(target) = target {
                    let temporary = temporary_output
                        .as_deref()
                        .expect("an explicit target always links to staging");
                    finalize_explicit_target_output(
                        temporary,
                        output,
                        source,
                        target,
                        &candidate.label,
                    )?;
                }
                return Ok(());
            }
            Ok(status) => {
                if let Some(temporary) = &temporary_output {
                    let _ = fs::remove_file(temporary);
                }
                return Err(KuError::message(format!(
                    "native C build failed: {} exited with {status}\nhelp: inspect generated source at {} and install the selected target's compiler/sysroot; Ku never falls back to a host binary",
                    candidate.label,
                    source.display()
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                if let Some(temporary) = &temporary_output {
                    let _ = fs::remove_file(temporary);
                }
                return Err(KuError::message(format!(
                    "failed to run C compiler '{}': {err}\nhelp: set KU_CC to clang, gcc, or 'zig cc', or use default `ku build`",
                    candidate.label
                )));
            }
        }
    }
    // No GCC-style compiler found. On Windows, fall back to a native MSVC (cl.exe)
    // toolchain located via vswhere and driven through vcvars64.bat. Only for the
    // native target. An explicit target that exactly matches this host may use
    // the same fallback; a real cross build still requires zig/clang or KU_CC.
    if target.is_none() || target.is_some_and(BuildTarget::matches_host) {
        if let Some(vcvars) = detect_msvc_vcvars() {
            tried.push("cl (MSVC)".to_string());
            let compiler_output = temporary_output.as_deref().unwrap_or(output);
            let result = compile_with_msvc(
                &vcvars,
                source,
                compiler_output,
                features,
                profile,
                static_link,
                verbose,
            );
            if let Err(error) = result {
                if let Some(temporary) = &temporary_output {
                    let _ = fs::remove_file(temporary);
                }
                return Err(error);
            }
            if let Some(target) = target {
                finalize_explicit_target_output(
                    compiler_output,
                    output,
                    source,
                    target,
                    "cl (MSVC)",
                )?;
            }
            return Ok(());
        }
    }
    if let Some(temporary) = &temporary_output {
        let _ = fs::remove_file(temporary);
    }
    let target_help = target.map_or_else(
        || "install clang/gcc/zig or Visual Studio (MSVC)".to_string(),
        |target| {
            format!(
                "install zig or clang with a '{}' sysroot, or set KU_CC to a compiler already configured for that target",
                target.rust_triple
            )
        },
    );
    Err(KuError::message(format!(
        "C compiler not found for native build\nhelp: {target_help}; tried {}",
        tried.join(", ")
    )))
}

fn c_compiler_supports_explicit_target(
    candidate: &CCompilerCandidate,
    target: &BuildTarget,
) -> bool {
    target.matches_host()
        || candidate.kind != CCompilerKind::Preconfigured
        || candidate.explicitly_configured
}

/// Locate a native MSVC toolchain by asking vswhere for the latest install that
/// carries the VC C/C++ tools, then returning its `vcvars64.bat`. Returns `None`
/// when vswhere or Visual Studio is absent (e.g. non-Windows hosts).
fn detect_msvc_vcvars() -> Option<PathBuf> {
    let program_files_x86 = env::var("ProgramFiles(x86)")
        .or_else(|_| env::var("ProgramFiles"))
        .ok()?;
    let vswhere = Path::new(&program_files_x86)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vswhere.exists() {
        return None;
    }
    let output = Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let install = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if install.is_empty() {
        return None;
    }
    let vcvars = Path::new(&install)
        .join("VC")
        .join("Auxiliary")
        .join("Build")
        .join("vcvars64.bat");
    vcvars.exists().then_some(vcvars)
}

/// Compile a generated C source with MSVC `cl.exe`. cl needs INCLUDE/LIB/PATH
/// from `vcvars64.bat`, so we snapshot that environment once and inject it into
/// the cl process directly (no `cmd`, which cannot handle the `\\?\` verbatim
/// paths that `fs::canonicalize` produces). `/utf-8` keeps diagnostics readable
/// regardless of the console code page.
fn compile_with_msvc(
    vcvars: &Path,
    source: &Path,
    output: &Path,
    features: CSourceFeatures,
    profile: BuildProfile,
    static_link: bool,
    verbose: bool,
) -> Result<(), KuError> {
    let env = load_vcvars_env(vcvars)?;
    let cl = find_cl_in_env(&env).ok_or_else(|| {
        KuError::message(
            "located Visual Studio but cl.exe was not on the vcvars PATH\nhelp: repair the \"Desktop development with C++\" workload, or install clang/gcc/zig and set KU_CC",
        )
    })?;

    // cl and cmd both dislike \\?\ verbatim paths; hand cl plain absolute paths.
    let source = strip_verbatim(source);
    let output = strip_verbatim(output);
    let obj = output.with_extension("obj");

    let mut command = Command::new(&cl);
    command.env_clear();
    for (key, value) in &env {
        command.env(key, value);
    }
    command.arg("/nologo").arg("/std:c11").arg("/utf-8");
    if let Some(opt) = profile.msvc_opt_flag() {
        command.arg(opt);
    }
    if static_link {
        command.arg("/MT");
    }
    let libmysql_dir = features.libmysql.then(detect_libmysql_dir).flatten();
    if features.libmysql {
        if let Some(include) = detect_libmysql_include_dir(libmysql_dir.as_deref()) {
            command.arg(format!("/I{}", include.display()));
        }
    }
    command.arg(&source);
    command.arg(format!("/Fe:{}", output.display()));
    command.arg(format!("/Fo:{}", obj.display()));
    // A `std.pg` program links libpq.lib (named via a pragma). MSVC needs its search
    // path since libpq is not on the default lib path.
    if features.libpq {
        validate_libpq_link_mode(true, static_link)?;
        if let Some(dir) = detect_libpq_dir(LibpqLibraryFormat::WindowsMsvc, true)? {
            command
                .arg("/link")
                .arg(format!("/LIBPATH:{}", dir.display()));
        }
    }
    if features.libmysql {
        if let Some(dir) = libmysql_dir {
            command
                .arg("/link")
                .arg(format!("/LIBPATH:{}", dir.display()));
        }
    }
    if verbose {
        println!("msvc: {} (env from {})", cl.display(), vcvars.display());
    }

    let result = command.output();
    let _ = fs::remove_file(&obj);
    match result {
        Ok(done) if done.status.success() => Ok(()),
        Ok(done) => {
            let stdout = String::from_utf8_lossy(&done.stdout);
            let stderr = String::from_utf8_lossy(&done.stderr);
            Err(KuError::message(format!(
                "native C build failed: MSVC cl exited with {}\n{}{}help: inspect generated source at {}",
                done.status,
                stdout,
                stderr,
                source.display()
            )))
        }
        Err(err) => Err(KuError::message(format!(
            "failed to run MSVC cl.exe: {err}\nhelp: repair Visual Studio C++ tools, or install clang/gcc/zig and set KU_CC"
        ))),
    }
}

/// Run `vcvars64.bat` and capture the resulting environment as key/value pairs.
/// We write a throwaway bat into a plain temp dir (not the `\\?\` build dir) and
/// run it as `cmd /C <bat>` — passing the vcvars path inside the bat body avoids
/// the `cmd /C "..."` inner-quote escaping that mangles spaced paths. A marker
/// line separates any residual vcvars banner from the `set` dump.
fn load_vcvars_env(vcvars: &Path) -> Result<Vec<(String, String)>, KuError> {
    const MARKER: &str = "___KU_VCVARS_ENV___";
    let bat = env::temp_dir().join(format!("ku_vcvars_probe_{}.bat", std::process::id()));
    let script = format!(
        "@echo off\r\ncall \"{}\" >nul 2>&1\r\necho {}\r\nset\r\n",
        vcvars.display(),
        MARKER
    );
    fs::write(&bat, script)
        .map_err(|err| KuError::message(format!("failed to write vcvars probe script: {err}")))?;
    let result = Command::new("cmd").arg("/C").arg(&bat).output();
    let _ = fs::remove_file(&bat);
    let output =
        result.map_err(|err| KuError::message(format!("failed to run vcvars64.bat: {err}")))?;
    if !output.status.success() {
        return Err(KuError::message(
            "vcvars64.bat failed to initialize the MSVC build environment",
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut vars = Vec::new();
    let mut seen_marker = false;
    for line in text.lines() {
        if !seen_marker {
            seen_marker = line.trim() == MARKER;
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if !key.is_empty() {
                vars.push((key.to_string(), value.to_string()));
            }
        }
    }
    if vars.is_empty() {
        return Err(KuError::message(
            "vcvars64.bat produced no environment; the MSVC C++ tools may be missing",
        ));
    }
    Ok(vars)
}

/// Find cl.exe by scanning the PATH captured from vcvars.
fn find_cl_in_env(env: &[(String, String)]) -> Option<PathBuf> {
    let path = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())?;
    for dir in path.split(';') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join("cl.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Strip a Windows `\\?\` verbatim prefix so downstream tools see a normal path.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn c_compiler_candidates(env_cc: Option<&str>) -> Vec<CCompilerCandidate> {
    let mut candidates = Vec::new();
    if let Some(env_cc) = env_cc {
        if let Some(candidate) = parse_c_compiler_candidate(env_cc, true) {
            candidates.push(candidate);
        }
    }
    for fallback in ["zig cc", "clang", "cc", "gcc"] {
        if let Some(candidate) = parse_c_compiler_candidate(fallback, false) {
            if !candidates
                .iter()
                .any(|existing| existing.label == candidate.label)
            {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn parse_c_compiler_candidate(
    value: &str,
    explicitly_configured: bool,
) -> Option<CCompilerCandidate> {
    let parts = split_command_words(value);
    let (program, args) = parts.split_first()?;
    let label = parts.join(" ");
    let kind = if program == "zig" && args.first().is_some_and(|arg| arg == "cc") {
        CCompilerKind::ZigCc
    } else if Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("clang"))
    {
        CCompilerKind::Clang
    } else {
        CCompilerKind::Preconfigured
    };
    Some(CCompilerCandidate {
        label,
        program: program.clone(),
        args: args.to_vec(),
        kind,
        explicitly_configured,
    })
}

fn split_command_words(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn reject_native_async(program: &Program) -> Result<(), KuError> {
    reject_compiled_async(
        program,
        "native C prototype does not support async/await yet; use the interpreter runtime",
    )
}

fn reject_compiled_async(program: &Program, message: &str) -> Result<(), KuError> {
    if program.items.iter().any(item_contains_async) {
        return Err(KuError::message(message));
    }
    Ok(())
}

fn item_contains_async(item: &Item) -> bool {
    match item {
        Item::Function(function) => function_contains_async(function),
        Item::Import(_) | Item::Struct(_) | Item::Enum(_) | Item::Module(_) => false,
    }
}

fn function_contains_async(function: &FnDecl) -> bool {
    function.is_async || function.body.iter().any(stmt_contains_async)
}

fn stmt_contains_async(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::VarDecl { value, .. }
        | Stmt::Assign { value, .. }
        | Stmt::Fail { value, .. }
        | Stmt::Panic { value, .. }
        | Stmt::Print { value, .. } => expr_contains_await(value),
        Stmt::AssignTarget { target, value, .. } => {
            assign_target_contains_await(target) || expr_contains_await(value)
        }
        Stmt::CompoundAssign { target, value, .. } => {
            assign_target_contains_await(target) || expr_contains_await(value)
        }
        Stmt::DestructureAssign { values, .. } => values.iter().any(expr_contains_await),
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            expr_contains_await(value)
                || bindings
                    .iter()
                    .any(|binding| binding.default.as_ref().is_some_and(expr_contains_await))
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_await(condition)
                || then_branch.iter().any(stmt_contains_async)
                || else_branch.iter().any(stmt_contains_async)
        }
        Stmt::While {
            condition, body, ..
        } => expr_contains_await(condition) || body.iter().any(stmt_contains_async),
        Stmt::For { iterable, body, .. } => {
            expr_contains_await(iterable) || body.iter().any(stmt_contains_async)
        }
        Stmt::Function(function) => function_contains_async(function),
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            body.iter().any(stmt_contains_async)
                || catch_body.iter().any(stmt_contains_async)
                || finally_body.iter().any(stmt_contains_async)
        }
        Stmt::Return { value, .. } => value.as_ref().is_some_and(expr_contains_await),
        Stmt::Expr { expr, .. } => expr_contains_await(expr),
        Stmt::Break { .. } | Stmt::Continue { .. } => false,
    }
}

fn assign_target_contains_await(target: &AssignTarget) -> bool {
    match target {
        AssignTarget::Variable(_) => false,
        AssignTarget::Index { target, index } => {
            expr_contains_await(target) || expr_contains_await(index)
        }
        AssignTarget::Field { target, .. } => expr_contains_await(target),
    }
}

fn expr_contains_await(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Await(_) => true,
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => expr_contains_await(expr),
        ExprKind::Binary { left, right, .. } => {
            expr_contains_await(left) || expr_contains_await(right)
        }
        ExprKind::Call { callee, args } => {
            expr_contains_await(callee) || args.iter().any(expr_contains_await)
        }
        ExprKind::Array(values) => values.iter().any(expr_contains_await),
        ExprKind::Index { target, index } => {
            expr_contains_await(target) || expr_contains_await(index)
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            expr_contains_await(target)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            fields.iter().any(|(_, value)| expr_contains_await(value))
        }
        ExprKind::Match { value, arms } => {
            expr_contains_await(value)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(expr_contains_await)
                        || expr_contains_await(&arm.value)
                })
        }
        ExprKind::Function { body, .. } => body.iter().any(stmt_contains_async),
        ExprKind::Literal(_) | ExprKind::Variable(_) => false,
    }
}

struct TempBuildDir {
    path: PathBuf,
}

impl TempBuildDir {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Drop for TempBuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn build_runner_source(path: &str, source: &str, dependency_mode: DependencyResolveMode) -> String {
    let literal = raw_string_literal(source);
    let dependency_mode = match dependency_mode {
        DependencyResolveMode::Refresh => "Refresh",
        DependencyResolveMode::Update => "Update",
        DependencyResolveMode::Locked => "Locked",
        DependencyResolveMode::Offline => "Offline",
    };
    format!(
        "const SOURCE: &str = {literal};\nfn main() {{\n    if let Err(err) = ku::cli::run_source_with_dependency_mode(\n        {path:?},\n        SOURCE,\n        ku::package::DependencyResolveMode::{dependency_mode},\n    ) {{\n        eprintln!(\"{{err}}\");\n        std::process::exit(1);\n    }}\n}}\n"
    )
}

fn raw_string_literal(source: &str) -> String {
    for hashes in 0..16 {
        let fence = "#".repeat(hashes);
        let close = format!("\"{fence}");
        if !source.contains(&close) {
            return format!("r{fence}\"{source}\"{fence}");
        }
    }
    format!("{source:?}")
}

fn find_ku_rlib(exe_dir: &Path) -> Result<PathBuf, KuError> {
    let direct = exe_dir.join("libku.rlib");
    let deps = exe_dir.join("deps");
    let mut candidates = Vec::new();
    if direct.is_file() {
        candidates.push(direct.clone());
    }
    if let Ok(entries) = fs::read_dir(&deps) {
        for (index, entry) in entries.enumerate() {
            if index >= MAX_RLIB_DIRECTORY_ENTRIES {
                return Err(KuError::message(format!(
                    "ku build stopped after scanning {MAX_RLIB_DIRECTORY_ENTRIES} entries in '{}'; remove stale target artifacts and retry",
                    deps.display()
                )));
            }
            let entry = entry.map_err(|err| {
                KuError::message(format!(
                    "failed to inspect Rust dependency directory '{}': {err}",
                    deps.display()
                ))
            })?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("libku-") && name.ends_with(".rlib") && path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop().ok_or_else(|| {
        KuError::message(format!(
            "ku build needs libku.rlib next to the ku executable; looked in {} and {}",
            direct.display(),
            deps.display()
        ))
    })
}

fn find_dependency_dirs(exe_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    push_existing_dir(&mut dirs, exe_dir.join("deps"));
    if exe_dir.file_name().is_some_and(|name| name == "release") {
        if let Some(repo) = exe_dir.parent().and_then(Path::parent) {
            push_existing_dir(&mut dirs, repo.join("target").join("release").join("deps"));
        }
    }
    if exe_dir.file_name().is_some_and(|name| name == "debug") {
        if let Some(repo) = exe_dir.parent().and_then(Path::parent) {
            push_existing_dir(&mut dirs, repo.join("target").join("debug").join("deps"));
        }
    }
    if exe_dir.file_name().is_some_and(|name| name == "release") {
        if let Some(repo) = exe_dir.parent() {
            push_existing_dir(&mut dirs, repo.join("target").join("release").join("deps"));
        }
    }
    if exe_dir.file_name().is_some_and(|name| name == "debug") {
        if let Some(repo) = exe_dir.parent() {
            push_existing_dir(&mut dirs, repo.join("target").join("debug").join("deps"));
        }
    }
    dirs
}

fn push_existing_dir(dirs: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() && !dirs.iter().any(|existing| existing == &path) {
        dirs.push(path);
    }
}

fn expected_ku_file(path: &str) -> KuError {
    KuError::message(expected_ku_file_message(path))
}

fn expected_ku_file_message(path: &str) -> String {
    format!("expected a .ku source file, got '{path}'")
}

fn command_error(message: impl Into<String>) -> KuError {
    KuError::message(format!("{}\n\n{}", message.into(), HELP))
}

fn diagnostic_json_line(error: &KuError, file: &str, source: &str) -> String {
    let diagnostic = error.diagnostic_data(file, source);
    format!(
        "{{\"level\":{},\"code\":{},\"message\":{},\"file\":{},\"line\":{},\"column\":{},\"endLine\":{},\"endColumn\":{},\"notes\":{},\"helps\":{}}}",
        json_string(diagnostic.level),
        json_string(diagnostic.code),
        json_string(&diagnostic.message),
        json_string(&diagnostic.file),
        diagnostic.line,
        diagnostic.column,
        diagnostic.end_line,
        diagnostic.end_column,
        json_string_array(&diagnostic.notes),
        json_string_array(&diagnostic.helps),
    )
}

fn json_string_array(values: &[&str]) -> String {
    let values = values
        .iter()
        .map(|value| json_string(value))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{values}]")
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch <= '\u{1F}' => {
                output.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn is_ku_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
}

fn looks_like_file_path(path: &str) -> bool {
    Path::new(path).extension().is_some() || path.contains('/') || path.contains('\\')
}

pub fn check_source(file: &str, source: &str) -> Result<(), KuError> {
    check_source_with_dependency_mode(file, source, DependencyResolveMode::Update)
}

pub fn check_source_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<(), KuError> {
    check_source_with_options(
        file,
        source,
        CheckOptions {
            dependency_mode,
            ..CheckOptions::default()
        },
    )
}

fn check_source_with_options(
    file: &str,
    source: &str,
    options: CheckOptions,
) -> Result<(), KuError> {
    parse_and_check_with_options(file, source, options)
        .map(|_| ())
        .map_err(|err| KuError::message(err.diagnostic(file, source)))
}

fn checked_program_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<Program, KuError> {
    let program = parse_and_check_with_dependency_mode(file, source, dependency_mode)
        .map_err(|err| KuError::message(err.diagnostic(file, source)))?;
    Ok(program)
}

pub fn run_source(file: &str, source: &str) -> Result<(), KuError> {
    run_source_with_dependency_mode(file, source, DependencyResolveMode::Update)
}

pub fn run_source_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<(), KuError> {
    let program = checked_program_with_dependency_mode(file, source, dependency_mode)?;
    run_program_with_stack(program, source_base_dir(file))
        .map_err(|err| KuError::message(err.diagnostic(file, source)))
}

fn run_program_with_stack(program: Program, base_dir: PathBuf) -> Result<(), KuError> {
    thread::Builder::new()
        .name("ku-interpreter".to_string())
        .stack_size(INTERPRETER_STACK_SIZE)
        .spawn(move || {
            let mut interpreter = Interpreter::with_base_dir(base_dir);
            interpreter.run(program)
        })
        .map_err(|err| KuError::message(format!("failed to start interpreter: {err}")))?
        .join()
        .map_err(|_| KuError::message("interpreter thread panicked"))?
}

fn source_base_dir(file: &str) -> PathBuf {
    Path::new(file)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn parse_source(source: &str) -> Result<Program, KuError> {
    let tokens = Lexer::new(source).tokenize()?;
    Parser::new(tokens).parse()
}

fn parse_and_check(file: &str, source: &str) -> Result<Program, KuError> {
    parse_and_check_with_dependency_mode(file, source, DependencyResolveMode::Update)
}

fn parse_and_check_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<Program, KuError> {
    parse_and_check_with_options(
        file,
        source,
        CheckOptions {
            dependency_mode,
            ..CheckOptions::default()
        },
    )
}

fn parse_and_check_with_options(
    file: &str,
    source: &str,
    options: CheckOptions,
) -> Result<Program, KuError> {
    let original = parse_source(source)?;
    deny_unused_imports(&original)?;
    let program = parse_and_expand_with_dependency_mode(file, source, options.dependency_mode)?;
    Checker::new().check(&program)?;
    if options.deny_unused {
        deny_unused_local_bindings(&program)?;
    }
    Ok(program)
}

#[derive(Debug, Clone)]
struct UnusedBinding {
    name: String,
    span: Span,
    used: bool,
}

#[derive(Debug, Default)]
struct UnusedScope {
    bindings: Vec<UnusedBinding>,
}

#[derive(Debug, Default)]
struct UnusedAnalyzer {
    scopes: Vec<UnusedScope>,
}

impl UnusedAnalyzer {
    fn new() -> Self {
        Self {
            scopes: vec![UnusedScope::default()],
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(UnusedScope::default());
    }

    fn pop_scope(&mut self) -> KuResult<()> {
        let Some(scope) = self.scopes.pop() else {
            return Ok(());
        };
        for binding in scope.bindings {
            if !binding.used {
                return Err(unused_binding_error(&binding.name, binding.span));
            }
        }
        Ok(())
    }

    fn define(&mut self, name: &str, span: Span, track: bool) {
        if name == "_" || name.starts_with('_') {
            return;
        }
        let used = !track;
        if let Some(scope) = self.scopes.last_mut() {
            if let Some(existing) = scope
                .bindings
                .iter_mut()
                .find(|binding| binding.name == name)
            {
                existing.span = span;
                existing.used = used;
            } else {
                scope.bindings.push(UnusedBinding {
                    name: name.to_string(),
                    span,
                    used,
                });
            }
        }
    }

    fn binding_exists(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.bindings.iter().any(|binding| binding.name == name))
    }

    fn use_name(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope
                .bindings
                .iter_mut()
                .rev()
                .find(|binding| binding.name == name)
            {
                binding.used = true;
                return;
            }
        }
    }

    fn write_name(&mut self, name: &str, span: Span) {
        if name == "_" || name.starts_with('_') {
            return;
        }
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope
                .bindings
                .iter_mut()
                .rev()
                .find(|binding| binding.name == name)
            {
                binding.span = span;
                binding.used = false;
                return;
            }
        }
        self.define(name, span, true);
    }

    fn finish(mut self) -> KuResult<()> {
        while !self.scopes.is_empty() {
            self.pop_scope()?;
        }
        Ok(())
    }

    fn visit_items(&mut self, items: &[Item]) -> KuResult<()> {
        for item in items {
            if let Item::Function(function) = item {
                self.visit_function(function, false)?;
            }
        }
        Ok(())
    }

    fn visit_function(&mut self, function: &FnDecl, track_name: bool) -> KuResult<()> {
        if track_name {
            self.define(&function.name, function.span, true);
        }
        self.push_scope();
        if track_name {
            self.define(&function.name, function.span, false);
        }
        for param in &function.params {
            self.define(&param.name, param.span, false);
        }
        self.visit_block(&function.body)?;
        self.pop_scope()
    }

    fn visit_block(&mut self, body: &[Stmt]) -> KuResult<()> {
        for stmt in body {
            self.visit_stmt(stmt)?;
        }
        Ok(())
    }

    fn visit_scoped_block(&mut self, body: &[Stmt]) -> KuResult<()> {
        self.push_scope();
        self.visit_block(body)?;
        self.pop_scope()
    }

    fn visit_stmt(&mut self, stmt: &Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name, value, span, ..
            } => {
                self.visit_expr(value)?;
                self.define(name, *span, true);
            }
            Stmt::Assign { name, value, span } => {
                self.visit_expr(value)?;
                if self.binding_exists(name) {
                    self.write_name(name, *span);
                } else {
                    self.define(name, *span, true);
                }
            }
            Stmt::AssignTarget { target, value, .. } => {
                self.visit_assign_target(target)?;
                self.visit_expr(value)?;
            }
            Stmt::CompoundAssign { target, value, .. } => {
                self.visit_assign_target(target)?;
                self.visit_expr(value)?;
            }
            Stmt::DestructureAssign {
                names,
                values,
                span,
            } => {
                for value in values {
                    self.visit_expr(value)?;
                }
                for name in names.iter().flatten() {
                    self.define(name, *span, true);
                }
            }
            Stmt::ObjectDestructureAssign {
                bindings,
                rest,
                value,
                ..
            } => {
                self.visit_expr(value)?;
                for binding in bindings {
                    if let Some(default) = &binding.default {
                        self.visit_expr(default)?;
                    }
                    let local = binding.local.as_deref().unwrap_or(&binding.field);
                    self.define(local, binding.span, true);
                }
                if let Some(rest) = rest {
                    if let Some(local) = &rest.local {
                        self.define(local, rest.span, true);
                    }
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.visit_expr(condition)?;
                self.visit_scoped_block(then_branch)?;
                if !else_branch.is_empty() {
                    self.visit_scoped_block(else_branch)?;
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.visit_expr(condition)?;
                self.visit_scoped_block(body)?;
            }
            Stmt::For {
                name,
                iterable,
                body,
                span,
            } => {
                self.visit_expr(iterable)?;
                self.push_scope();
                self.define(name, *span, true);
                self.visit_block(body)?;
                self.pop_scope()?;
            }
            Stmt::Function(function) => {
                self.visit_function(function, true)?;
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                span,
            } => {
                self.visit_scoped_block(body)?;
                if !catch_body.is_empty() {
                    self.push_scope();
                    if let Some(catch_name) = catch_name {
                        self.define(catch_name, *span, true);
                    }
                    self.visit_block(catch_body)?;
                    self.pop_scope()?;
                }
                if !finally_body.is_empty() {
                    self.visit_scoped_block(finally_body)?;
                }
            }
            Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
                self.visit_expr(value)?;
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.visit_expr(value)?;
                }
            }
            Stmt::Expr { expr, .. } => {
                self.visit_expr(expr)?;
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
        Ok(())
    }

    fn visit_assign_target(&mut self, target: &AssignTarget) -> KuResult<()> {
        match target {
            AssignTarget::Variable(name) => self.use_name(name),
            AssignTarget::Index { target, index } => {
                self.visit_expr(target)?;
                self.visit_expr(index)?;
            }
            AssignTarget::Field { target, .. } => {
                self.visit_expr(target)?;
            }
        }
        Ok(())
    }

    fn visit_expr(&mut self, expr: &Expr) -> KuResult<()> {
        match &expr.kind {
            ExprKind::Literal(Literal::TemplateString(text)) => {
                self.visit_template_string(text, expr.span)?;
            }
            ExprKind::Literal(_) => {}
            ExprKind::Variable(name) => self.use_name(name),
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
                self.visit_expr(expr)?;
            }
            ExprKind::Binary { left, right, .. } => {
                self.visit_expr(left)?;
                self.visit_expr(right)?;
            }
            ExprKind::Call { callee, args } => {
                self.visit_expr(callee)?;
                for arg in args {
                    self.visit_expr(arg)?;
                }
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.visit_expr(value)?;
                }
            }
            ExprKind::Index { target, index } => {
                self.visit_expr(target)?;
                self.visit_expr(index)?;
            }
            ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
                self.visit_expr(target)?;
            }
            ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                for (_, value) in fields {
                    self.visit_expr(value)?;
                }
            }
            ExprKind::Match { value, arms } => {
                self.visit_expr(value)?;
                for arm in arms {
                    self.push_scope();
                    self.define_match_pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard)?;
                    }
                    self.visit_expr(&arm.value)?;
                    self.pop_scope()?;
                }
            }
            ExprKind::Function { params, body, .. } => {
                self.push_scope();
                for param in params {
                    self.define(&param.name, param.span, false);
                }
                self.visit_block(body)?;
                self.pop_scope()?;
            }
        }
        Ok(())
    }

    fn visit_template_string(&mut self, text: &str, base_span: Span) -> KuResult<()> {
        let mut chars = text.char_indices().peekable();
        while let Some((_, ch)) = chars.next() {
            if ch == '\\' {
                let _ = chars.next();
                continue;
            }
            if ch != '{' {
                continue;
            }
            let mut expr = String::new();
            let mut depth = 1usize;
            let iter = chars.by_ref();
            while let Some((_, inner)) = iter.next() {
                match inner {
                    '\\' => {
                        expr.push(inner);
                        if let Some((_, escaped)) = iter.next() {
                            expr.push(escaped);
                        }
                    }
                    '{' => {
                        depth += 1;
                        expr.push(inner);
                    }
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                        expr.push(inner);
                    }
                    _ => expr.push(inner),
                }
            }
            if depth == 0 && !expr.trim().is_empty() {
                let parsed = Lexer::new(&expr)
                    .tokenize()
                    .and_then(|tokens| Parser::new(tokens).parse_expression_only())
                    .map_err(|err| KuError::runtime(err.message, base_span))?;
                self.visit_expr(&parsed)?;
            }
        }
        Ok(())
    }

    fn define_match_pattern(&mut self, pattern: &MatchPattern) {
        match pattern {
            MatchPattern::Binding(name) => self.define(name, Span::default(), true),
            MatchPattern::EnumVariant { fields, .. } => {
                for field in fields {
                    self.define_match_pattern(field);
                }
            }
            MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
        }
    }
}

fn deny_unused_local_bindings(program: &Program) -> KuResult<()> {
    let mut analyzer = UnusedAnalyzer::new();
    analyzer.visit_items(&program.items)?;
    analyzer.finish()
}

fn deny_unused_imports(program: &Program) -> KuResult<()> {
    let used = collect_import_name_references(program);
    for item in &program.items {
        let Item::Import(import) = item else {
            continue;
        };
        if is_std_import_path(&import.path) && std_import_modules(import).is_err() {
            continue;
        }
        match &import.kind {
            ImportKind::Named(names) => {
                for name in names {
                    let local = name.local_name();
                    if local == "_" || local.starts_with('_') {
                        continue;
                    }
                    if !used.contains(local) {
                        return Err(unused_import_error(local, name.span));
                    }
                }
            }
            ImportKind::Namespace(namespace) => {
                if namespace == "_" || namespace.starts_with('_') {
                    continue;
                }
                if !used.contains(namespace) {
                    return Err(unused_import_error(namespace, import.span));
                }
            }
            ImportKind::Glob => {}
        }
    }
    Ok(())
}

fn unused_binding_error(name: &str, span: Span) -> KuError {
    KuError::runtime(
        format!(
            "unused local binding '{name}'; remove it, use it, or rename it to '_{name}' when it is intentionally unused"
        ),
        span,
    )
}

fn unused_import_error(name: &str, span: Span) -> KuError {
    KuError::runtime(
        format!(
            "unused import '{name}'; remove it, use it, or rename it to '_{name}' when it is intentionally unused"
        ),
        span,
    )
}

fn collect_import_name_references(program: &Program) -> HashSet<String> {
    let mut used = HashSet::new();
    for item in &program.items {
        match item {
            Item::Import(_) | Item::Module(_) => {}
            Item::Function(function) => collect_function_references(function, &mut used),
            Item::Struct(decl) => {
                for field in &decl.fields {
                    if let Some(ty) = &field.ty {
                        collect_type_references(ty, &mut used);
                    }
                }
            }
            Item::Enum(decl) => {
                for variant in &decl.variants {
                    for field in &variant.fields {
                        if let Some(ty) = &field.ty {
                            collect_type_references(ty, &mut used);
                        }
                    }
                }
            }
        }
    }
    used
}

fn collect_function_references(function: &FnDecl, used: &mut HashSet<String>) {
    for param in &function.params {
        if let Some(ty) = &param.ty {
            collect_type_references(ty, used);
        }
    }
    if let Some(return_type) = &function.return_type {
        collect_type_references(return_type, used);
    }
    collect_stmt_references(&function.body, used);
}

fn collect_stmt_references(body: &[Stmt], used: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::VarDecl { ty, value, .. } => {
                if let Some(ty) = ty {
                    collect_type_references(ty, used);
                }
                collect_expr_references(value, used);
            }
            Stmt::Assign { value, .. } | Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => {
                collect_expr_references(value, used)
            }
            Stmt::Print { value, .. } => collect_expr_references(value, used),
            Stmt::AssignTarget { target, value, .. }
            | Stmt::CompoundAssign { target, value, .. } => {
                collect_assign_target_references(target, used);
                collect_expr_references(value, used);
            }
            Stmt::DestructureAssign { values, .. } => {
                for value in values {
                    collect_expr_references(value, used);
                }
            }
            Stmt::ObjectDestructureAssign {
                bindings, value, ..
            } => {
                collect_expr_references(value, used);
                for binding in bindings {
                    if let Some(default) = &binding.default {
                        collect_expr_references(default, used);
                    }
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                collect_expr_references(condition, used);
                collect_stmt_references(then_branch, used);
                collect_stmt_references(else_branch, used);
            }
            Stmt::While {
                condition, body, ..
            } => {
                collect_expr_references(condition, used);
                collect_stmt_references(body, used);
            }
            Stmt::For { iterable, body, .. } => {
                collect_expr_references(iterable, used);
                collect_stmt_references(body, used);
            }
            Stmt::Function(function) => collect_function_references(function, used),
            Stmt::Try {
                body,
                catch_body,
                finally_body,
                ..
            } => {
                collect_stmt_references(body, used);
                collect_stmt_references(catch_body, used);
                collect_stmt_references(finally_body, used);
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    collect_expr_references(value, used);
                }
            }
            Stmt::Expr { expr, .. } => collect_expr_references(expr, used),
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }
}

fn collect_assign_target_references(target: &AssignTarget, used: &mut HashSet<String>) {
    match target {
        AssignTarget::Variable(name) => collect_name_reference(name, used),
        AssignTarget::Index { target, index } => {
            collect_expr_references(target, used);
            collect_expr_references(index, used);
        }
        AssignTarget::Field { target, .. } => collect_expr_references(target, used),
    }
}

fn collect_expr_references(expr: &Expr, used: &mut HashSet<String>) {
    match &expr.kind {
        ExprKind::Variable(name) => collect_name_reference(name, used),
        ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
            collect_expr_references(expr, used)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr_references(left, used);
            collect_expr_references(right, used);
        }
        ExprKind::Call { callee, args } => {
            collect_expr_references(callee, used);
            for arg in args {
                collect_expr_references(arg, used);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_expr_references(value, used);
            }
        }
        ExprKind::Index { target, index } => {
            collect_expr_references(target, used);
            collect_expr_references(index, used);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_expr_references(target, used);
        }
        ExprKind::StructLiteral { name, fields } => {
            collect_name_reference(name, used);
            for (_, value) in fields {
                collect_expr_references(value, used);
            }
        }
        ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_expr_references(value, used);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_expr_references(value, used);
            for arm in arms {
                collect_match_pattern_references(&arm.pattern, used);
                if let Some(guard) = &arm.guard {
                    collect_expr_references(guard, used);
                }
                collect_expr_references(&arm.value, used);
            }
        }
        ExprKind::Function {
            params,
            return_type,
            body,
        } => {
            for param in params {
                if let Some(ty) = &param.ty {
                    collect_type_references(ty, used);
                }
            }
            if let Some(return_type) = return_type {
                collect_type_references(return_type, used);
            }
            collect_stmt_references(body, used);
        }
        ExprKind::Literal(_) => {}
    }
}

fn collect_match_pattern_references(pattern: &MatchPattern, used: &mut HashSet<String>) {
    if let MatchPattern::EnumVariant {
        enum_name, fields, ..
    } = pattern
    {
        collect_name_reference(enum_name, used);
        for field in fields {
            collect_match_pattern_references(field, used);
        }
    }
}

fn collect_type_references(ty: &TypeName, used: &mut HashSet<String>) {
    match ty {
        TypeName::Array(inner) | TypeName::Result(inner) => collect_type_references(inner, used),
        TypeName::Function {
            params,
            return_type,
            ..
        } => {
            for param in params {
                collect_type_references(param, used);
            }
            collect_type_references(return_type, used);
        }
        TypeName::Union(types) => {
            for ty in types {
                collect_type_references(ty, used);
            }
        }
        TypeName::Custom(name) => collect_name_reference(name, used),
        TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::String | TypeName::Null => {}
    }
}

fn collect_name_reference(name: &str, used: &mut HashSet<String>) {
    used.insert(name.to_string());
    if let Some((namespace, _)) = name.split_once('.') {
        used.insert(namespace.to_string());
    }
}

fn parse_and_expand_with_dependency_mode(
    file: &str,
    source: &str,
    dependency_mode: DependencyResolveMode,
) -> Result<Program, KuError> {
    let program = parse_source(source)?;
    let program = if program_has_imports(&program) {
        let path = Path::new(file);
        if !path.exists() {
            if program_has_only_std_imports(&program) {
                let mut loader = ModuleLoader::new(None)?;
                loader.load_virtual_entry(path, program, source.len())?
            } else {
                return Err(KuError::runtime(
                    "imports require a real .ku file path",
                    Span::default(),
                ));
            }
        } else {
            let mut package = package::discover_for_file(path)?;
            let deadline = package
                .as_ref()
                .map(|_| package::package_operation_deadline());
            let _usage_lease = package
                .as_ref()
                .map(|package| {
                    package::acquire_package_usage_lease_until(
                        package,
                        deadline.expect("package operation deadline is present"),
                    )
                })
                .transpose()?;
            if let Some(package) = &mut package {
                package::ensure_cache_dir(package)?;
                package::resolve_remote_dependencies_with_mode_until(
                    package,
                    dependency_mode,
                    deadline.expect("package operation deadline is present"),
                )?;
            }
            let mut loader = ModuleLoader::new(package)?;
            let program = loader.load_entry(path, program, source.len())?;
            if matches!(
                dependency_mode,
                DependencyResolveMode::Update | DependencyResolveMode::Refresh
            ) {
                if let Some(package) = &loader.package {
                    package::write_lock_with_frozen_dependencies(
                        package,
                        &loader.dependency_snapshots,
                    )?;
                }
            }
            program
        }
    } else {
        program
    };
    Ok(program)
}

fn program_has_imports(program: &Program) -> bool {
    program
        .items
        .iter()
        .any(|item| matches!(item, Item::Import(_)))
}

fn program_has_only_std_imports(program: &Program) -> bool {
    program.items.iter().all(|item| match item {
        Item::Import(import) => is_std_import_path(&import.path),
        _ => true,
    })
}

fn is_std_import_path(path: &str) -> bool {
    path == "std" || path.starts_with("std.") || path.starts_with("std:")
}

struct ModuleExports {
    path: PathBuf,
    /// Source-facing export name -> the single compiler symbol interned for the
    /// defining module. Import aliases never create another declaration/name;
    /// they only rewrite references to this symbol.
    exports: BTreeMap<String, String>,
    closure: ModuleClosure,
}

const IMPORT_CLOSURE_WORDS: usize = MAX_IMPORT_MODULES.div_ceil(64);

#[derive(Clone)]
struct ModuleClosure {
    words: [u64; IMPORT_CLOSURE_WORDS],
}

impl ModuleClosure {
    fn new() -> Self {
        Self {
            words: [0; IMPORT_CLOSURE_WORDS],
        }
    }

    fn insert(&mut self, module_id: usize) {
        debug_assert!((1..=MAX_IMPORT_MODULES).contains(&module_id));
        let index = module_id - 1;
        self.words[index / 64] |= 1_u64 << (index % 64);
    }

    fn union_with(&mut self, other: &Self) {
        for (word, other) in self.words.iter_mut().zip(other.words.iter().copied()) {
            *word |= other;
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words
            .iter()
            .enumerate()
            .flat_map(|(word_index, word)| {
                let word = *word;
                (0..64).filter_map(move |bit| {
                    (word & (1_u64 << bit) != 0).then_some(word_index * 64 + bit + 1)
                })
            })
    }
}

#[derive(Clone, Copy, Default)]
struct ExpandedModuleMaterial {
    items: usize,
    source_bytes: u64,
}

impl ExpandedModuleMaterial {
    fn new(source_bytes: usize) -> Self {
        Self {
            items: 0,
            source_bytes: source_bytes as u64,
        }
    }

    fn add_item(&mut self) {
        self.items += 1;
    }
}

#[derive(Default)]
struct ImportBudget {
    modules: usize,
    active_depth: usize,
    source_bytes: u64,
    expanded_items: usize,
    cloned_source_bytes: u64,
    import_edges: usize,
    import_bindings: usize,
}

impl ImportBudget {
    fn begin_module(&mut self, span: Span) -> KuResult<()> {
        if self.modules >= MAX_IMPORT_MODULES {
            return Err(import_limit_error(
                "module_limit",
                format!("import graph exceeds {MAX_IMPORT_MODULES} source modules"),
                span,
            ));
        }
        if self.active_depth >= MAX_IMPORT_DEPTH {
            return Err(import_limit_error(
                "depth_limit",
                format!("import graph exceeds recursive depth {MAX_IMPORT_DEPTH}"),
                span,
            ));
        }
        self.modules += 1;
        self.active_depth += 1;
        Ok(())
    }

    fn finish_module(&mut self) {
        debug_assert!(self.active_depth > 0);
        self.active_depth -= 1;
    }

    fn charge_source(&mut self, bytes: usize, span: Span) -> KuResult<()> {
        let next = self.source_bytes.checked_add(bytes as u64).ok_or_else(|| {
            import_limit_error(
                "source_limit",
                "import graph source byte accounting overflowed",
                span,
            )
        })?;
        if next > MAX_IMPORT_SOURCE_BYTES {
            return Err(import_limit_error(
                "source_limit",
                format!("import graph source exceeds {MAX_IMPORT_SOURCE_BYTES} cumulative bytes"),
                span,
            ));
        }
        self.source_bytes = next;
        Ok(())
    }

    fn charge_item(&mut self, span: Span) -> KuResult<()> {
        self.charge_items(1, span)
    }

    fn charge_import_edge(&mut self, span: Span) -> KuResult<()> {
        self.import_edges = self.import_edges.checked_add(1).ok_or_else(|| {
            import_limit_error("edge_limit", "import edge accounting overflowed", span)
        })?;
        if self.import_edges > MAX_IMPORT_EDGES {
            return Err(import_limit_error(
                "edge_limit",
                format!("import graph exceeds {MAX_IMPORT_EDGES} import edges"),
                span,
            ));
        }
        Ok(())
    }

    fn charge_import_bindings(&mut self, count: usize, span: Span) -> KuResult<()> {
        self.import_bindings = self.import_bindings.checked_add(count).ok_or_else(|| {
            import_limit_error(
                "binding_limit",
                "import binding accounting overflowed",
                span,
            )
        })?;
        if self.import_bindings > MAX_IMPORT_BINDINGS {
            return Err(import_limit_error(
                "binding_limit",
                format!("import graph exceeds {MAX_IMPORT_BINDINGS} imported bindings"),
                span,
            ));
        }
        self.charge_items(count, span)
    }

    fn charge_items(&mut self, count: usize, span: Span) -> KuResult<()> {
        let next = self.expanded_items.checked_add(count).ok_or_else(|| {
            import_limit_error(
                "expanded_item_limit",
                "import expansion item accounting overflowed",
                span,
            )
        })?;
        if next > MAX_IMPORT_EXPANDED_ITEMS {
            return Err(import_limit_error(
                "expanded_item_limit",
                format!("import expansion exceeds {MAX_IMPORT_EXPANDED_ITEMS} materialized items"),
                span,
            ));
        }
        self.expanded_items = next;
        Ok(())
    }

    fn charge_clone(&mut self, material: ExpandedModuleMaterial, span: Span) -> KuResult<()> {
        let next_items = self
            .expanded_items
            .checked_add(material.items)
            .ok_or_else(|| {
                import_limit_error(
                    "expanded_item_limit",
                    "import expansion item accounting overflowed",
                    span,
                )
            })?;
        if next_items > MAX_IMPORT_EXPANDED_ITEMS {
            return Err(import_limit_error(
                "expanded_item_limit",
                format!("import expansion exceeds {MAX_IMPORT_EXPANDED_ITEMS} materialized items"),
                span,
            ));
        }
        let next_bytes = self
            .cloned_source_bytes
            .checked_add(material.source_bytes)
            .ok_or_else(|| {
                import_limit_error(
                    "expanded_clone_limit",
                    "import expansion clone accounting overflowed",
                    span,
                )
            })?;
        if next_bytes > MAX_IMPORT_CLONED_SOURCE_BYTES {
            return Err(import_limit_error(
                "expanded_clone_limit",
                format!(
                    "import expansion exceeds {MAX_IMPORT_CLONED_SOURCE_BYTES} source-equivalent cloned bytes"
                ),
                span,
            ));
        }
        self.expanded_items = next_items;
        self.cloned_source_bytes = next_bytes;
        Ok(())
    }
}

fn import_limit_error(code: &'static str, message: impl Into<String>, span: Span) -> KuError {
    KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "import",
        code,
        message,
        span,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Visiting,
    Done,
}

struct ModuleLoader {
    states: HashMap<PathBuf, LoadState>,
    modules: HashMap<PathBuf, Arc<ModuleExports>>,
    /// A module receives one identity for the whole compilation. The generated
    /// name is based on this id rather than on an individual import edge, so a
    /// diamond graph cannot split one nominal struct/enum into multiple types.
    module_ids: HashMap<PathBuf, usize>,
    next_module_id: usize,
    /// Canonical declarations in dependency-before-dependent order. Each
    /// source declaration is stored here once, independent of import diamonds.
    materialized_modules: Vec<Vec<Item>>,
    /// Source-equivalent size of each canonical module's stored declarations.
    /// This is parallel to `materialized_modules` and lets every real checker /
    /// final-program AST clone participate in the hard expansion budget.
    materialized_materials: Vec<ExpandedModuleMaterial>,
    materialized_order: Vec<usize>,
    materialized_names: HashSet<String>,
    package: Option<PackageContext>,
    package_import_scopes: Vec<package::PackageImportScope>,
    dependency_snapshots: Vec<package::LockDependency>,
    budget: ImportBudget,
}

impl ModuleLoader {
    fn new(package: Option<PackageContext>) -> KuResult<Self> {
        let package_import_scopes = package
            .as_ref()
            .map(|package| package::package_import_scopes(package, Span::default()))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            states: HashMap::new(),
            modules: HashMap::new(),
            module_ids: HashMap::new(),
            next_module_id: 0,
            materialized_modules: Vec::new(),
            materialized_materials: Vec::new(),
            materialized_order: Vec::new(),
            materialized_names: HashSet::new(),
            package,
            package_import_scopes,
            dependency_snapshots: Vec::new(),
            budget: ImportBudget::default(),
        })
    }

    fn load_virtual_entry(
        &mut self,
        path: &Path,
        program: Program,
        source_bytes: usize,
    ) -> KuResult<Program> {
        self.budget.begin_module(Span::default())?;
        let result = (|| {
            self.budget.charge_source(source_bytes, Span::default())?;
            self.expand_program(path, program, true, source_bytes)
                .map(|(program, _, _, _)| program)
        })();
        self.budget.finish_module();
        result
    }

    fn load_entry(
        &mut self,
        path: &Path,
        program: Program,
        source_bytes: usize,
    ) -> KuResult<Program> {
        let canonical = canonical_file(path, Span::default())?;
        self.budget.begin_module(Span::default())?;
        let result = (|| {
            self.budget.charge_source(source_bytes, Span::default())?;
            self.states.insert(canonical.clone(), LoadState::Visiting);
            let (expanded, _, _, _) =
                self.expand_program(&canonical, program, true, source_bytes)?;
            self.states.insert(canonical.clone(), LoadState::Done);
            let mut items = Vec::new();
            let mut materialized_names = HashSet::new();
            for module_id in &self.materialized_order {
                let material = self.materialized_materials[module_id - 1];
                self.budget.charge_clone(material, Span::default())?;
                for item in &self.materialized_modules[module_id - 1] {
                    push_materialized_item(&mut items, &mut materialized_names, item.clone());
                }
            }
            for item in expanded.items {
                if matches!(&item, Item::Module(module) if module.name.starts_with("std:")) {
                    push_materialized_item(&mut items, &mut materialized_names, item);
                } else {
                    // Preserve duplicate declarations written in the entry file;
                    // the checker owns that diagnostic.
                    items.push(item);
                }
            }
            Ok(Program { items })
        })();
        if result.is_err() {
            self.states.remove(&canonical);
        }
        self.budget.finish_module();
        result
    }

    fn load_module(&mut self, path: &Path, span: Span) -> KuResult<Arc<ModuleExports>> {
        let canonical = canonical_file(path, span)?;
        if self.states.get(&canonical) == Some(&LoadState::Visiting) {
            return Err(KuError::runtime(
                format!("circular import detected at {}", canonical.display()),
                span,
            ));
        }
        if let Some(module) = self.modules.get(&canonical) {
            return Ok(Arc::clone(module));
        }
        if !self.module_ids.contains_key(&canonical) {
            self.next_module_id = self.next_module_id.checked_add(1).ok_or_else(|| {
                import_limit_error("module_limit", "module identity counter overflowed", span)
            })?;
            self.module_ids
                .insert(canonical.clone(), self.next_module_id);
        }
        self.budget.begin_module(span)?;
        self.states.insert(canonical.clone(), LoadState::Visiting);
        let result = (|| {
            let source = read_import_source(&canonical, span)?;
            self.budget.charge_source(source.len(), span)?;
            let program = parse_source(&source).map_err(|err| {
                err.with_diagnostic_context(canonical.display().to_string(), source.clone())
            })?;
            let (expanded, material, exports, mut closure) = self
                .expand_program(&canonical, program, false, source.len())
                .map_err(|err| {
                    err.with_diagnostic_context(canonical.display().to_string(), source.clone())
                })?;
            let module_id = self.module_ids[&canonical];
            let mut check_items = Vec::new();
            let mut check_names = HashSet::new();
            for dependency_id in closure.iter() {
                let dependency_material = self.materialized_materials[dependency_id - 1];
                self.budget.charge_clone(dependency_material, span)?;
                for item in &self.materialized_modules[dependency_id - 1] {
                    push_materialized_item(&mut check_items, &mut check_names, item.clone());
                }
            }
            self.budget.charge_clone(material, span)?;
            for item in expanded.items.iter().cloned() {
                if matches!(&item, Item::Module(module) if module.name.starts_with("std:")) {
                    push_materialized_item(&mut check_items, &mut check_names, item);
                } else {
                    check_items.push(item);
                }
            }
            check_library_program(&Program { items: check_items }).map_err(|err| {
                err.with_diagnostic_context(canonical.display().to_string(), source.clone())
            })?;
            let dependency_snapshot =
                package::freeze_lock_dependency(&canonical, source.as_bytes())?;
            closure.insert(module_id);
            let module = Arc::new(ModuleExports {
                path: canonical.clone(),
                exports,
                closure,
            });
            let mut module_items = Vec::new();
            for item in expanded.items {
                if matches!(&item, Item::Module(module) if module.name.starts_with("std:")) {
                    module_items.push(item);
                } else {
                    let Some(name) = item_top_level_name(&item) else {
                        continue;
                    };
                    if matches!(&item, Item::Module(_)) {
                        continue;
                    }
                    if self.materialized_names.insert(name) {
                        module_items.push(item);
                    }
                }
            }
            if self.materialized_modules.len() < module_id {
                self.materialized_modules.resize_with(module_id, Vec::new);
                self.materialized_materials
                    .resize(module_id, ExpandedModuleMaterial::new(0));
            }
            self.materialized_modules[module_id - 1] = module_items;
            self.materialized_materials[module_id - 1] = material;
            self.materialized_order.push(module_id);
            self.states.insert(canonical.clone(), LoadState::Done);
            self.modules.insert(canonical.clone(), Arc::clone(&module));
            self.dependency_snapshots.push(dependency_snapshot);
            Ok(module)
        })();
        if result.is_err() {
            self.states.remove(&canonical);
        }
        self.budget.finish_module();
        result
    }

    fn expand_program(
        &mut self,
        path: &Path,
        program: Program,
        is_entry: bool,
        source_bytes: usize,
    ) -> KuResult<(
        Program,
        ExpandedModuleMaterial,
        BTreeMap<String, String>,
        ModuleClosure,
    )> {
        let mut items = Vec::new();
        let mut material = ExpandedModuleMaterial::new(source_bytes);
        let mut namespace_maps = HashMap::new();
        let local_names = top_level_names(&program);
        let mut imported_names = HashSet::new();
        let own_renames = if is_entry {
            HashMap::new()
        } else {
            let module_id = self.module_ids.get(path).copied().ok_or_else(|| {
                import_limit_error(
                    "module_identity",
                    format!("module identity was not interned for {}", path.display()),
                    Span::default(),
                )
            })?;
            program
                .items
                .iter()
                .filter_map(item_export_name)
                .map(|name| {
                    let canonical = format!("__ku_import{module_id}_{name}");
                    (name, canonical)
                })
                .collect()
        };
        let mut reference_renames = own_renames.clone();
        let mut exports = BTreeMap::new();
        let mut closure = ModuleClosure::new();
        for item in &program.items {
            let Some(name) = item_export_name(item) else {
                continue;
            };
            if is_exported_name(&name) {
                exports.insert(
                    name.clone(),
                    own_renames.get(&name).cloned().unwrap_or(name),
                );
            }
        }

        for item in &program.items {
            let Item::Import(import) = item else {
                continue;
            };
            self.budget.charge_import_edge(import.span)?;
            if let Some(modules) = std_import_modules(import)? {
                self.budget
                    .charge_import_bindings(modules.len(), import.span)?;
                for module in modules {
                    if local_names.contains(&module) || !imported_names.insert(module.clone()) {
                        return Err(KuError::runtime(
                            format!(
                                "import namespace '{module}' conflicts with another top-level name"
                            ),
                            import.span,
                        ));
                    }
                    items.push(Item::Module(ModuleDecl {
                        name: format!("std:{module}"),
                        span: import.span,
                    }));
                    material.add_item();
                }
                continue;
            }
            let import_path = resolve_import_path(
                path,
                &import.path,
                import.span,
                self.package.as_ref(),
                &self.package_import_scopes,
            )?;
            let module = self.load_module(&import_path, import.span)?;
            closure.union_with(&module.closure);
            match &import.kind {
                ImportKind::Named(names) => {
                    self.budget
                        .charge_import_bindings(names.len(), import.span)?;
                    let mut seen_sources = HashSet::new();
                    let mut seen_locals = HashSet::new();
                    for name in names {
                        if !seen_sources.insert(name.source.clone()) {
                            return Err(KuError::runtime(
                                format!("duplicate import name '{}'", name.source),
                                name.span,
                            ));
                        }
                        let local = name.local_name().to_string();
                        if !seen_locals.insert(local.clone()) {
                            return Err(KuError::runtime(
                                format!("duplicate import alias '{local}'"),
                                name.span,
                            ));
                        }
                        if local_names.contains(&local) || !imported_names.insert(local.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{local}' conflicts with another top-level name"
                                ),
                                name.span,
                            ));
                        }
                        let canonical =
                            module.exports.get(&name.source).cloned().ok_or_else(|| {
                                KuError::runtime(
                                    format!(
                                        "'{}' is not exported by {}",
                                        name.source,
                                        module.path.display()
                                    ),
                                    name.span,
                                )
                            })?;
                        reference_renames.insert(local.clone(), canonical.clone());
                    }
                }
                ImportKind::Glob => {
                    self.budget
                        .charge_import_bindings(module.exports.len(), import.span)?;
                    for (name, canonical) in &module.exports {
                        if local_names.contains(name) || !imported_names.insert(name.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{name}' conflicts with another top-level name"
                                ),
                                import.span,
                            ));
                        }
                        reference_renames.insert(name.clone(), canonical.clone());
                    }
                }
                ImportKind::Namespace(namespace) => {
                    if local_names.contains(namespace) || !imported_names.insert(namespace.clone())
                    {
                        return Err(KuError::runtime(
                            format!("import namespace '{namespace}' conflicts with another top-level name"),
                            import.span,
                        ));
                    }
                    self.budget
                        .charge_import_bindings(module.exports.len(), import.span)?;
                    let mut map = BTreeMap::new();
                    for (name, canonical) in &module.exports {
                        map.insert(name.clone(), canonical.clone());
                    }
                    namespace_maps.insert(namespace.clone(), map);
                }
            }
        }

        for item in program.items {
            if matches!(item, Item::Import(_)) {
                continue;
            }
            let span = match &item {
                Item::Import(decl) => decl.span,
                Item::Function(decl) => decl.span,
                Item::Struct(decl) => decl.span,
                Item::Enum(decl) => decl.span,
                Item::Module(decl) => decl.span,
            };
            self.budget.charge_item(span)?;
            let item = rewrite_top_level_names_in_item(item, &reference_renames, &namespace_maps)?;
            // Do not deduplicate source declarations: duplicate declarations in
            // one file must still reach the checker and produce its diagnostic.
            items.push(item);
            material.add_item();
        }
        debug_assert_eq!(items.len(), material.items);
        Ok((Program { items }, material, exports, closure))
    }
}

fn read_import_source(path: &Path, span: Span) -> KuResult<String> {
    let file = fs::File::open(path).map_err(|err| {
        KuError::runtime(
            format!("failed to read import '{}': {err}", path.display()),
            span,
        )
    })?;
    let mut source = String::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_string(&mut source)
        .map_err(|err| {
            KuError::runtime(
                format!("failed to read import '{}': {err}", path.display()),
                span,
            )
        })?;
    if source.len() as u64 > MAX_SOURCE_BYTES {
        return Err(KuError::runtime(
            format!(
                "source file too large: {} bytes exceeds {} bytes",
                source.len(),
                MAX_SOURCE_BYTES
            ),
            span,
        ));
    }
    Ok(source)
}

fn check_library_program(program: &Program) -> KuResult<()> {
    let mut program = program.clone();
    if !program
        .items
        .iter()
        .any(|item| matches!(item, Item::Function(function) if function.name == "main"))
    {
        program.items.push(Item::Function(FnDecl {
            name: "main".to_string(),
            is_async: false,
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: None,
            body: Vec::new(),
            span: Span::default(),
        }));
    }
    Checker::new().check(&program)
}

fn std_import_modules(import: &ImportDecl) -> KuResult<Option<Vec<String>>> {
    if import.path == "std" {
        let ImportKind::Named(names) = &import.kind else {
            return Err(KuError::runtime(
                "std root imports must use named form, for example import { fs, http } from \"std\"",
                import.span,
            ));
        };
        let mut modules = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if name.alias.is_some() {
                return Err(KuError::runtime(
                    "std root imports do not support aliases yet",
                    name.span,
                ));
            }
            if !stdlib::metadata::is_std_module(&name.source) {
                return Err(KuError::runtime(
                    format!("unknown std module '{}'", name.source),
                    name.span,
                ));
            }
            if !seen.insert(name.source.clone()) {
                return Err(KuError::runtime(
                    format!("duplicate std module import '{}'", name.source),
                    name.span,
                ));
            }
            modules.push(name.source.clone());
        }
        return Ok(Some(modules));
    }
    let module = if let Some(module) = import.path.strip_prefix("std.") {
        module
    } else {
        return Ok(None);
    };
    if !stdlib::metadata::is_std_module(module) {
        return Err(KuError::runtime(
            format!("unknown std module '{}'", import.path),
            import.span,
        ));
    }
    match &import.kind {
        ImportKind::Namespace(namespace) if namespace == module => Ok(Some(vec![module.to_string()])),
        ImportKind::Namespace(_) => Err(KuError::runtime(
            format!(
                "std module '{}' must be imported as '{}'",
                import.path, module
            ),
            import.span,
        )),
        ImportKind::Glob => Ok(Some(vec![module.to_string()])),
        ImportKind::Named(_) => Err(KuError::runtime(
            "std module imports must use namespace form, for example import http from \"std.http\", or shorthand import \"std.http\"",
            import.span,
        )),
    }
}

fn reject_large_file(path: &Path, span: Span) -> KuResult<()> {
    let metadata = fs::metadata(path).map_err(|err| {
        KuError::runtime(format!("failed to read '{}': {err}", path.display()), span)
    })?;
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(KuError::runtime(
            format!(
                "source file too large: {} bytes exceeds {} bytes",
                metadata.len(),
                MAX_SOURCE_BYTES
            ),
            span,
        ));
    }
    Ok(())
}

fn canonical_file(path: &Path, span: Span) -> KuResult<PathBuf> {
    fs::canonicalize(path).map_err(|err| {
        KuError::runtime(
            format!("failed to resolve '{}': {err}", path.display()),
            span,
        )
    })
}

fn resolve_import_path(
    current_file: &Path,
    import_path: &str,
    span: Span,
    package: Option<&PackageContext>,
    package_import_scopes: &[package::PackageImportScope],
) -> KuResult<PathBuf> {
    if package.is_some() {
        package::validate_package_import_text(import_path, span)?;
    }
    let raw = Path::new(import_path);
    let current_scope = package
        .map(|_| package::package_import_scope_for_file(package_import_scopes, current_file, span))
        .transpose()?;
    if let Some(current_scope) = current_scope {
        if let Some(path) = package::resolve_dependency_import(
            package_import_scopes,
            current_scope,
            import_path,
            span,
        )? {
            return Ok(path);
        }
    }
    let base = if raw.is_absolute() {
        PathBuf::new()
    } else if let Some(current_scope) = current_scope {
        if import_path.starts_with("./") || import_path.starts_with("../") {
            current_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        } else {
            current_scope.import_root.clone()
        }
    } else {
        current_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    let mut path = base.join(raw);
    if path.extension().is_none() {
        path.set_extension("ku");
    }
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
    {
        return Err(KuError::runtime(
            "import path must point to a .ku file",
            span,
        ));
    }
    if let Some(scope) = current_scope {
        path = package::canonical_import_in_scope(&path, scope, span)?;
    }
    Ok(path)
}

fn top_level_names(program: &Program) -> HashSet<String> {
    program
        .items
        .iter()
        .filter_map(item_top_level_name)
        .collect()
}

fn item_top_level_name(item: &Item) -> Option<String> {
    match item {
        Item::Function(function) => Some(function.name.clone()),
        Item::Struct(decl) => Some(decl.name.clone()),
        Item::Enum(decl) => Some(decl.name.clone()),
        Item::Module(decl) => Some(decl.name.clone()),
        Item::Import(_) => None,
    }
}

fn item_export_name(item: &Item) -> Option<String> {
    match item {
        Item::Function(function) => Some(function.name.clone()),
        Item::Struct(decl) => Some(decl.name.clone()),
        Item::Enum(decl) => Some(decl.name.clone()),
        Item::Module(_) | Item::Import(_) => None,
    }
}

fn is_exported_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn push_materialized_item(
    items: &mut Vec<Item>,
    materialized_names: &mut HashSet<String>,
    item: Item,
) {
    // A source-level `module foo` is metadata for its own file and historically
    // was not copied into importers. Synthetic std modules are capabilities used
    // by imported function bodies, so they do belong to the dependency closure.
    if matches!(&item, Item::Module(module) if !module.name.starts_with("std:")) {
        return;
    }
    let Some(name) = item_top_level_name(&item) else {
        debug_assert!(matches!(item, Item::Import(_)));
        return;
    };
    if materialized_names.insert(name) {
        items.push(item);
    }
}

type NamespaceMaps = HashMap<String, BTreeMap<String, String>>;

fn rewrite_top_level_names_in_item(
    item: Item,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) -> KuResult<Item> {
    match item {
        Item::Function(mut function) => {
            if let Some(renamed) = rename_map.get(&function.name) {
                function.name = renamed.clone();
            }
            rewrite_top_level_references_in_function(&mut function, rename_map, namespaces)?;
            Ok(Item::Function(function))
        }
        Item::Struct(mut decl) => {
            if let Some(renamed) = rename_map.get(&decl.name) {
                decl.name = renamed.clone();
            }
            for field in &mut decl.fields {
                rewrite_required_type_name(&mut field.ty, rename_map, namespaces);
            }
            Ok(Item::Struct(decl))
        }
        Item::Enum(mut decl) => {
            if let Some(renamed) = rename_map.get(&decl.name) {
                decl.name = renamed.clone();
            }
            for variant in &mut decl.variants {
                for field in &mut variant.fields {
                    rewrite_required_type_name(&mut field.ty, rename_map, namespaces);
                }
            }
            Ok(Item::Enum(decl))
        }
        Item::Module(_) | Item::Import(_) => Ok(item),
    }
}

fn rewrite_type_names_in_function(
    function: &mut FnDecl,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    for param in &mut function.params {
        rewrite_optional_type_name(&mut param.ty, rename_map, namespaces);
    }
    if let Some(return_type) = &mut function.return_type {
        rewrite_type_name(return_type, rename_map, namespaces);
    }
}

fn rewrite_optional_type_name(
    ty: &mut Option<TypeName>,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    if let Some(ty) = ty {
        rewrite_type_name(ty, rename_map, namespaces);
    }
}

fn rewrite_required_type_name(
    ty: &mut Option<TypeName>,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    rewrite_optional_type_name(ty, rename_map, namespaces);
}

fn rewrite_type_name(
    ty: &mut TypeName,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) {
    match ty {
        TypeName::Array(inner) | TypeName::Result(inner) => {
            rewrite_type_name(inner, rename_map, namespaces)
        }
        TypeName::Function {
            params,
            return_type,
            ..
        } => {
            for param in params {
                rewrite_type_name(param, rename_map, namespaces);
            }
            rewrite_type_name(return_type, rename_map, namespaces);
        }
        TypeName::Union(types) => {
            for ty in types {
                rewrite_type_name(ty, rename_map, namespaces);
            }
        }
        TypeName::Custom(name) => {
            if let Some(renamed) = rename_map.get(name) {
                *name = renamed.clone();
            } else if let Some(renamed) = namespace_lookup(name, namespaces) {
                *name = renamed;
            }
        }
        TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::String | TypeName::Null => {}
    }
}

fn rewrite_top_level_references_in_function(
    function: &mut FnDecl,
    rename_map: &HashMap<String, String>,
    namespaces: &NamespaceMaps,
) -> KuResult<()> {
    rewrite_type_names_in_function(function, rename_map, namespaces);
    let mut rewriter = TopLevelReferenceRewriter::new(rename_map, namespaces);
    rewriter.push_scope();
    for param in &function.params {
        rewriter.define(&param.name);
    }
    let result = rewriter.rewrite_block(&mut function.body);
    rewriter.pop_scope();
    result
}

struct TopLevelReferenceRewriter<'a> {
    rename_map: &'a HashMap<String, String>,
    namespaces: &'a NamespaceMaps,
    scopes: Vec<HashSet<String>>,
}

impl<'a> TopLevelReferenceRewriter<'a> {
    fn new(rename_map: &'a HashMap<String, String>, namespaces: &'a NamespaceMaps) -> Self {
        Self {
            rename_map,
            namespaces,
            scopes: Vec::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashSet::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string());
        }
    }

    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|scope| scope.contains(name))
    }

    fn namespace_symbol(
        &self,
        namespace: &str,
        name: &str,
        kind: &str,
        span: Span,
    ) -> KuResult<Option<String>> {
        if self.is_local(namespace) {
            return Ok(None);
        }
        let Some(exports) = self.namespaces.get(namespace) else {
            return Ok(None);
        };
        exports.get(name).cloned().map(Some).ok_or_else(|| {
            KuError::runtime(
                format!("module '{namespace}' has no exported {kind} '{name}'"),
                span,
            )
        })
    }

    fn rewrite_block(&mut self, body: &mut [Stmt]) -> KuResult<()> {
        for stmt in body {
            self.rewrite_stmt(stmt)?;
        }
        Ok(())
    }

    fn rewrite_scoped_block(&mut self, body: &mut [Stmt]) -> KuResult<()> {
        self.push_scope();
        let result = self.rewrite_block(body);
        self.pop_scope();
        result
    }

    fn rewrite_stmt(&mut self, stmt: &mut Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name, ty, value, ..
            } => {
                if let Some(ty) = ty {
                    rewrite_type_name(ty, self.rename_map, self.namespaces);
                }
                self.rewrite_expr(value)?;
                self.define(name);
            }
            Stmt::Assign { name, value, .. } => {
                self.rewrite_expr(value)?;
                if !self.is_local(name) {
                    self.define(name);
                }
            }
            Stmt::AssignTarget { target, value, .. }
            | Stmt::CompoundAssign { target, value, .. } => {
                self.rewrite_assign_target(target)?;
                self.rewrite_expr(value)?;
            }
            Stmt::DestructureAssign { names, values, .. } => {
                for value in values {
                    self.rewrite_expr(value)?;
                }
                for name in names.iter().flatten() {
                    self.define(name);
                }
            }
            Stmt::ObjectDestructureAssign {
                bindings,
                rest,
                value,
                ..
            } => {
                self.rewrite_expr(value)?;
                for binding in bindings {
                    if let Some(default) = &mut binding.default {
                        self.rewrite_expr(default)?;
                    }
                    let local = binding.local.as_deref().unwrap_or(&binding.field);
                    self.define(local);
                }
                if let Some(local) = rest.as_ref().and_then(|rest| rest.local.as_deref()) {
                    self.define(local);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.rewrite_expr(condition)?;
                self.rewrite_scoped_block(then_branch)?;
                if !else_branch.is_empty() {
                    self.rewrite_scoped_block(else_branch)?;
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.rewrite_expr(condition)?;
                self.rewrite_scoped_block(body)?;
            }
            Stmt::For {
                name,
                iterable,
                body,
                ..
            } => {
                self.rewrite_expr(iterable)?;
                self.push_scope();
                self.define(name);
                let result = self.rewrite_block(body);
                self.pop_scope();
                result?;
            }
            Stmt::Function(function) => {
                let local_name = function.name.clone();
                self.define(&local_name);
                rewrite_type_names_in_function(function, self.rename_map, self.namespaces);
                self.push_scope();
                self.define(&local_name);
                for param in &function.params {
                    self.define(&param.name);
                }
                let result = self.rewrite_block(&mut function.body);
                self.pop_scope();
                result?;
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                ..
            } => {
                self.rewrite_scoped_block(body)?;
                if !catch_body.is_empty() {
                    self.push_scope();
                    if let Some(catch_name) = catch_name.as_deref() {
                        self.define(catch_name);
                    }
                    let result = self.rewrite_block(catch_body);
                    self.pop_scope();
                    result?;
                }
                if !finally_body.is_empty() {
                    self.rewrite_scoped_block(finally_body)?;
                }
            }
            Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
                self.rewrite_expr(value)?
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.rewrite_expr(value)?;
                }
            }
            Stmt::Expr { expr, .. } => self.rewrite_expr(expr)?,
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
        Ok(())
    }

    fn rewrite_assign_target(&mut self, target: &mut AssignTarget) -> KuResult<()> {
        match target {
            AssignTarget::Variable(_) => Ok(()),
            AssignTarget::Index { target, index } => {
                self.rewrite_expr(target)?;
                self.rewrite_expr(index)
            }
            AssignTarget::Field { target, .. } => self.rewrite_expr(target),
        }
    }

    fn rewrite_expr(&mut self, expr: &mut Expr) -> KuResult<()> {
        match &mut expr.kind {
            ExprKind::Variable(name) => {
                if !self.is_local(name) {
                    if let Some(renamed) = self.rename_map.get(name).cloned() {
                        *name = renamed;
                    }
                }
            }
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
                self.rewrite_expr(expr)?
            }
            ExprKind::Binary { left, right, .. } => {
                self.rewrite_expr(left)?;
                self.rewrite_expr(right)?;
            }
            ExprKind::Call { callee, args } => {
                self.rewrite_expr(callee)?;
                for arg in args {
                    self.rewrite_expr(arg)?;
                }
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.rewrite_expr(value)?;
                }
            }
            ExprKind::Index { target, index } => {
                self.rewrite_expr(target)?;
                self.rewrite_expr(index)?;
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Field {
                    target: enum_target,
                    name: enum_name,
                } = &mut target.kind
                {
                    if let ExprKind::Variable(namespace) = &enum_target.kind {
                        if let Some(renamed) =
                            self.namespace_symbol(namespace, enum_name, "type", target.span)?
                        {
                            target.kind = ExprKind::Variable(renamed);
                        }
                    }
                }
                let replacement = if let ExprKind::Variable(namespace) = &target.kind {
                    self.namespace_symbol(namespace, name, "symbol", expr.span)?
                } else {
                    None
                };
                if let Some(renamed) = replacement {
                    expr.kind = ExprKind::Variable(renamed);
                } else {
                    self.rewrite_expr(target)?;
                }
            }
            ExprKind::OptionalField { target, .. } => {
                self.rewrite_expr(target)?;
            }
            ExprKind::StructLiteral { name, fields } => {
                if let Some(renamed) = self.rename_map.get(name).cloned() {
                    *name = renamed;
                } else if let Some(renamed) = namespace_lookup(name, self.namespaces) {
                    *name = renamed;
                }
                for (_, value) in fields {
                    self.rewrite_expr(value)?;
                }
            }
            ExprKind::ObjectLiteral { fields } => {
                for (_, value) in fields {
                    self.rewrite_expr(value)?;
                }
            }
            ExprKind::Match { value, arms } => {
                self.rewrite_expr(value)?;
                for arm in arms {
                    self.push_scope();
                    self.rewrite_match_pattern(&mut arm.pattern);
                    let result = (|| {
                        if let Some(guard) = &mut arm.guard {
                            self.rewrite_expr(guard)?;
                        }
                        self.rewrite_expr(&mut arm.value)
                    })();
                    self.pop_scope();
                    result?;
                }
            }
            ExprKind::Function {
                params,
                return_type,
                body,
            } => {
                for param in params.iter_mut() {
                    if let Some(ty) = &mut param.ty {
                        rewrite_type_name(ty, self.rename_map, self.namespaces);
                    }
                }
                if let Some(return_type) = return_type {
                    rewrite_type_name(return_type, self.rename_map, self.namespaces);
                }
                self.push_scope();
                for param in params.iter() {
                    self.define(&param.name);
                }
                let result = self.rewrite_block(body);
                self.pop_scope();
                result?;
            }
            ExprKind::Literal(_) => {}
        }
        Ok(())
    }

    fn rewrite_match_pattern(&mut self, pattern: &mut MatchPattern) {
        match pattern {
            MatchPattern::Binding(name) => self.define(name),
            MatchPattern::EnumVariant {
                enum_name, fields, ..
            } => {
                if let Some(renamed) = self.rename_map.get(enum_name).cloned() {
                    *enum_name = renamed;
                } else if let Some(renamed) = namespace_lookup(enum_name, self.namespaces) {
                    *enum_name = renamed;
                }
                for field in fields {
                    self.rewrite_match_pattern(field);
                }
            }
            MatchPattern::Wildcard | MatchPattern::Literal(_) => {}
        }
    }
}

fn namespace_lookup(path: &str, namespaces: &NamespaceMaps) -> Option<String> {
    let (namespace, name) = path.split_once('.')?;
    namespaces.get(namespace)?.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_yank_has_one_cli_shape_and_bounded_arguments() {
        assert!(HELP.contains("ku package yank [path]"));
        assert!(!HELP.contains("ku yank "));
        let receipt = package::PackageYankReceipt {
            name: "math".to_string(),
            version: "1.2.3".to_string(),
            registry: "https://registry.example/v1/".to_string(),
        };
        assert_eq!(
            package_yank_success_message(&receipt),
            "package yank ok: math@1.2.3 https://registry.example/v1/"
        );

        let error = run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "yank".to_string(),
            ".".to_string(),
            "unexpected".to_string(),
        ])
        .expect_err("yank accepts at most one package path");
        assert!(error
            .to_string()
            .contains("too many arguments for 'ku package yank'"));
    }

    #[test]
    fn dependency_mode_flags_are_single_and_propagate_to_built_runner() {
        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "src/main.ku".to_string(),
            "--offline".to_string(),
        ];
        let options = parse_build_options(&args).expect("parse offline build");
        assert_eq!(options.dependency_mode, DependencyResolveMode::Offline);

        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "--native".to_string(),
            "--locked".to_string(),
            "src/main.ku".to_string(),
        ];
        let (path, mode) = parse_native_compat_args(&args).expect("parse locked native build");
        assert_eq!(path, PathBuf::from("src/main.ku"));
        assert_eq!(mode, DependencyResolveMode::Locked);

        let runner = build_runner_source("src/main.ku", "fn main() {}", mode);
        assert!(runner.contains("run_source_with_dependency_mode"));
        assert!(runner.contains("DependencyResolveMode::Locked"));

        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "--locked".to_string(),
            "--offline".to_string(),
            "src/main.ku".to_string(),
        ];
        let error = parse_build_options(&args).expect_err("conflicting modes must fail");
        assert!(error
            .to_string()
            .contains("only one of --locked or --offline"));
    }

    #[test]
    fn imported_source_snapshot_prevents_ast_lock_hash_mismatch() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-import-lock-snapshot-{}-{nonce}",
            std::process::id()
        ));
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("create import lock fixture");
        fs::write(
            dir.join(package::MANIFEST_FILE),
            b"name = \"app\"\nversion = \"0.1.0\"\nroot = \"src\"\n",
        )
        .expect("write import lock manifest");
        let dependency = src.join("value.ku");
        let original_dependency = b"fn Value(): int { return 1 }\n";
        fs::write(&dependency, original_dependency).expect("write original dependency");
        let main = src.join("main.ku");
        let main_source = "import { Value } from \"./value\"\nfn main() { println(Value()) }\n";
        fs::write(&main, main_source).expect("write import lock entry");

        let package = package::discover_from_dir(&dir)
            .expect("discover import lock package")
            .expect("import lock package exists");
        let program = parse_source(main_source).expect("parse import lock entry");
        let mut loader = ModuleLoader::new(Some(package)).expect("create module loader");
        loader
            .load_entry(&main, program, main_source.len())
            .expect("load dependency from original source bytes");
        assert_eq!(loader.dependency_snapshots.len(), 1);
        let frozen = loader.dependency_snapshots.clone();
        let expected = package::freeze_lock_dependency(&dependency, original_dependency)
            .expect("hash original parsed source");
        assert_eq!(frozen[0].cache_key, expected.cache_key);
        let package = loader.package.as_ref().expect("loader package context");
        package::write_lock_with_frozen_dependencies(package, &frozen)
            .expect("unchanged imported source writes the frozen hash");
        let original_lock = fs::read_to_string(&package.lock_path).expect("read frozen lock");
        assert!(original_lock.contains(&expected.cache_key));

        fs::write(&dependency, b"fn Value(): int { return 2 }\n")
            .expect("replace dependency after parsing");
        let err = package::write_lock_with_frozen_dependencies(package, &frozen)
            .expect_err("changed imported source must not update ku.lock");
        assert_eq!(err.code.as_deref(), Some("source_changed"));
        assert_eq!(
            fs::read_to_string(&package.lock_path).expect("read unchanged frozen lock"),
            original_lock
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_module_load_clears_transient_state_before_retry() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-import-retry-state-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create retry-state fixture");
        let dependency = dir.join("value.ku");
        fs::write(&dependency, "fn Value(): int { return \"bad\" }\n")
            .expect("write invalid dependency");
        let canonical = canonical_file(&dependency, Span::default()).expect("canonical dependency");
        let mut loader = ModuleLoader::new(None).expect("create module loader");

        let first_error = loader
            .load_module(&dependency, Span::default())
            .err()
            .expect("invalid dependency must fail checking");
        assert!(
            first_error.to_string().contains("type error"),
            "unexpected first load error: {first_error}"
        );
        assert!(!loader.states.contains_key(&canonical));
        assert!(!loader.modules.contains_key(&canonical));
        assert!(loader.materialized_order.is_empty());
        assert!(loader.dependency_snapshots.is_empty());

        fs::write(&dependency, "fn Value(): int { return 1 }\n").expect("repair dependency");
        let module = loader
            .load_module(&dependency, Span::default())
            .expect("a repaired dependency must not look circular");
        assert_eq!(
            module.exports.get("Value").map(String::as_str),
            Some("__ku_import1_Value")
        );
        assert_eq!(loader.materialized_order, vec![1]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_binding_budget_is_structured_and_hard_bounded() {
        let mut budget = ImportBudget::default();
        let error = budget
            .charge_import_bindings(MAX_IMPORT_BINDINGS + 1, Span::default())
            .expect_err("an oversized binding map must fail before allocation");
        assert_eq!(error.domain.as_deref(), Some("import"));
        assert_eq!(error.code.as_deref(), Some("binding_limit"));
        assert_eq!(budget.expanded_items, 0);
    }

    #[test]
    fn native_fs_locator_is_relative_to_executable_directory() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\app")
        } else {
            PathBuf::from("/workspace/app")
        };
        assert_eq!(
            executable_relative_locator(&root.join("bin"), &root.join("source"))
                .expect("sibling locator"),
            "../source"
        );
        assert_eq!(
            executable_relative_locator(&root.join("source"), &root.join("source"))
                .expect("same-directory locator"),
            "."
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_fs_locator_rejects_different_windows_drives() {
        let error = executable_relative_locator(Path::new(r"C:\bin"), Path::new(r"D:\source"))
            .expect_err("different drives cannot be relocatable");
        assert!(error.contains("filesystem root"));
    }

    #[test]
    fn build_target_resolver_accepts_host_and_supported_targets() {
        assert!(resolve_build_target(None)
            .expect("default target")
            .is_none());
        assert!(resolve_build_target(Some("host"))
            .expect("host target")
            .is_none());

        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("resolved linux target");
        assert_eq!(linux.slug, "x86_64-linux");
        assert_eq!(linux.rust_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(linux.c_triple, "x86_64-linux-gnu");
        assert!(!linux.is_windows);

        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("resolved windows target");
        assert_eq!(windows.rust_triple, "x86_64-pc-windows-msvc");
        assert!(windows.is_windows);
        assert_eq!(
            with_executable_extension(PathBuf::from("app"), Some(&windows)),
            PathBuf::from("app.exe")
        );

        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("resolved darwin target");
        assert_eq!(darwin.rust_triple, "aarch64-apple-darwin");
    }

    #[test]
    fn build_target_resolver_rejects_path_escape_and_unknown_targets() {
        let err = resolve_build_target(Some("../escape")).expect_err("path target must fail");
        assert!(
            err.to_string().contains("invalid build target"),
            "unexpected error: {err}"
        );

        let err = resolve_build_target(Some("wasm32-wasi")).expect_err("unknown target must fail");
        assert!(
            err.to_string().contains("unsupported build target"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn three_native_targets_emit_separate_source_free_import_graphs_without_a_linker() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-three-target-c-{}-{nonce}", std::process::id()));
        let source_dir = dir.join("source");
        let retained_dir = dir.join("retained-c");
        fs::create_dir_all(&source_dir).expect("create three-target source fixture");
        fs::create_dir_all(&retained_dir).expect("create retained C fixture");
        fs::write(
            source_dir.join("math.ku"),
            "fn Add(a:int, b:int): int { return a + b }\n",
        )
        .expect("write imported target fixture");
        let main = source_dir.join("main.ku");
        fs::write(
            &main,
            "import { Add } from \"./math.ku\"\nfn main(): null! { println(Add(20, 22)) return ok(null) }\n",
        )
        .expect("write target entry fixture");

        let mut retained = Vec::new();
        for target_name in ["x86_64-windows", "x86_64-linux", "aarch64-darwin"] {
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                "--target".to_string(),
                target_name.to_string(),
                main.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse target build");
            let plan = resolve_build_plan(&options).expect("resolve target build plan");
            fs::create_dir_all(&plan.build_dir).expect("create target build directory");
            assert_eq!(
                plan.build_dir,
                fs::canonicalize(&source_dir)
                    .expect("canonical target source fixture")
                    .join(package::DEFAULT_BUILD_DIR)
                    .join(target_name)
                    .join("debug")
            );
            assert_eq!(
                plan.output.extension().and_then(|value| value.to_str()),
                (target_name == "x86_64-windows").then_some("exe")
            );
            let c_path = write_native_c_artifact(&plan, DependencyResolveMode::Update)
                .expect("emit target-specific C without a linker");
            assert_eq!(c_path, plan.build_dir.join("c").join("main.c"));
            let c = fs::read_to_string(&c_path).expect("read target-specific C");
            assert!(
                c.lines().any(|line| {
                    line.starts_with("int64_t __ku_import")
                        && line.contains("_Add(int64_t a, int64_t b)")
                }),
                "import graph missing from {target_name} artifact"
            );
            assert!(!c.contains("run_source"));
            assert!(!c.contains("const SOURCE"));
            let retained_path = retained_dir.join(format!("{target_name}.c"));
            fs::copy(&c_path, &retained_path).expect("retain emitted C outside source tree");
            retained.push(retained_path);
        }

        fs::remove_dir_all(&source_dir).expect("remove complete Ku source tree");
        for artifact in retained {
            let c = fs::read_to_string(&artifact).expect("read retained source-free C");
            assert!(c.contains("KuResult_null ku_main()"));
            assert!(!c.contains("run_source"));
            assert!(!c.contains("const SOURCE"));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parallel_standalone_entries_keep_separate_native_c_artifacts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-parallel-native-entries-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create parallel native fixture");

        let make_plan = |name: &str, marker: &str| {
            let entry = dir.join(format!("{name}.ku"));
            fs::write(
                &entry,
                format!("fn main(): null! {{ println(\"{marker}\") return ok(null) }}\n"),
            )
            .expect("write standalone native entry");
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                entry.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse standalone native build");
            let plan = resolve_build_plan(&options).expect("resolve standalone native build");
            fs::create_dir_all(&plan.build_dir).expect("create standalone build directory");
            plan
        };

        let first = make_plan("first", "FIRST_NATIVE_ENTRY");
        let second = make_plan("second", "SECOND_NATIVE_ENTRY");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let first_handle = thread::spawn(move || {
            first_barrier.wait();
            write_native_c_artifact(&first, DependencyResolveMode::Update)
                .expect("emit first standalone native C")
        });
        let second_handle = thread::spawn(move || {
            second_barrier.wait();
            write_native_c_artifact(&second, DependencyResolveMode::Update)
                .expect("emit second standalone native C")
        });
        let first_c = first_handle.join().expect("first native build panicked");
        let second_c = second_handle.join().expect("second native build panicked");

        assert_ne!(first_c, second_c, "parallel entries must not share main.c");
        assert_eq!(
            first_c.file_name().and_then(|name| name.to_str()),
            Some("first.c")
        );
        assert_eq!(
            second_c.file_name().and_then(|name| name.to_str()),
            Some("second.c")
        );
        let first_source = fs::read_to_string(first_c).expect("read first native C");
        let second_source = fs::read_to_string(second_c).expect("read second native C");
        assert!(first_source.contains("FIRST_NATIVE_ENTRY"));
        assert!(!first_source.contains("SECOND_NATIVE_ENTRY"));
        assert!(second_source.contains("SECOND_NATIVE_ENTRY"));
        assert!(!second_source.contains("FIRST_NATIVE_ENTRY"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_file_locks_are_bounded_shared_and_released() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-build-lock-test-{}-{nonce}", std::process::id()));
        let path = dir.join("build.lock");

        let exclusive = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Exclusive,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("acquire initial exclusive build lock");
        let blocked_at = Instant::now();
        let blocked = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Shared,
            blocked_at + Duration::from_millis(40),
        );
        let error = match blocked {
            Ok(_) => panic!("an exclusive build lock must block another holder"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("build output remained busy"));
        assert!(
            blocked_at.elapsed() < Duration::from_secs(2),
            "lock contention must stop at its absolute deadline"
        );
        drop(exclusive);

        let first_shared = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Shared,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("acquire first shared build lock after release");
        let second_shared = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Shared,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("shared build locks may coexist");
        let blocked = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Exclusive,
            Instant::now() + Duration::from_millis(40),
        );
        assert!(
            blocked.is_err(),
            "a clean/exclusive lock must wait for ordinary builds"
        );
        drop(second_shared);
        drop(first_shared);

        let released = acquire_build_file_lock_until(
            &path,
            BuildLockMode::Exclusive,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("exclusive build lock must become available after all holders drop");
        drop(released);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn separate_projects_targeting_one_output_share_the_output_lock() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-shared-native-output-lock-{}-{nonce}",
            std::process::id()
        ));
        let output = dir.join("dist").join("shared-program");

        let make_plan = |project: &str| {
            let project_dir = dir.join(project);
            fs::create_dir_all(&project_dir).expect("create standalone project");
            let entry = project_dir.join("main.ku");
            fs::write(&entry, "fn main() {}\n").expect("write standalone project entry");
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                "-o".to_string(),
                output.display().to_string(),
                entry.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse shared output build");
            resolve_build_plan(&options).expect("resolve shared output build")
        };

        let first = make_plan("first");
        let second = make_plan("second");
        assert_ne!(
            first.root_lock_path, second.root_lock_path,
            "independent build trees need independent clean leases"
        );
        assert_eq!(
            first.output_lock_path, second.output_lock_path,
            "the absolute final output identity must select one global lock"
        );
        assert_ne!(
            first.native_c_output, second.native_c_output,
            "each project retains its own generated C artifact"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn output_digest_uses_complete_compact_sha256_encoding() {
        assert_eq!(encode_base64url_no_pad(b""), "");
        assert_eq!(encode_base64url_no_pad(b"f"), "Zg");
        assert_eq!(encode_base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(encode_base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(encode_base64url_no_pad(&[0xff, 0xee, 0xdd]), "_-7d");

        let digest = native_output_path_digest(Path::new("dist/app"), Path::new("/workspace"));
        assert_eq!(digest.len(), 43, "all 256 digest bits must be retained");
        assert!(digest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
    }

    #[test]
    fn parallel_explicit_outputs_with_the_same_name_keep_separate_native_c_artifacts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-parallel-native-output-dirs-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create explicit-output native fixture");

        let make_plan = |name: &str, marker: &str| {
            let entry = dir.join(format!("{name}.ku"));
            fs::write(
                &entry,
                format!("fn main(): null! {{ println(\"{marker}\") return ok(null) }}\n"),
            )
            .expect("write explicit-output native entry");
            let output = dir.join(name).join("program");
            let args = vec![
                "ku".to_string(),
                "build".to_string(),
                "--backend".to_string(),
                "c".to_string(),
                "-o".to_string(),
                output.display().to_string(),
                entry.display().to_string(),
            ];
            let options = parse_build_options(&args).expect("parse explicit-output build");
            let plan = resolve_build_plan(&options).expect("resolve explicit-output build");
            assert!(
                plan.native_c_output.starts_with(plan.build_dir.join("c")),
                "an explicit output C artifact must stay inside the isolated build tree"
            );
            plan
        };

        let first = make_plan("first", "FIRST_EXPLICIT_OUTPUT");
        let second = make_plan("second", "SECOND_EXPLICIT_OUTPUT");
        let first_expected = first.native_c_output.clone();
        let second_expected = second.native_c_output.clone();
        assert_ne!(first.ir_output, second.ir_output);
        assert_ne!(first.llvm_output, second.llvm_output);
        assert_eq!(
            first_expected
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(str::len),
            Some(43),
            "the explicit-output isolation directory keeps complete SHA-256 entropy"
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let first_handle = thread::spawn(move || {
            first_barrier.wait();
            write_native_c_artifact(&first, DependencyResolveMode::Update)
                .expect("emit first explicit-output native C")
        });
        let second_handle = thread::spawn(move || {
            second_barrier.wait();
            write_native_c_artifact(&second, DependencyResolveMode::Update)
                .expect("emit second explicit-output native C")
        });
        let first_c = first_handle.join().expect("first native build panicked");
        let second_c = second_handle.join().expect("second native build panicked");

        assert_eq!(first_c, first_expected);
        assert_eq!(second_c, second_expected);
        assert_ne!(first_c, second_c);
        assert_ne!(
            first_c.parent(),
            second_c.parent(),
            "the absolute output path must select an isolated artifact directory"
        );
        assert_eq!(
            first_c.file_name().and_then(|name| name.to_str()),
            Some("program.c")
        );
        assert_eq!(
            second_c.file_name().and_then(|name| name.to_str()),
            Some("program.c")
        );
        let first_source = fs::read_to_string(first_c).expect("read first explicit-output C");
        let second_source = fs::read_to_string(second_c).expect("read second explicit-output C");
        assert!(first_source.contains("FIRST_EXPLICIT_OUTPUT"));
        assert!(!first_source.contains("SECOND_EXPLICIT_OUTPUT"));
        assert!(second_source.contains("SECOND_EXPLICIT_OUTPUT"));
        assert!(!second_source.contains("FIRST_EXPLICIT_OUTPUT"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_main_package_entries_require_an_explicit_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-package-extra-entry-{}-{nonce}",
            std::process::id()
        ));
        let source_dir = dir.join("src");
        fs::create_dir_all(&source_dir).expect("create package entry fixture");
        fs::write(
            dir.join(package::MANIFEST_FILE),
            "name = \"entry_fixture\"\nversion = \"0.0.1\"\nroot = \"src\"\nmain = \"main.ku\"\n",
        )
        .expect("write package manifest");
        fs::write(source_dir.join("main.ku"), "fn main() {}\n").expect("write package main entry");
        let worker = source_dir.join("worker.ku");
        fs::write(&worker, "fn main() { println(\"worker\") }\n")
            .expect("write non-main package entry");

        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            worker.display().to_string(),
        ];
        let options = parse_build_options(&args).expect("parse non-main package build");
        let error = resolve_build_plan(&options)
            .expect_err("a non-main package entry without -o must not share package output")
            .to_string();
        assert!(error.contains("non-main package entry requires an explicit output path"));
        assert!(error.contains("ku build -o <output>"));

        let output = dir.join("bin").join("worker");
        let args = vec![
            "ku".to_string(),
            "build".to_string(),
            "-o".to_string(),
            output.display().to_string(),
            worker.display().to_string(),
        ];
        let options = parse_build_options(&args).expect("parse explicit non-main package build");
        let plan = resolve_build_plan(&options).expect("resolve explicit non-main package build");
        assert_eq!(
            plan.output.file_stem().and_then(|name| name.to_str()),
            Some("worker")
        );
        assert!(plan.native_c_output.starts_with(plan.build_dir.join("c")));
        assert_eq!(
            plan.native_c_output
                .file_name()
                .and_then(|name| name.to_str()),
            Some("worker.c")
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_target_binary_verification_accepts_only_matching_executables() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-target-format-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create target format fixture");

        let mut elf = vec![0u8; 0x200];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[6] = 1;
        elf[7] = 3;
        elf[16..18].copy_from_slice(&3u16.to_le_bytes());
        elf[18..20].copy_from_slice(&62u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[24..32].copy_from_slice(&0x400080u64.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[52..54].copy_from_slice(&64u16.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        elf[64..68].copy_from_slice(&1u32.to_le_bytes());
        elf[68..72].copy_from_slice(&5u32.to_le_bytes());
        elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
        elf[88..96].copy_from_slice(&0x400000u64.to_le_bytes());
        let elf_len = elf.len() as u64;
        elf[96..104].copy_from_slice(&elf_len.to_le_bytes());
        elf[104..112].copy_from_slice(&elf_len.to_le_bytes());
        elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes());
        let elf_path = dir.join("app-linux");
        fs::write(&elf_path, &elf).expect("write ELF fixture");

        let mut pe = vec![0u8; 0x400];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        pe[0x96..0x98].copy_from_slice(&0x0022u16.to_le_bytes());
        pe[0x98..0x9a].copy_from_slice(&0x020bu16.to_le_bytes());
        pe[0xa8..0xac].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xb8..0xbc].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0xbc..0xc0].copy_from_slice(&0x200u32.to_le_bytes());
        pe[0xd0..0xd4].copy_from_slice(&0x2000u32.to_le_bytes());
        pe[0xd4..0xd8].copy_from_slice(&0x200u32.to_le_bytes());
        pe[0x104..0x108].copy_from_slice(&16u32.to_le_bytes());
        let section = 0x188usize;
        pe[section..section + 5].copy_from_slice(b".text");
        pe[section + 8..section + 12].copy_from_slice(&16u32.to_le_bytes());
        pe[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 36..section + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        let pe_path = dir.join("app.exe");
        fs::write(&pe_path, &pe).expect("write PE fixture");

        let mut macho = vec![0u8; 160];
        macho[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        macho[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        macho[12..16].copy_from_slice(&2u32.to_le_bytes());
        macho[16..20].copy_from_slice(&2u32.to_le_bytes());
        macho[20..24].copy_from_slice(&96u32.to_le_bytes());
        macho[32..36].copy_from_slice(&0x19u32.to_le_bytes());
        macho[36..40].copy_from_slice(&72u32.to_le_bytes());
        macho[64..72].copy_from_slice(&0x1000u64.to_le_bytes());
        let macho_len = macho.len() as u64;
        macho[80..88].copy_from_slice(&macho_len.to_le_bytes());
        macho[92..96].copy_from_slice(&5u32.to_le_bytes());
        macho[104..108].copy_from_slice(&0x32u32.to_le_bytes());
        macho[108..112].copy_from_slice(&24u32.to_le_bytes());
        macho[112..116].copy_from_slice(&1u32.to_le_bytes());
        let macho_path = dir.join("app-darwin");
        fs::write(&macho_path, &macho).expect("write Mach-O fixture");

        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("explicit linux target");
        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("explicit darwin target");
        verify_native_binary_target(&elf_path, &linux).expect("matching ELF target");
        verify_native_binary_target(&pe_path, &windows).expect("matching PE target");
        verify_native_binary_target(&macho_path, &darwin).expect("matching Mach-O target");
        assert!(verify_native_binary_target(&elf_path, &windows).is_err());
        assert!(verify_native_binary_target(&pe_path, &darwin).is_err());
        assert!(verify_native_binary_target(&macho_path, &linux).is_err());

        let truncated_elf = dir.join("truncated-elf");
        fs::write(&truncated_elf, &elf[..100]).expect("write truncated ELF");
        assert!(verify_native_binary_target(&truncated_elf, &linux).is_err());
        let wrong_os_elf = dir.join("wrong-os-elf");
        elf[7] = 9;
        fs::write(&wrong_os_elf, &elf).expect("write wrong-OS ELF");
        assert!(verify_native_binary_target(&wrong_os_elf, &linux).is_err());

        let truncated_pe = dir.join("truncated.exe");
        fs::write(&truncated_pe, &pe[..0x200]).expect("write truncated PE");
        assert!(verify_native_binary_target(&truncated_pe, &windows).is_err());

        let ios_macho = dir.join("app-ios");
        macho[112..116].copy_from_slice(&2u32.to_le_bytes());
        fs::write(&ios_macho, &macho).expect("write iOS Mach-O");
        assert!(verify_native_binary_target(&ios_macho, &darwin).is_err());
        let truncated_macho = dir.join("truncated-macho");
        fs::write(&truncated_macho, &macho[..100]).expect("write truncated Mach-O");
        assert!(verify_native_binary_target(&truncated_macho, &darwin).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_target_link_staging_never_reuses_old_or_non_file_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-target-staging-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create link staging fixture");
        let final_output = dir.join("app.exe");
        let staging = temporary_link_output(&final_output);
        assert_eq!(staging.parent(), Some(dir.as_path()));
        assert_eq!(
            staging.extension().and_then(|value| value.to_str()),
            Some("exe")
        );

        fs::write(&staging, b"old artifact from an interrupted build")
            .expect("write stale link staging fixture");
        prepare_link_output_staging(&staging).expect("remove stale regular staging file");
        assert!(!staging.exists());
        prepare_link_output_staging(&staging).expect("missing staging is already clean");

        fs::create_dir(&staging).expect("create non-file staging fixture");
        let error = prepare_link_output_staging(&staging)
            .expect_err("a non-file staging path must fail closed")
            .to_string();
        assert!(error.contains("not a regular file"));
        assert!(staging.is_dir(), "preparation must not delete a directory");
        fs::remove_dir(&staging).expect("remove non-file staging fixture");

        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        assert!(
            verify_native_binary_target(&staging, &windows).is_err(),
            "a successful compiler exit without a new file must not reuse old output"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_link_cleanup_is_bounded_and_preserves_non_regular_or_recent_entries() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-stale-link-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create stale link fixture");
        let old_one = dir.join(".ku-link-1-1-1.exe");
        let old_two = dir.join(".ku-link-1-2-2.exe");
        fs::write(&old_one, b"old").expect("write old staging one");
        fs::write(&old_two, b"old").expect("write old staging two");
        std::thread::sleep(Duration::from_millis(30));
        let now = SystemTime::now();
        let recent = dir.join(".ku-link-1-3-3.exe");
        fs::write(&recent, b"recent").expect("write recent staging");
        let unrelated = dir.join(".ku-link-invalid.exe");
        fs::write(&unrelated, b"unrelated").expect("write unrelated file");
        let directory = dir.join(".ku-link-1-4-4.exe");
        fs::create_dir(&directory).expect("create matching directory");

        #[cfg(unix)]
        let symlink = {
            let path = dir.join(".ku-link-1-5-5.exe");
            std::os::unix::fs::symlink(&old_one, &path).expect("create staging symlink");
            Some(path)
        };
        #[cfg(windows)]
        let symlink = {
            let path = dir.join(".ku-link-1-5-5.exe");
            std::os::windows::fs::symlink_file(&old_one, &path)
                .ok()
                .map(|()| path)
        };

        let deleted =
            cleanup_stale_link_outputs_with_policy(&dir, now, Duration::from_millis(20), 256, 1)
                .expect("clean stale staging");
        assert_eq!(deleted, 1, "deletion cap must be enforced");
        assert_eq!(
            usize::from(old_one.exists()) + usize::from(old_two.exists()),
            1
        );
        assert!(recent.exists(), "recent staging must remain active");
        assert!(unrelated.exists(), "non-matching names must remain");
        assert!(directory.is_dir(), "matching directories must remain");
        if let Some(symlink) = symlink {
            assert!(
                fs::symlink_metadata(symlink).is_ok(),
                "matching symlinks must remain"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_matching_host_target_links_when_a_native_toolchain_is_available() {
        let Some(target) = ["x86_64-linux", "x86_64-windows", "aarch64-darwin"]
            .into_iter()
            .map(|name| {
                resolve_build_target(Some(name))
                    .expect("supported target")
                    .expect("explicit target")
            })
            .find(BuildTarget::matches_host)
        else {
            eprintln!("skip: this host architecture has no first-stage explicit target");
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "ku-explicit-host-link-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create explicit host link fixture");
        let source = dir.join("main.c");
        fs::write(&source, "int main(void) { return 0; }\n")
            .expect("write explicit host C fixture");
        let output = with_executable_extension(dir.join("app"), Some(&target));
        match compile_c_source(
            &source,
            &output,
            Some(&target),
            BuildProfile::Debug,
            false,
            false,
        ) {
            Ok(()) => {
                verify_native_binary_target(&output, &target)
                    .expect("explicit host output must match its target");
                assert!(
                    fs::read_dir(&dir)
                        .expect("scan explicit host fixture")
                        .flatten()
                        .all(|entry| !entry.file_name().to_string_lossy().starts_with(".ku-link-")),
                    "successful explicit host link left staging behind"
                );
            }
            Err(error) if error.to_string().contains("C compiler not found") => {
                eprintln!("skip: no host C compiler available: {error}");
            }
            Err(error) => panic!("explicit matching-host target failed: {error}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn target_incompatible_native_modules_fail_before_linking() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-target-features-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create target feature fixture");
        let source = dir.join("main.c");
        fs::write(
            &source,
            "#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1\n#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1\n#if defined(_WIN32)\n#include <winsock2.h>\n#else\n#include <pthread.h>\n#include <poll.h>\n#endif\n",
        )
        .expect("write portable socket runtime fixture");
        let features = CSourceFeatures::inspect(&source).expect("inspect feature fixture");
        let linux = resolve_build_target(Some("x86_64-linux"))
            .expect("linux target")
            .expect("explicit linux target");
        let windows = resolve_build_target(Some("x86_64-windows"))
            .expect("windows target")
            .expect("explicit windows target");
        validate_c_target_features(features, Some(&linux))
            .expect("portable HTTP and Redis are valid for a Linux target");
        validate_c_target_features(features, Some(&windows))
            .expect("portable HTTP and Redis are valid for a Windows target");
        let darwin = resolve_build_target(Some("aarch64-darwin"))
            .expect("darwin target")
            .expect("explicit darwin target");
        validate_c_target_features(features, Some(&darwin))
            .expect("portable HTTP and Redis are valid for a macOS target");

        fs::write(&source, "#pragma comment(lib, \"libmysql.lib\")\n")
            .expect("write libmysql feature fixture");
        let features = CSourceFeatures::inspect(&source).expect("inspect libmysql fixture");
        let non_host = [linux.clone(), windows.clone(), darwin]
            .into_iter()
            .find(|target| !target.matches_host())
            .expect("at least one supported target differs from this host");
        let error = validate_c_target_features(features, Some(&non_host))
            .expect_err("cross-target libmysql must fail closed")
            .to_string();
        assert!(error.contains("no portable target-library contract"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_compiler_candidates_use_ku_cc_then_bounded_fallbacks() {
        let candidates = c_compiler_candidates(Some("zig cc"));
        let labels = candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["zig cc", "clang", "cc", "gcc"]);
        assert_eq!(candidates[0].program, "zig");
        assert_eq!(candidates[0].args, vec!["cc"]);
        assert_eq!(candidates[0].kind, CCompilerKind::ZigCc);
        assert!(candidates[0].explicitly_configured);
        assert!(!labels.contains(&"cl"));

        let candidates = c_compiler_candidates(Some("clang"));
        let labels = candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["clang", "zig cc", "cc", "gcc"]);
        assert_eq!(candidates[0].kind, CCompilerKind::Clang);
        assert!(candidates[0].explicitly_configured);
        assert_eq!(candidates[2].kind, CCompilerKind::Preconfigured);
        assert!(!candidates[2].explicitly_configured);
        let cross_target = ["x86_64-linux", "x86_64-windows", "aarch64-darwin"]
            .into_iter()
            .map(|name| {
                resolve_build_target(Some(name))
                    .expect("supported target")
                    .expect("explicit target")
            })
            .find(|target| !target.matches_host())
            .expect("at least one target differs from this host");
        assert!(c_compiler_supports_explicit_target(
            &candidates[0],
            &cross_target
        ));
        assert!(c_compiler_supports_explicit_target(
            &candidates[1],
            &cross_target
        ));
        assert!(!c_compiler_supports_explicit_target(
            &candidates[2],
            &cross_target
        ));
        assert!(!c_compiler_supports_explicit_target(
            &candidates[3],
            &cross_target
        ));

        let configured_gcc = parse_c_compiler_candidate("x86_64-w64-mingw32-gcc", true)
            .expect("parse configured cross gcc");
        assert_eq!(configured_gcc.kind, CCompilerKind::Preconfigured);
        assert!(c_compiler_supports_explicit_target(
            &configured_gcc,
            &cross_target
        ));
    }

    #[test]
    fn libpq_library_names_are_platform_specific() {
        for name in ["libpq.lib", "LIBPQ.LIB"] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::WindowsMsvc),
                Some(0),
                "MSVC should accept {name}"
            );
        }
        assert_eq!(
            libpq_library_name_priority("libpq.dll.a", LibpqLibraryFormat::WindowsMsvc),
            None,
            "MSVC must not accept a MinGW import archive"
        );
        assert_eq!(
            libpq_library_name_priority("libpq.dll.a", LibpqLibraryFormat::WindowsMingw),
            Some(0),
            "MinGW should accept its import archive"
        );
        assert_eq!(
            libpq_library_name_priority("libpq.lib", LibpqLibraryFormat::WindowsMingw),
            Some(1),
            "MinGW may use a COFF import library when no dll.a is available"
        );
        for (name, priority) in [("libpq.so", 0), ("libpq.so.5", 1), ("libpq.so.5.17", 1)] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Linux),
                Some(priority),
                "Linux should accept {name}"
            );
        }
        for (name, priority) in [
            ("libpq.dylib", 0),
            ("libpq.5.dylib", 1),
            ("libpq.5.17.dylib", 1),
        ] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Darwin),
                Some(priority),
                "Darwin should accept {name}"
            );
        }
        for name in ["libpq.dll", "pq.lib", "libpq.so", "libpq.a"] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::WindowsMsvc),
                None,
                "MSVC should reject {name}"
            );
        }
        for name in [
            "libpq.lib",
            "libpq.so.",
            "libpq.so.backup",
            "libpq.dylib",
            "libpq.a",
            "README",
        ] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Linux),
                None,
                "Linux should reject {name}"
            );
        }
        for name in [
            "libpq.lib",
            "libpq.so",
            "libpq.foo.dylib",
            "libpq.dylib.backup",
            "libpq.a",
            "README",
        ] {
            assert_eq!(
                libpq_library_name_priority(name, LibpqLibraryFormat::Darwin),
                None,
                "Darwin should reject {name}"
            );
        }
    }

    #[test]
    fn libpq_link_target_uses_target_os_arch_and_only_matching_host_discovery() {
        let host_platform = LibpqLibraryPlatform::host();
        let host_architecture = LibpqArchitecture::host();
        for (name, platform, architecture) in [
            (
                "x86_64-linux",
                LibpqLibraryPlatform::Linux,
                LibpqArchitecture::X86_64,
            ),
            (
                "x86_64-windows",
                LibpqLibraryPlatform::Windows,
                LibpqArchitecture::X86_64,
            ),
            (
                "aarch64-darwin",
                LibpqLibraryPlatform::Darwin,
                LibpqArchitecture::Aarch64,
            ),
        ] {
            let target = resolve_build_target(Some(name))
                .expect("supported target")
                .expect("explicit target");
            let link_target = libpq_link_target(Some(&target));
            assert_eq!(link_target.platform, platform);
            assert_eq!(link_target.architecture, architecture);
            assert_eq!(
                link_target.allow_host_discovery,
                platform == host_platform && architecture == host_architecture
            );
        }

        assert_eq!(
            libpq_link_target(None),
            LibpqLinkTarget {
                platform: host_platform,
                architecture: host_architecture,
                allow_host_discovery: true,
            }
        );
    }

    #[test]
    fn windows_libpq_format_tracks_msvc_and_mingw_compilers() {
        let msvc = CCompilerCandidate {
            label: "clang".to_string(),
            program: "clang".to_string(),
            args: Vec::new(),
            kind: CCompilerKind::Clang,
            explicitly_configured: false,
        };
        let mingw = CCompilerCandidate {
            label: "x86_64-w64-mingw32-gcc".to_string(),
            program: "x86_64-w64-mingw32-gcc".to_string(),
            args: Vec::new(),
            kind: CCompilerKind::Preconfigured,
            explicitly_configured: true,
        };
        let zig = CCompilerCandidate {
            label: "zig cc".to_string(),
            program: "zig".to_string(),
            args: vec!["cc".to_string()],
            kind: CCompilerKind::ZigCc,
            explicitly_configured: false,
        };
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &msvc),
            LibpqLibraryFormat::WindowsMsvc
        );
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &mingw),
            LibpqLibraryFormat::WindowsMingw
        );
        assert_eq!(
            libpq_library_format(LibpqLibraryPlatform::Windows, &zig),
            LibpqLibraryFormat::WindowsMingw
        );
    }

    #[test]
    fn static_std_pg_linking_fails_closed_with_actionable_help() {
        validate_libpq_link_mode(false, true).expect("non-PG static builds remain supported");
        validate_libpq_link_mode(true, false).expect("dynamic PG builds remain supported");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("ku-static-libpq-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).expect("create static libpq fixture");
        let source = dir.join("main.c");
        fs::write(&source, "#pragma comment(lib, \"libpq.lib\")\n")
            .expect("write static libpq fixture");
        let err = compile_c_source(
            &source,
            &dir.join("app"),
            None,
            BuildProfile::Debug,
            true,
            false,
        )
        .expect_err("static libpq must fail before invoking a linker");
        let message = err.to_string();
        assert!(message.contains("cannot safely link std.pg with --static"));
        assert!(message.contains("transitive libraries"));
        assert!(message.contains("omit --static"));
        assert!(message.contains("link the emitted C yourself"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn libpq_directory_discovery_requires_an_existing_library_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("ku-libpq-discovery-{}-{nonce}", std::process::id()));
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux),
            None,
            "a missing directory must not be trusted"
        );

        fs::create_dir(&dir).expect("create libpq discovery fixture");
        fs::write(dir.join("README"), b"not a library").expect("write unrelated discovery fixture");
        fs::create_dir(dir.join("libpq.so")).expect("create misleading library directory");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux),
            None,
            "a directory or unrelated file must not count as libpq"
        );

        let versioned = dir.join("libpq.so.5");
        fs::write(&versioned, b"link fixture").expect("write versioned libpq fixture");
        let archive = dir.join("libpq.a");
        fs::write(&archive, b"static fixture").expect("write static libpq fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux),
            Some(versioned.clone())
        );

        fs::remove_file(versioned).expect("remove versioned libpq fixture");
        assert_eq!(
            find_libpq_library(&dir, LibpqLibraryFormat::Linux),
            None,
            "automatic discovery must not select a static archive"
        );
        let err = libpq_dir_has_supported_library(&dir, LibpqLibraryFormat::Linux)
            .expect_err("a static-only libpq directory must fail closed");
        let message = err.to_string();
        assert!(message.contains(&archive.display().to_string()));
        assert!(message.contains("transitive libraries"));
        assert!(message.contains("shared libpq"));
        assert!(message.contains("link the emitted C yourself"));
        fs::remove_file(archive).expect("remove static libpq fixture");
        fs::remove_dir(dir.join("libpq.so")).expect("remove misleading library directory");
        fs::remove_file(dir.join("README")).expect("remove unrelated discovery fixture");
        fs::remove_dir(dir).expect("remove libpq discovery fixture");
    }

    #[test]
    fn installed_library_versions_are_sorted_numerically() {
        assert!(numeric_version_key("17") > numeric_version_key("9.6"));
        assert!(
            numeric_version_key("MySQL Server 8.0.12") > numeric_version_key("MySQL Server 5.7")
        );

        let mut dirs = vec![
            PathBuf::from(r"C:\Program Files\PostgreSQL\9.6\lib"),
            PathBuf::from(r"C:\Program Files\PostgreSQL\17\lib"),
            PathBuf::from(r"C:\Program Files\PostgreSQL\10\lib"),
        ];
        sort_install_dirs_by_version(&mut dirs);
        assert_eq!(
            dirs.last(),
            Some(&PathBuf::from(r"C:\Program Files\PostgreSQL\17\lib"))
        );
    }
}
