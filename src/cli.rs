use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    ast::*,
    checker::Checker,
    error::{KuError, KuResult},
    interpreter::Interpreter,
    lexer::Lexer,
    parser::Parser,
    span::Span,
};

const KU_VERSION: &str = "0.0.2";
const HELP: &str = "\
ku - simple, small, fast language tool

Usage:
  ku <file.ku>          Run a Ku source file
  ku run <file.ku>      Run a Ku source file
  ku check <file.ku>    Check a Ku source file without running it
  ku build <file.ku>    Build a runnable executable wrapper
  ku version            Print version
  ku -v                 Print version
  ku -h                 Print this help
  ku -help              Print this help
  ku --help             Print this help
  ku help               Print this help

Examples:
  ku run examples\\hello.ku
  ku check examples\\error.ku
  ku build examples\\hello.ku
";

pub fn run_cli(args: Vec<String>) -> Result<(), KuError> {
    match args.get(1).map(String::as_str) {
        Some("run") => {
            let path = exact_path(&args, "run")?;
            let source = read_ku_file(path)?;
            run_source(path, &source)
        }
        Some("check") => {
            let path = exact_path(&args, "check")?;
            let source = read_ku_file(path)?;
            check_source(path, &source)?;
            println!("check ok: {path}");
            Ok(())
        }
        Some("build") => {
            let path = exact_path(&args, "build")?;
            let source = read_ku_file(path)?;
            let output = build_executable(path, &source)?;
            println!("build ok: {}", output.display());
            Ok(())
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
    if !is_ku_file(path) {
        return Err(expected_ku_file(path));
    }
    fs::read_to_string(path).map_err(|e| KuError::message(format!("failed to read {path}: {e}")))
}

fn build_executable(path: &str, source: &str) -> Result<PathBuf, KuError> {
    check_source(path, source)?;
    let output = executable_output_path(path);
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
    let deps = target_dir.join("deps");
    let status = Command::new("rustc")
        .arg("--edition=2021")
        .arg(&runner)
        .arg("--extern")
        .arg(format!("ku={}", lib.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("-o")
        .arg(&output)
        .status()
        .map_err(|err| KuError::message(format!("failed to run rustc for ku build: {err}")))?;
    temp_guard.cleanup();
    if !status.success() {
        return Err(KuError::message(format!(
            "ku build failed: rustc exited with {status}"
        )));
    }
    Ok(output)
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

fn executable_output_path(path: &str) -> PathBuf {
    let mut output = Path::new(path).with_extension("");
    if cfg!(windows) {
        output.set_extension("exe");
    }
    output
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

fn expected_ku_file(path: &str) -> KuError {
    KuError::message(expected_ku_file_message(path))
}

fn expected_ku_file_message(path: &str) -> String {
    format!("expected a .ku source file, got '{path}'")
}

fn command_error(message: impl Into<String>) -> KuError {
    KuError::message(format!("{}\n\n{}", message.into(), HELP))
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
    let mut interpreter = Interpreter::with_base_dir(source_base_dir(file));
    interpreter
        .run(program)
        .map_err(|err| KuError::message(err.diagnostic(file, source)))
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
    let program = parse_source(source)?;
    let program = if program_has_imports(&program) {
        let path = Path::new(file);
        if !path.exists() {
            return Err(KuError::runtime(
                "imports require a real .ku file path",
                Span::default(),
            ));
        }
        let mut loader = ModuleLoader::new();
        loader.load_entry(path, source)?
    } else {
        program
    };
    Checker::new().check(&program)?;
    Ok(program)
}

fn program_has_imports(program: &Program) -> bool {
    program
        .items
        .iter()
        .any(|item| matches!(item, Item::Import(_)))
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
}

impl ModuleLoader {
    fn new() -> Self {
        Self {
            states: HashMap::new(),
            modules: HashMap::new(),
            namespace_counter: 0,
        }
    }

    fn load_entry(&mut self, path: &Path, source: &str) -> KuResult<Program> {
        let canonical = canonical_file(path, Span::default())?;
        self.states.insert(canonical.clone(), LoadState::Visiting);
        let program = parse_source(source)?;
        let expanded = self.expand_program(&canonical, program, true)?;
        self.states.insert(canonical, LoadState::Done);
        Ok(expanded)
    }

    fn load_module(&mut self, path: &Path, span: Span) -> KuResult<ModuleExports> {
        let canonical = canonical_file(path, span)?;
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
        let source = fs::read_to_string(&canonical).map_err(|err| {
            KuError::runtime(
                format!("failed to read import '{}': {err}", canonical.display()),
                span,
            )
        })?;
        let program = parse_source(&source)?;
        let expanded = self.expand_program(&canonical, program, false)?;
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
            let import_path = resolve_import_path(path, &import.path, import.span)?;
            let module = self.load_module(&import_path, import.span)?;
            match &import.kind {
                ImportKind::Named(names) => {
                    let mut seen = HashSet::new();
                    for name in names {
                        if !seen.insert(name) {
                            return Err(KuError::runtime(
                                format!("duplicate import name '{name}'"),
                                import.span,
                            ));
                        }
                        if local_names.contains(name) || !imported_names.insert(name.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{name}' conflicts with another top-level name"
                                ),
                                import.span,
                            ));
                        }
                        module.exports.get(name).ok_or_else(|| {
                            KuError::runtime(
                                format!("'{name}' is not exported by {}", module.path.display()),
                                import.span,
                            )
                        })?;
                        let prepared = prepare_imported_module_items(
                            &module.items,
                            &HashSet::from([name.clone()]),
                            &mut self.namespace_counter,
                        )?;
                        items.extend(prepared);
                    }
                }
                ImportKind::Glob => {
                    let visible: HashSet<String> = module.exports.keys().cloned().collect();
                    for (name, exported) in &module.exports {
                        if local_names.contains(name) || !imported_names.insert(name.clone()) {
                            return Err(KuError::runtime(
                                format!(
                                    "imported name '{name}' conflicts with another top-level name"
                                ),
                                import.span,
                            ));
                        }
                        let _ = exported;
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
                        if let Item::Function(function) = exported {
                            let renamed = format!("{prefix}_{name}");
                            map.insert(name.clone(), renamed.clone());
                            rename_map.insert(function.name.clone(), renamed);
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

fn canonical_file(path: &Path, span: Span) -> KuResult<PathBuf> {
    fs::canonicalize(path).map_err(|err| {
        KuError::runtime(
            format!("failed to resolve '{}': {err}", path.display()),
            span,
        )
    })
}

fn resolve_import_path(current_file: &Path, import_path: &str, span: Span) -> KuResult<PathBuf> {
    let raw = Path::new(import_path);
    if raw.is_absolute() {
        return Err(KuError::runtime("import path must be relative", span));
    }
    let mut path = current_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(raw);
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
                for stmt in &mut function.body {
                    rewrite_function_calls_in_stmt(stmt, &effective_renames)?;
                }
                items.push(Item::Function(function));
            }
            Item::Struct(decl) if visible_names.contains(&decl.name) => {
                items.push(Item::Struct(decl.clone()));
            }
            Item::Enum(decl) if visible_names.contains(&decl.name) => {
                items.push(Item::Enum(decl.clone()));
            }
            Item::Module(_) | Item::Import(_) | Item::Struct(_) | Item::Enum(_) => {}
        }
    }
    Ok(items)
}

fn rewrite_function_calls_in_stmt(
    stmt: &mut Stmt,
    rename_map: &HashMap<String, String>,
) -> KuResult<()> {
    match stmt {
        Stmt::VarDecl { value, .. } | Stmt::Assign { value, .. } | Stmt::Print { value, .. } => {
            rewrite_function_calls_in_expr(value, rename_map)
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
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        Stmt::Expr { expr, .. } => rewrite_function_calls_in_expr(expr, rename_map),
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
        ExprKind::Field { target, .. } => rewrite_function_calls_in_expr(target, rename_map),
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                rewrite_function_calls_in_expr(value, rename_map)?;
            }
            Ok(())
        }
        ExprKind::Function { body, .. } => {
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
        Stmt::VarDecl { value, .. } | Stmt::Assign { value, .. } | Stmt::Print { value, .. } => {
            rewrite_namespaces_in_expr(value, namespaces)
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
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        Stmt::Expr { expr, .. } => rewrite_namespaces_in_expr(expr, namespaces),
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
        ExprKind::Field { target, .. } => rewrite_namespaces_in_expr(target, namespaces),
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                rewrite_namespaces_in_expr(value, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Function { body, .. } => {
            for stmt in body {
                rewrite_namespaces_in_stmt(stmt, namespaces)?;
            }
            Ok(())
        }
        ExprKind::Literal(_) | ExprKind::Variable(_) => Ok(()),
    }
}
