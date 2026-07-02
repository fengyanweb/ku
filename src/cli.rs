use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    ast::*,
    backend,
    checker::Checker,
    error::{KuError, KuResult},
    interpreter::Interpreter,
    ir,
    lexer::Lexer,
    package::{self, PackageContext},
    parser::Parser,
    span::Span,
    stdlib,
};

const KU_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SOURCE_BYTES: u64 = 1_000_000;
const INTERPRETER_STACK_SIZE: usize = 8 * 1024 * 1024;
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
  ku run                Run the nearest ku.mod package entry
  ku run <file.ku>      Run a Ku source file
  ku check              Check the nearest ku.mod package entry
  ku check <file.ku>    Check a Ku source file without running it
  ku check --json       Check nearest ku.mod package and emit JSON Lines diagnostics
  ku check --json <file.ku>
                        Check and emit JSON Lines diagnostics
  ku ir <file.ku>       Print checked Ku IR draft
  ku llvm <file.ku>     Emit prototype LLVM text IR
  ku build [file.ku]    Build a runnable executable package
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
  ku build --backend c [file.ku]
                        Build the native C prototype with a C compiler
  ku build --native <file.ku>
                        Compatibility form: emit prototype native C source beside file
  ku package gc <file.ku>
                        Remove unused package cache entries for a package
  ku version            Print version
  ku -v                 Print version
  ku -h                 Print this help
  ku -help              Print this help
  ku --help             Print this help
  ku help               Print this help

Examples:
  ku create hello
  ku create my-api --template http
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
  ku build --native examples\\function.ku
  ku package gc examples\\package\\src\\main.ku
";

pub fn run_cli(args: Vec<String>) -> Result<(), KuError> {
    match args.get(1).map(String::as_str) {
        Some("create") => run_create_command(&args),
        Some("init") => run_init_command(&args),
        Some("template") => run_template_command(&args),
        Some("run") => {
            if args.get(2).is_some_and(|arg| arg == "build") {
                eprintln!(
                    "warning[W0101]: `ku run build` is deprecated\nhelp: use `ku build` instead"
                );
                let mut build_args = vec![args[0].clone(), "build".to_string()];
                build_args.extend(args.iter().skip(3).cloned());
                return run_build_command(&build_args);
            }
            let (path, source) = source_arg_or_project(&args, "run")?;
            run_source(&path_string(&path), &source)
        }
        Some("check") => {
            if args.get(2).is_some_and(|arg| arg == "--json") {
                let (path, source) = if args.len() == 3 {
                    project_entry_source("check --json")?
                } else {
                    let path = exact_path_at(&args, "check --json", 3)?;
                    let path = PathBuf::from(path);
                    let source = read_ku_path(&path).map_err(|err| {
                        KuError::message(diagnostic_json_line(&err, &path_string(&path), ""))
                    })?;
                    (path, source)
                };
                let path_text = path_string(&path);
                parse_and_check(&path_text, &source)
                    .map(|_| ())
                    .map_err(|err| {
                        KuError::message(diagnostic_json_line(&err, &path_text, &source))
                    })
            } else {
                let (path, source) = source_arg_or_project(&args, "check")?;
                let path_text = path_string(&path);
                check_source(&path_text, &source)?;
                println!("check ok: {path_text}");
                Ok(())
            }
        }
        Some("ir") => {
            let path = exact_path(&args, "ir")?;
            let source = read_ku_file(path)?;
            let program = parse_and_check(path, &source)?;
            print!("{}", ir::lower_program(&program)?);
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
                let path = exact_path_at(&args, "build --native", 3)?;
                let source = read_ku_file(path)?;
                let output = build_native_c(path, &source)?;
                println!("native c ok: {}", output.display());
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
                    let path = exact_path_at(&args, "package gc", 3)?;
                    let package = package::discover_for_file(Path::new(path))?
                        .ok_or_else(|| KuError::message(format!("no ku.mod found for '{path}'")))?;
                    let removed = package::gc_cache(&package, 64)?;
                    println!("package gc ok: removed {removed} cache entries");
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

pub fn help_text() -> &'static str {
    HELP
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
    let manifest = project_manifest(name, template);
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

fn ret(req, res) {
    // `req` is the request object; handlers return an HttpResponse.
    return http.text("Ku HTTP OK")
}

fn main(): null! {
    app = http.service()

    app.get("/", ret)
    app.get("/index", fn(req, res) {
        return http.text("Ku HTTP 123")
    })
    app.get("/json", (req, res) => {
        return http.json({
            code: 0,
            msg: "ok",
            data: {
                path: req.path.clone(),
                now_ms: time.millis()
            }
        })
    })
    app.get("/user/{id}", (req, res) => {
        return http.json({
            code: 0,
            msg: "ok",
            data: {
                id: req.params.id.clone(),
                q: req.query["q"]?,
                method: req.method.clone()
            }
        })
    })
    app.post("/echo", (req, res) => {
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

fn main() {
    data = {
        code: 0,
        msg: "ok",
        data: { name: "Ku" }
    }
    println(json.stringify(data))
}
"#
        }
        "fs" => {
            r#"import { fs } from "std"

fn main(): null! {
    fs.write("hello.txt", "Hello Ku")
    text = fs.read("hello.txt")
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
            "help: use lowercase names like `hello`, `my-api`, or `data_tool`",
        ))
    }
}

fn is_valid_project_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn project_command_error(message: impl Into<String>, help: impl Into<String>) -> KuError {
    KuError::message(format!("{}\n{}", message.into(), help.into()))
}

fn source_arg_or_project(args: &[String], command: &str) -> Result<(PathBuf, String), KuError> {
    match args.len() {
        2 => project_entry_source(command),
        3 => {
            let path = PathBuf::from(&args[2]);
            let source = read_ku_path(&path)?;
            Ok((path, source))
        }
        _ => Err(command_error(format!(
            "too many arguments for 'ku {command}'"
        ))),
    }
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

fn exact_path_at<'a>(
    args: &'a [String],
    command: &str,
    path_index: usize,
) -> Result<&'a str, KuError> {
    if args.len() <= path_index {
        return Err(command_error(format!(
            "missing .ku file path for 'ku {command}'"
        )));
    }
    reject_extra_args(args, path_index + 1, command)?;
    Ok(args[path_index].as_str())
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
}

#[derive(Debug)]
struct BuildPlan {
    entry: PathBuf,
    source: String,
    out_root: PathBuf,
    build_dir: PathBuf,
    output: PathBuf,
}

#[derive(Clone, Copy, Debug)]
struct RunnerBuildConfig<'a> {
    profile: BuildProfile,
    target: Option<&'a str>,
    lto: bool,
    strip: bool,
    verbose: bool,
}

fn run_build_command(args: &[String]) -> Result<(), KuError> {
    let options = parse_build_options(args)?;
    let plan = resolve_build_plan(&options)?;
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

    if options.verbose {
        println!("build entry: {}", plan.entry.display());
        println!("build profile: {}", options.profile.as_str());
        println!("build directory: {}", plan.build_dir.display());
    }

    if options.emit_ir {
        let output = write_checked_ir_artifact(&plan)?;
        println!("ir ok: {}", output.display());
    }
    if options.emit_llvm {
        let output = write_llvm_ir_artifact(&plan)?;
        println!("llvm ir ok: {}", output.display());
    }

    match options.backend {
        BuildBackend::Runner => {
            if options.emit_c {
                let output = write_native_c_artifact(&plan)?;
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
                    target: options.target.as_deref(),
                    lto: options.lto,
                    strip: options.strip,
                    verbose: options.verbose,
                },
            )?;
            println!("build ok: {}", output.display());
        }
        BuildBackend::C => {
            let c_output = write_native_c_artifact(&plan)?;
            println!("native c ok: {}", c_output.display());
            compile_c_source(
                &c_output,
                &plan.output,
                options.profile,
                options.static_link,
                options.verbose,
            )?;
            println!("build ok: {}", plan.output.display());
        }
        BuildBackend::Llvm => {
            let llvm_output = write_llvm_ir_artifact(&plan)?;
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
    };

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

    Ok(options)
}

fn resolve_build_plan(options: &BuildOptions) -> Result<BuildPlan, KuError> {
    let cwd = env::current_dir()
        .map_err(|err| KuError::message(format!("failed to read current directory: {err}")))?;
    let (entry, package) = match &options.entry {
        Some(path) if path.is_dir() => {
            let package = package::discover_from_dir(path)?.ok_or_else(|| {
                KuError::message(format!(
                    "no ku.mod found for project '{}'\nhelp: run `ku build <file.ku>` or add ku.mod with name/root/main",
                    path.display()
                ))
            })?;
            (package_entry_path(&package), Some(package))
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
            (entry, package)
        }
        None => {
            let package = package::discover_from_dir(&cwd)?.ok_or_else(|| {
                KuError::message(
                    "ku build needs a .ku file or a ku.mod package in the current directory\nhelp: use `ku build <file.ku>`, or create ku.mod with name/root/main",
                )
            })?;
            (package_entry_path(&package), Some(package))
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
    let build_dir = build_profile_dir(&out_root, options.profile, options.target.as_deref());
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| build_dir.join(&package_name));
    let output = with_executable_extension(output, options.target.as_deref());

    Ok(BuildPlan {
        entry,
        source,
        out_root,
        build_dir,
        output,
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

fn build_profile_dir(out_root: &Path, profile: BuildProfile, target: Option<&str>) -> PathBuf {
    if let Some(target) = target {
        out_root.join(target).join(profile.as_str())
    } else {
        out_root.join(profile.as_str())
    }
}

fn with_executable_extension(mut path: PathBuf, target: Option<&str>) -> PathBuf {
    let needs_exe = target
        .map(|target| target.contains("windows"))
        .unwrap_or_else(|| cfg!(windows));
    if needs_exe && path.extension().is_none() {
        path.set_extension("exe");
    }
    path
}

fn is_ku_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write_checked_ir_artifact(plan: &BuildPlan) -> Result<PathBuf, KuError> {
    let entry = path_string(&plan.entry);
    let program = parse_and_check(&entry, &plan.source)?;
    let output = plan.build_dir.join("ir").join("main.ir");
    write_text_artifact(&output, format!("{}", ir::lower_program(&program)?))
}

fn write_native_c_artifact(plan: &BuildPlan) -> Result<PathBuf, KuError> {
    let output = plan.build_dir.join("c").join("main.c");
    write_native_c_to(&path_string(&plan.entry), &plan.source, &output)
}

fn write_llvm_ir_artifact(plan: &BuildPlan) -> Result<PathBuf, KuError> {
    let output = plan.build_dir.join("llvm").join("main.ll");
    write_llvm_ir_to(&path_string(&plan.entry), &plan.source, &output)
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
    check_source(path, source)?;
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
    let rust_source = build_runner_source(&embedded_path, source);
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

fn build_native_c(path: &str, source: &str) -> Result<PathBuf, KuError> {
    let output = Path::new(path).with_extension("c");
    write_native_c_to(path, source, &output)
}

fn write_native_c_to(path: &str, source: &str, output: &Path) -> Result<PathBuf, KuError> {
    let program = parse_and_expand(path, source)?;
    reject_native_async(&program)?;
    Checker::new().check(&program)?;
    let ir = ir::lower_program(&program)?;
    let c_source = backend::c::generate_c_source(&ir)?;
    write_text_artifact(output, c_source)
}

fn build_llvm_ir(path: &str, source: &str) -> Result<PathBuf, KuError> {
    let output = Path::new(path).with_extension("ll");
    write_llvm_ir_to(path, source, &output)
}

fn write_llvm_ir_to(path: &str, source: &str, output: &Path) -> Result<PathBuf, KuError> {
    let program = parse_and_expand(path, source)?;
    reject_compiled_async(
        &program,
        "LLVM text prototype does not support async/await yet; use the interpreter runtime",
    )?;
    Checker::new().check(&program)?;
    let ir = ir::lower_program(&program)?;
    let llvm_ir = backend::llvm::generate_llvm_ir(&ir)?;
    write_text_artifact(output, llvm_ir)
}

fn compile_c_source(
    source: &Path,
    output: &Path,
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
    let mut tried = Vec::new();
    let mut candidates = Vec::new();
    if let Ok(cc) = env::var("KU_CC") {
        if !cc.trim().is_empty() {
            candidates.push(cc);
        }
    }
    for fallback in ["zig", "clang", "cc", "gcc", "cl"] {
        if !candidates.iter().any(|candidate| candidate == fallback) {
            candidates.push(fallback.to_string());
        }
    }
    for candidate in candidates {
        tried.push(candidate.clone());
        let mut command = if candidate == "zig" {
            let mut command = Command::new("zig");
            command.arg("cc");
            command
        } else {
            Command::new(&candidate)
        };
        if candidate == "cl" {
            command
                .arg("/nologo")
                .arg(source)
                .arg(format!("/Fe:{}", output.display()));
            if profile != BuildProfile::Debug {
                command.arg("/O2");
            }
        } else {
            command.arg(source).arg("-std=c11").arg("-o").arg(output);
            if let Some(opt_level) = profile.rustc_opt_level() {
                command.arg(format!("-O{opt_level}"));
            }
            if static_link {
                command.arg("-static");
            }
        }
        if verbose {
            println!("c compiler command: {command:?}");
        }
        match command.status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                return Err(KuError::message(format!(
                    "native C build failed: {candidate} exited with {status}\nhelp: inspect generated source at {} or use default `ku build` until the native backend supports this program",
                    source.display()
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to run C compiler '{candidate}': {err}\nhelp: set KU_CC to clang/gcc/zig/cl, or use default `ku build`"
                )));
            }
        }
    }
    Err(KuError::message(format!(
        "C compiler not found for native build\nhelp: install clang/gcc/zig/cl, set KU_CC, or use default `ku build`; tried {}",
        tried.join(", ")
    )))
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

fn build_runner_source(path: &str, source: &str) -> String {
    let literal = raw_string_literal(source);
    format!(
        "const SOURCE: &str = {literal};\nfn main() {{\n    if let Err(err) = ku::cli::run_source({path:?}, SOURCE) {{\n        eprintln!(\"{{err}}\");\n        std::process::exit(1);\n    }}\n}}\n"
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
    if direct.exists() {
        return Ok(direct);
    }
    let deps = exe_dir.join("deps");
    let mut candidates = Vec::new();
    if let Ok(entries) = fs::read_dir(&deps) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("libku") && name.ends_with(".rlib") {
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
    parse_and_check(file, source)
        .map(|_| ())
        .map_err(|err| KuError::message(err.diagnostic(file, source)))
}

pub fn run_source(file: &str, source: &str) -> Result<(), KuError> {
    let program = parse_and_check(file, source)
        .map_err(|err| KuError::message(err.diagnostic(file, source)))?;
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
    let program = parse_and_expand(file, source)?;
    Checker::new().check(&program)?;
    Ok(program)
}

fn parse_and_expand(file: &str, source: &str) -> Result<Program, KuError> {
    let program = parse_source(source)?;
    let program = if program_has_imports(&program) {
        let path = Path::new(file);
        if !path.exists() {
            if program_has_only_std_imports(&program) {
                let mut loader = ModuleLoader::new(None);
                loader.expand_program(path, program, true)?
            } else {
                return Err(KuError::runtime(
                    "imports require a real .ku file path",
                    Span::default(),
                ));
            }
        } else {
            let package = package::discover_for_file(path)?;
            if let Some(package) = &package {
                package::ensure_cache_dir(package)?;
                package::resolve_remote_dependencies(package)?;
            }
            let mut loader = ModuleLoader::new(package);
            let program = loader.load_entry(path, program)?;
            if let Some(package) = &loader.package {
                package::write_lock_with_dependencies(package, &loader.dependency_paths)?;
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

#[derive(Clone)]
struct ModuleExports {
    path: PathBuf,
    items: Vec<Item>,
    exports: HashMap<String, Item>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Visiting,
    Done,
}

struct ModuleLoader {
    states: HashMap<PathBuf, LoadState>,
    modules: HashMap<PathBuf, ModuleExports>,
    namespace_counter: usize,
    package: Option<PackageContext>,
    dependency_paths: Vec<PathBuf>,
}

impl ModuleLoader {
    fn new(package: Option<PackageContext>) -> Self {
        Self {
            states: HashMap::new(),
            modules: HashMap::new(),
            namespace_counter: 0,
            package,
            dependency_paths: Vec::new(),
        }
    }

    fn load_entry(&mut self, path: &Path, program: Program) -> KuResult<Program> {
        let canonical = canonical_file(path, Span::default())?;
        self.states.insert(canonical.clone(), LoadState::Visiting);
        let expanded = self.expand_program(&canonical, program, true)?;
        self.states.insert(canonical, LoadState::Done);
        Ok(expanded)
    }

    fn load_module(&mut self, path: &Path, span: Span) -> KuResult<ModuleExports> {
        let canonical = canonical_file(path, span)?;
        if !self.dependency_paths.contains(&canonical) {
            self.dependency_paths.push(canonical.clone());
        }
        if self.states.get(&canonical) == Some(&LoadState::Visiting) {
            return Err(KuError::runtime(
                format!("circular import detected at {}", canonical.display()),
                span,
            ));
        }
        if let Some(module) = self.modules.get(&canonical) {
            return Ok(module.clone());
        }
        self.states.insert(canonical.clone(), LoadState::Visiting);
        reject_large_file(&canonical, span)?;
        let source = fs::read_to_string(&canonical).map_err(|err| {
            KuError::runtime(
                format!("failed to read import '{}': {err}", canonical.display()),
                span,
            )
        })?;
        let program = parse_source(&source).map_err(|err| {
            err.with_diagnostic_context(canonical.display().to_string(), source.clone())
        })?;
        let expanded = self
            .expand_program(&canonical, program, false)
            .map_err(|err| {
                err.with_diagnostic_context(canonical.display().to_string(), source.clone())
            })?;
        check_library_program(&expanded).map_err(|err| {
            err.with_diagnostic_context(canonical.display().to_string(), source.clone())
        })?;
        let exports = collect_exports(&expanded)?;
        let module = ModuleExports {
            path: canonical.clone(),
            items: expanded.items,
            exports,
        };
        self.states.insert(canonical.clone(), LoadState::Done);
        self.modules.insert(canonical, module.clone());
        Ok(module)
    }

    fn expand_program(
        &mut self,
        path: &Path,
        program: Program,
        is_entry: bool,
    ) -> KuResult<Program> {
        let mut items = Vec::new();
        let mut namespace_maps = HashMap::new();
        let local_names = top_level_names(&program);
        let mut imported_names = HashSet::new();

        for item in &program.items {
            let Item::Import(import) = item else {
                continue;
            };
            if let Some(modules) = std_import_modules(import)? {
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
                }
                continue;
            }
            let import_path =
                resolve_import_path(path, &import.path, import.span, self.package.as_ref())?;
            let module = self.load_module(&import_path, import.span)?;
            match &import.kind {
                ImportKind::Named(names) => {
                    let mut seen_sources = HashSet::new();
                    let mut seen_locals = HashSet::new();
                    let mut visible = HashSet::new();
                    let mut rename_map = HashMap::new();
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
                        module.exports.get(&name.source).ok_or_else(|| {
                            KuError::runtime(
                                format!(
                                    "'{}' is not exported by {}",
                                    name.source,
                                    module.path.display()
                                ),
                                name.span,
                            )
                        })?;
                        if local != name.source {
                            rename_map.insert(name.source.clone(), local.clone());
                        }
                        visible.insert(name.source.clone());
                    }
                    self.namespace_counter += 1;
                    let prefix = format!("__ku_import{}", self.namespace_counter);
                    let prepared = prepare_imported_module_items_with_renames(
                        &module.items,
                        &rename_map,
                        &prefix,
                        &visible,
                    )?;
                    items.extend(prepared);
                }
                ImportKind::Glob => {
                    let visible: HashSet<String> = module.exports.keys().cloned().collect();
                    for name in module.exports.keys() {
                        if local_names.contains(name) || !imported_names.insert(name.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{name}' conflicts with another top-level name"
                                ),
                                import.span,
                            ));
                        }
                    }
                    let prepared = prepare_imported_module_items(
                        &module.items,
                        &visible,
                        &mut self.namespace_counter,
                    )?;
                    items.extend(prepared);
                }
                ImportKind::Namespace(namespace) => {
                    if local_names.contains(namespace) || !imported_names.insert(namespace.clone())
                    {
                        return Err(KuError::runtime(
                            format!("import namespace '{namespace}' conflicts with another top-level name"),
                            import.span,
                        ));
                    }
                    let mut map = HashMap::new();
                    self.namespace_counter += 1;
                    let prefix = format!("__ku_ns{}_{}", self.namespace_counter, namespace);
                    let mut rename_map = HashMap::new();
                    for (name, exported) in &module.exports {
                        match exported {
                            Item::Function(function) => {
                                let renamed = format!("{prefix}_{name}");
                                map.insert(name.clone(), renamed.clone());
                                rename_map.insert(function.name.clone(), renamed);
                            }
                            Item::Struct(decl) => {
                                let renamed = format!("{prefix}_{name}");
                                map.insert(name.clone(), renamed.clone());
                                rename_map.insert(decl.name.clone(), renamed);
                            }
                            Item::Enum(decl) => {
                                let renamed = format!("{prefix}_{name}");
                                map.insert(name.clone(), renamed.clone());
                                rename_map.insert(decl.name.clone(), renamed);
                            }
                            Item::Module(_) | Item::Import(_) => {}
                        }
                    }
                    let prepared = prepare_imported_module_items_with_renames(
                        &module.items,
                        &rename_map,
                        &prefix,
                        &HashSet::new(),
                    )?;
                    items.extend(prepared);
                    namespace_maps.insert(namespace.clone(), map);
                }
            }
        }

        for item in program.items {
            if matches!(item, Item::Import(_)) {
                continue;
            }
            items.push(rewrite_namespaces_in_item(item, &namespace_maps)?);
        }
        let _ = is_entry;
        Ok(Program { items })
    }
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
) -> KuResult<PathBuf> {
    let raw = Path::new(import_path);
    if let Some(package) = package {
        if let Some(path) = package::resolve_dependency_import(package, import_path, span)? {
            return Ok(path);
        }
    }
    let base = if raw.is_absolute() {
        PathBuf::new()
    } else if let Some(package) = package {
        if import_path.starts_with("./") || import_path.starts_with("../") {
            current_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        } else {
            package.import_root.clone()
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
    if let Some(package) = package {
        package::ensure_inside_import_root(&path, package, span)?;
    }
    Ok(path)
}

fn collect_exports(program: &Program) -> KuResult<HashMap<String, Item>> {
    let mut exports = HashMap::new();
    for item in &program.items {
        let Some(name) = item_export_name(item) else {
            continue;
        };
        if is_exported_name(&name) {
            exports.insert(name, item.clone());
        }
    }
    Ok(exports)
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

fn prepare_imported_module_items(
    module_items: &[Item],
    visible_names: &HashSet<String>,
    namespace_counter: &mut usize,
) -> KuResult<Vec<Item>> {
    *namespace_counter += 1;
    let prefix = format!("__ku_import{}", namespace_counter);
    let mut rename_map = HashMap::new();
    for item in module_items {
        if let Item::Function(function) = item {
            if !visible_names.contains(&function.name) {
                rename_map.insert(function.name.clone(), format!("{prefix}_{}", function.name));
            }
        }
    }
    prepare_imported_module_items_with_renames(module_items, &rename_map, &prefix, visible_names)
}

fn prepare_imported_module_items_with_renames(
    module_items: &[Item],
    rename_map: &HashMap<String, String>,
    fallback_prefix: &str,
    visible_names: &HashSet<String>,
) -> KuResult<Vec<Item>> {
    let mut effective_renames = rename_map.clone();
    for item in module_items {
        if let Item::Function(function) = item {
            if !is_exported_name(&function.name) && !effective_renames.contains_key(&function.name)
            {
                effective_renames.insert(
                    function.name.clone(),
                    format!("{fallback_prefix}_{}", function.name),
                );
            }
        }
    }

    let mut items = Vec::new();
    for item in module_items {
        match item {
            Item::Function(function) => {
                let mut function = function.clone();
                if let Some(renamed) = effective_renames.get(&function.name) {
                    function.name = renamed.clone();
                }
                rewrite_type_names_in_function(&mut function, &effective_renames);
                for stmt in &mut function.body {
                    rewrite_function_calls_in_stmt(stmt, &effective_renames)?;
                }
                items.push(Item::Function(function));
            }
            Item::Struct(decl)
                if visible_names.contains(&decl.name)
                    || effective_renames.contains_key(&decl.name) =>
            {
                let mut decl = decl.clone();
                if let Some(renamed) = effective_renames.get(&decl.name) {
                    decl.name = renamed.clone();
                }
                for field in &mut decl.fields {
                    rewrite_required_type_name(&mut field.ty, &effective_renames);
                }
                items.push(Item::Struct(decl));
            }
            Item::Enum(decl)
                if visible_names.contains(&decl.name)
                    || effective_renames.contains_key(&decl.name) =>
            {
                let mut decl = decl.clone();
                if let Some(renamed) = effective_renames.get(&decl.name) {
                    decl.name = renamed.clone();
                }
                for variant in &mut decl.variants {
                    for field in &mut variant.fields {
                        rewrite_required_type_name(&mut field.ty, &effective_renames);
                    }
                }
                items.push(Item::Enum(decl));
            }
            Item::Module(_) | Item::Import(_) | Item::Struct(_) | Item::Enum(_) => {}
        }
    }
    Ok(items)
}

fn rewrite_type_names_in_function(function: &mut FnDecl, rename_map: &HashMap<String, String>) {
    for param in &mut function.params {
        rewrite_optional_type_name(&mut param.ty, rename_map);
    }
    if let Some(return_type) = &mut function.return_type {
        rewrite_type_name(return_type, rename_map);
    }
}

fn rewrite_optional_type_name(ty: &mut Option<TypeName>, rename_map: &HashMap<String, String>) {
    if let Some(ty) = ty {
        rewrite_type_name(ty, rename_map);
    }
}

fn rewrite_required_type_name(ty: &mut Option<TypeName>, rename_map: &HashMap<String, String>) {
    rewrite_optional_type_name(ty, rename_map);
}

fn rewrite_type_name(ty: &mut TypeName, rename_map: &HashMap<String, String>) {
    match ty {
        TypeName::Array(inner) | TypeName::Result(inner) => rewrite_type_name(inner, rename_map),
        TypeName::Union(types) => {
            for ty in types {
                rewrite_type_name(ty, rename_map);
            }
        }
        TypeName::Custom(name) => {
            if let Some(renamed) = rename_map.get(name) {
                *name = renamed.clone();
            }
        }
        TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::String | TypeName::Null => {}
    }
}

fn rewrite_function_calls_in_stmt(
    stmt: &mut Stmt,
    rename_map: &HashMap<String, String>,
) -> KuResult<()> {
    match stmt {
        Stmt::VarDecl { ty, value, .. } => {
            if let Some(ty) = ty {
                rewrite_type_name(ty, rename_map);
            }
            rewrite_function_calls_in_expr(value, rename_map)
        }
        Stmt::Assign { value, .. } | Stmt::Print { value, .. } => {
            rewrite_function_calls_in_expr(value, rename_map)
        }
        Stmt::AssignTarget { target, value, .. } => {
            rewrite_function_calls_in_assign_target(target, rename_map)?;
            rewrite_function_calls_in_expr(value, rename_map)
        }
        Stmt::CompoundAssign { target, value, .. } => {
            rewrite_function_calls_in_assign_target(target, rename_map)?;
            rewrite_function_calls_in_expr(value, rename_map)
        }
        Stmt::DestructureAssign { values, .. } => {
            for value in values {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            rewrite_function_calls_in_expr(value, rename_map)?;
            for binding in bindings {
                if let Some(default) = &mut binding.default {
                    rewrite_function_calls_in_expr(default, rename_map)?;
                }
            }
            Ok(())
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_function_calls_in_expr(condition, rename_map)?;
            for stmt in then_branch {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            for stmt in else_branch {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            rewrite_function_calls_in_expr(condition, rename_map)?;
            for stmt in body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            Ok(())
        }
        Stmt::For { iterable, body, .. } => {
            rewrite_function_calls_in_expr(iterable, rename_map)?;
            for stmt in body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            Ok(())
        }
        Stmt::Function(function) => {
            rewrite_type_names_in_function(function, rename_map);
            for stmt in &mut function.body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            Ok(())
        }
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            for stmt in body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            for stmt in catch_body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            for stmt in finally_body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            Ok(())
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => {
            rewrite_function_calls_in_expr(value, rename_map)
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => Ok(()),
        Stmt::Expr { expr, .. } => rewrite_function_calls_in_expr(expr, rename_map),
    }
}

fn rewrite_function_calls_in_assign_target(
    target: &mut AssignTarget,
    rename_map: &HashMap<String, String>,
) -> KuResult<()> {
    match target {
        AssignTarget::Variable(_) => Ok(()),
        AssignTarget::Index { target, index } => {
            rewrite_function_calls_in_expr(target, rename_map)?;
            rewrite_function_calls_in_expr(index, rename_map)
        }
        AssignTarget::Field { target, .. } => rewrite_function_calls_in_expr(target, rename_map),
    }
}

fn rewrite_function_calls_in_expr(
    expr: &mut Expr,
    rename_map: &HashMap<String, String>,
) -> KuResult<()> {
    match &mut expr.kind {
        ExprKind::Unary { expr, .. } => rewrite_function_calls_in_expr(expr, rename_map),
        ExprKind::Binary { left, right, .. } => {
            rewrite_function_calls_in_expr(left, rename_map)?;
            rewrite_function_calls_in_expr(right, rename_map)
        }
        ExprKind::Call { callee, args } => {
            if let ExprKind::Variable(name) = &mut callee.kind {
                if let Some(renamed) = rename_map.get(name) {
                    *name = renamed.clone();
                }
            }
            rewrite_function_calls_in_expr(callee, rename_map)?;
            for arg in args {
                rewrite_function_calls_in_expr(arg, rename_map)?;
            }
            Ok(())
        }
        ExprKind::Array(values) => {
            for value in values {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        ExprKind::Index { target, index } => {
            rewrite_function_calls_in_expr(target, rename_map)?;
            rewrite_function_calls_in_expr(index, rename_map)
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            rewrite_function_calls_in_expr(target, rename_map)
        }
        ExprKind::StructLiteral { name, fields } => {
            if let Some(renamed) = rename_map.get(name) {
                *name = renamed.clone();
            }
            for (_, value) in fields {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        ExprKind::Match { value, arms } => {
            rewrite_function_calls_in_expr(value, rename_map)?;
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_function_calls_in_expr(guard, rename_map)?;
                }
                rewrite_function_calls_in_expr(&mut arm.value, rename_map)?;
            }
            Ok(())
        }
        ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
            rewrite_function_calls_in_expr(expr, rename_map)
        }
        ExprKind::Function {
            params,
            return_type,
            body,
        } => {
            for param in params {
                if let Some(ty) = &mut param.ty {
                    rewrite_type_name(ty, rename_map);
                }
            }
            if let Some(return_type) = return_type {
                rewrite_type_name(return_type, rename_map);
            }
            for stmt in body {
                rewrite_function_calls_in_stmt(stmt, rename_map)?;
            }
            Ok(())
        }
        ExprKind::Literal(_) | ExprKind::Variable(_) => Ok(()),
    }
}

fn rewrite_namespaces_in_item(
    item: Item,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> KuResult<Item> {
    match item {
        Item::Function(mut function) => {
            rewrite_namespaces_in_function_types(&mut function, namespaces);
            for stmt in &mut function.body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(Item::Function(function))
        }
        other => Ok(other),
    }
}

fn rewrite_namespaces_in_stmt(
    stmt: &mut Stmt,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> KuResult<()> {
    match stmt {
        Stmt::VarDecl { ty, value, .. } => {
            if let Some(ty) = ty {
                rewrite_namespaced_type_name(ty, namespaces);
            }
            rewrite_namespaces_in_expr(value, namespaces)
        }
        Stmt::Assign { value, .. } | Stmt::Print { value, .. } => {
            rewrite_namespaces_in_expr(value, namespaces)
        }
        Stmt::AssignTarget { target, value, .. } => {
            rewrite_namespaces_in_assign_target(target, namespaces)?;
            rewrite_namespaces_in_expr(value, namespaces)
        }
        Stmt::CompoundAssign { target, value, .. } => {
            rewrite_namespaces_in_assign_target(target, namespaces)?;
            rewrite_namespaces_in_expr(value, namespaces)
        }
        Stmt::DestructureAssign { values, .. } => {
            for value in values {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            rewrite_namespaces_in_expr(value, namespaces)?;
            for binding in bindings {
                if let Some(default) = &mut binding.default {
                    rewrite_namespaces_in_expr(default, namespaces)?;
                }
            }
            Ok(())
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_namespaces_in_expr(condition, namespaces)?;
            for stmt in then_branch {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            for stmt in else_branch {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            rewrite_namespaces_in_expr(condition, namespaces)?;
            for stmt in body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        Stmt::For { iterable, body, .. } => {
            rewrite_namespaces_in_expr(iterable, namespaces)?;
            for stmt in body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        Stmt::Function(function) => {
            rewrite_namespaces_in_function_types(function, namespaces);
            for stmt in &mut function.body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            for stmt in body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            for stmt in catch_body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            for stmt in finally_body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } => {
            rewrite_namespaces_in_expr(value, namespaces)
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => Ok(()),
        Stmt::Expr { expr, .. } => rewrite_namespaces_in_expr(expr, namespaces),
    }
}

fn rewrite_namespaces_in_assign_target(
    target: &mut AssignTarget,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> KuResult<()> {
    match target {
        AssignTarget::Variable(_) => Ok(()),
        AssignTarget::Index { target, index } => {
            rewrite_namespaces_in_expr(target, namespaces)?;
            rewrite_namespaces_in_expr(index, namespaces)
        }
        AssignTarget::Field { target, .. } => rewrite_namespaces_in_expr(target, namespaces),
    }
}

fn rewrite_namespaces_in_function_types(
    function: &mut FnDecl,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    for param in &mut function.params {
        rewrite_optional_namespaced_type_name(&mut param.ty, namespaces);
    }
    if let Some(return_type) = &mut function.return_type {
        rewrite_namespaced_type_name(return_type, namespaces);
    }
}

fn rewrite_optional_namespaced_type_name(
    ty: &mut Option<TypeName>,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    if let Some(ty) = ty {
        rewrite_namespaced_type_name(ty, namespaces);
    }
}

fn rewrite_namespaced_type_name(
    ty: &mut TypeName,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    match ty {
        TypeName::Array(inner) | TypeName::Result(inner) => {
            rewrite_namespaced_type_name(inner, namespaces)
        }
        TypeName::Union(types) => {
            for ty in types {
                rewrite_namespaced_type_name(ty, namespaces);
            }
        }
        TypeName::Custom(name) => {
            if let Some(renamed) = namespace_lookup(name, namespaces) {
                *name = renamed;
            }
        }
        TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::String | TypeName::Null => {}
    }
}

fn rewrite_namespaces_in_expr(
    expr: &mut Expr,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> KuResult<()> {
    match &mut expr.kind {
        ExprKind::Unary { expr, .. } => rewrite_namespaces_in_expr(expr, namespaces),
        ExprKind::Binary { left, right, .. } => {
            rewrite_namespaces_in_expr(left, namespaces)?;
            rewrite_namespaces_in_expr(right, namespaces)
        }
        ExprKind::Call { callee, args } => {
            rewrite_namespace_enum_path(callee, namespaces)?;
            if let ExprKind::Field { target, name } = &callee.kind {
                if let ExprKind::Variable(namespace) = &target.kind {
                    if let Some(map) = namespaces.get(namespace) {
                        let renamed = map.get(name).ok_or_else(|| {
                            KuError::runtime(
                                format!("module '{namespace}' has no exported function '{name}'"),
                                callee.span,
                            )
                        })?;
                        callee.kind = ExprKind::Variable(renamed.clone());
                    }
                }
            }
            rewrite_namespaces_in_expr(callee, namespaces)?;
            for arg in args {
                rewrite_namespaces_in_expr(arg, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Array(values) => {
            for value in values {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Index { target, index } => {
            rewrite_namespaces_in_expr(target, namespaces)?;
            rewrite_namespaces_in_expr(index, namespaces)
        }
        ExprKind::Field { target: _, .. } => {
            rewrite_namespace_enum_path(expr, namespaces)?;
            if let ExprKind::Field { target, .. } = &mut expr.kind {
                rewrite_namespaces_in_expr(target, namespaces)?;
            }
            Ok(())
        }
        ExprKind::OptionalField { target, .. } => rewrite_namespaces_in_expr(target, namespaces),
        ExprKind::StructLiteral { name, fields } => {
            if let Some(renamed) = namespace_lookup(name, namespaces) {
                *name = renamed;
            }
            for (_, value) in fields {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Match { value, arms } => {
            rewrite_namespaces_in_expr(value, namespaces)?;
            for arm in arms {
                rewrite_namespaces_in_match_pattern(&mut arm.pattern, namespaces);
                if let Some(guard) = &mut arm.guard {
                    rewrite_namespaces_in_expr(guard, namespaces)?;
                }
                rewrite_namespaces_in_expr(&mut arm.value, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
            rewrite_namespaces_in_expr(expr, namespaces)
        }
        ExprKind::Function {
            params,
            return_type,
            body,
        } => {
            for param in params {
                if let Some(ty) = &mut param.ty {
                    rewrite_namespaced_type_name(ty, namespaces);
                }
            }
            if let Some(return_type) = return_type {
                rewrite_namespaced_type_name(return_type, namespaces);
            }
            for stmt in body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Literal(_) | ExprKind::Variable(_) => Ok(()),
    }
}

fn rewrite_namespaces_in_match_pattern(
    pattern: &mut MatchPattern,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    if let MatchPattern::EnumVariant {
        enum_name, fields, ..
    } = pattern
    {
        if let Some(renamed) = namespace_lookup(enum_name, namespaces) {
            *enum_name = renamed;
        }
        for field in fields {
            rewrite_namespaces_in_match_pattern(field, namespaces);
        }
    }
}

fn namespace_lookup(
    path: &str,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> Option<String> {
    let (namespace, name) = path.split_once('.')?;
    namespaces.get(namespace)?.get(name).cloned()
}

fn rewrite_namespace_enum_path(
    expr: &mut Expr,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> KuResult<()> {
    let ExprKind::Field { target, .. } = &mut expr.kind else {
        return Ok(());
    };
    let ExprKind::Field {
        target: enum_target,
        name: enum_name,
    } = &mut target.kind
    else {
        return Ok(());
    };
    let ExprKind::Variable(namespace) = &enum_target.kind else {
        return Ok(());
    };
    if let Some(map) = namespaces.get(namespace) {
        let renamed = map.get(enum_name).ok_or_else(|| {
            KuError::runtime(
                format!("module '{namespace}' has no exported type '{enum_name}'"),
                target.span,
            )
        })?;
        target.kind = ExprKind::Variable(renamed.clone());
    }
    Ok(())
}
