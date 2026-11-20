//! Stage0 differential checks for the Ku declaration name pass.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use ku::{span::Span, stdlib::json, value::Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ku-bootstrap-names-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create private bootstrap directory");
        Self(path)
    }
}

#[test]
fn bootstrap_checks_diamond_modules_from_a_relocated_installation() {
    let temporary = Temp::new();
    let cwd = temporary.0.join("invocation");
    let compiler = temporary.0.join("compiler");
    let project = temporary.0.join("project");
    fs::create_dir(&cwd).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    for stage in ["stage1", "stage2", "stage3", "semantics"] {
        let target = compiler.join(stage);
        fs::create_dir_all(&target).unwrap();
        for entry in fs::read_dir(repo.join("bootstrap").join(stage)).unwrap() {
            let entry = entry.unwrap();
            if entry.path().extension().is_some_and(|ext| ext == "ku") {
                fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
            }
        }
    }
    for file in ["stage2/context.ku", "stage2/span.ku", "stage1/token.ku"] {
        let target = project.join(file);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(compiler.join(file), target).unwrap();
    }
    let entry = compiler.join("semantics/project_main.ku");
    let file = format!("ku-check-project{}", std::env::consts::EXE_SUFFIX);
    let built = compiler.join("semantics").join(&file);
    let executable = project.join(&file);
    let ku = Path::new(env!("CARGO_BIN_EXE_ku"));
    let source_dir = entry.parent().unwrap();
    fs::write(source_dir.join("modules.txt"), "call-witness.ku\n").unwrap();
    fs::write(
        source_dir.join("call-witness.ku"),
        "fn Value(x: int): int { return x }\nfn main() { println(Value(1)) }\n",
    )
    .unwrap();
    let interpreted = run(Command::new(ku).arg("run").arg(&entry), &cwd, 20);
    assert_success(&interpreted);
    assert_eq!(
        String::from_utf8_lossy(&interpreted.stdout).trim(),
        "MODULE|call-witness.ku"
    );
    assert_success(&run(
        Command::new(ku)
            .args(["build", "--release"])
            .arg(entry)
            .arg("-o")
            .arg(&built),
        &cwd,
        120,
    ));
    fs::rename(built, &executable).unwrap();
    fs::remove_dir_all(compiler).unwrap();
    const MANIFEST: &str = "stage2/context.ku\r\nstage2/span.ku\r\nstage1/token.ku\r\n";
    let manifest = project.join("modules.txt");
    fs::write(&manifest, MANIFEST).unwrap();
    let result = run(&mut Command::new(&executable), &cwd, 20);
    assert_success(&result);
    let rows = String::from_utf8(result.stdout).unwrap();
    let modules: Vec<_> = rows
        .lines()
        .filter(|row| row.starts_with("MODULE|"))
        .collect();
    assert_eq!(
        modules,
        [
            "MODULE|stage1/token.ku",
            "MODULE|stage2/span.ku",
            "MODULE|stage2/context.ku"
        ]
    );
    for row in [
        "IMPORT|stage2/span.ku|Token|stage1/token.ku|Token|19",
        "IMPORT|stage2/context.ku|Token|stage1/token.ku|Token|19",
        "TYPE|stage2/context.ku|Span|stage2/span.ku|7|15",
        "TYPE|stage2/context.ku|Token|stage1/token.ku|46|19",
    ] {
        assert!(
            rows.lines().any(|actual| row == actual),
            "missing {row}: {rows}"
        );
    }

    fs::write(
        project.join("std-user.ku"),
        r#"import { fs, string } from "std"
import "std.json"
import bytes from "std.bytes"
import { Scalar } from "./std-data.ku"
struct Local { data: Scalar }
fn main() {}
"#,
    )
    .unwrap();
    fs::write(
        project.join("std-data.ku"),
        "struct Scalar { value: int }\n",
    )
    .unwrap();
    fs::write(&manifest, "std-user.ku\nstd-data.ku\n").unwrap();
    let result = run(&mut Command::new(&executable), &cwd, 20);
    assert_success(&result);
    let rows = String::from_utf8(result.stdout).unwrap();
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        [
            "MODULE|std-data.ku",
            "MODULE|std-user.ku",
            "STD|std-user.ku|fs|3|1",
            "STD|std-user.ku|string|3|2",
            "STD|std-user.ku|json|4|4",
            "STD|std-user.ku|bytes|6|5",
            "IMPORT|std-user.ku|Scalar|std-data.ku|Scalar|3",
            "TYPE|std-user.ku|Scalar|std-data.ku|9|3",
        ]
    );

    for (source, diagnostic, position) in [
        ("import \"std\"\n", "std_import_shape", "1:1@0..1:13@12"),
        (
            "import { fs as disk } from \"std\"\n",
            "std_import_alias",
            "1:10@9..1:12@11",
        ),
        (
            "import { fs, fs } from \"std\"\n",
            "duplicate_std_module",
            "1:14@13..1:16@15",
        ),
        (
            "import { fs, kuvalue } from \"std\"\nfn fs() {}\n",
            "unknown_std_module",
            "1:14@13..1:21@20",
        ),
        (
            "import disk from \"std.fs\"\n",
            "std_import_namespace",
            "1:1@0..1:26@25",
        ),
        (
            "import \"std.fs\"\nstruct Save { source: fs }\n",
            "undefined_type",
            "2:23@38..2:25@40",
        ),
        (
            "import \"std.fs\"\nimport { Scalar as fs } from \"./std-data.ku\"\n",
            "import_name_conflict",
            "2:10@25..2:16@31",
        ),
    ] {
        fs::write(project.join("std-user.ku"), source).unwrap();
        let result = run(&mut Command::new(&executable), &cwd, 20);
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(&format!("|{diagnostic}|")), "{stderr}");
        assert!(stderr.trim_end().ends_with(position), "{stderr}");
    }

    fs::write(
        project.join("left.ku"),
        "import { Right } from \"./right.ku\"\nstruct Left { other: Right }\n",
    )
    .unwrap();
    fs::write(
        project.join("right.ku"),
        "import { Left } from \"./left.ku\"\nstruct Right { other: Left }\n",
    )
    .unwrap();
    for (input, diagnostic) in [
        ("stage2/context.ku\nstage2/span.ku\n", "unresolved_import"),
        ("stage1/token.ku\nstage1/token.ku\n", "ambiguous_module"),
        ("../outside.ku\n", "project_path"),
        ("bad\0name.ku\n", "project_path"),
        ("left.ku\nright.ku\n", "cyclic_import"),
    ] {
        fs::write(&manifest, input).unwrap();
        let result = run(&mut Command::new(&executable), &cwd, 20);
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(&format!("|{diagnostic}|")), "{stderr}");
        if diagnostic == "unresolved_import" {
            assert!(stderr.starts_with("stage2/context.ku: "), "{stderr}");
        }
    }

    assert_direct_call_contracts(ku, &executable, &project, &cwd);
}

fn assert_direct_call_contracts(ku: &Path, executable: &Path, project: &Path, cwd: &Path) {
    let manifest = project.join("modules.txt");
    let diagnostic = project.join("stage2/diagnostic.ku");
    let main = project.join("main.ku");
    let pristine = include_str!("../bootstrap/stage2/diagnostic.ku");
    let imported = pristine
        .replace("Span, SpanCanonical", "Span, SpanCanonical as RenderSpan")
        .replace(
            "SpanCanonical(diagnostic.span)",
            "RenderSpan(diagnostic.span)",
        );
    assert_ne!(imported, pristine);
    fs::write(&diagnostic, &imported).expect("real alias fixture");
    fs::write(
        &manifest,
        "stage2/diagnostic.ku\nstage2/span.ku\nstage1/token.ku\n",
    )
    .unwrap();
    fs::write(&main, "import { DiagnosticCanonical as _DiagnosticCanonical } from \"./stage2/diagnostic.ku\"\nfn main() {}\n").unwrap();
    assert_success(&run(Command::new(ku).arg("check").arg(&main), cwd, 20));
    let accepted = run(&mut Command::new(executable), cwd, 20);
    assert_success(&accepted);
    assert!(String::from_utf8_lossy(&accepted.stdout)
        .contains("IMPORT|stage2/diagnostic.ku|RenderSpan|stage2/span.ku|SpanCanonical|"));

    let wrong = imported.replace(
        "RenderSpan(diagnostic.span)",
        "RenderSpan(diagnostic.span, 0)",
    );
    assert_ne!(wrong, imported);
    fs::write(&diagnostic, wrong).expect("introduce wrong arity in the real module");
    let rust = run(Command::new(ku).arg("check").arg(&main), cwd, 20);
    assert_eq!(rust.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rust.stderr).contains("expects 1 arguments but got 2"));
    let rejected = run(&mut Command::new(executable), cwd, 20);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    let stale = String::from_utf8_lossy(&rejected.stderr);
    assert!(stale.starts_with("stage2/diagnostic.ku: "), "{stale}");
    assert!(
        stale.contains(
            "|call_arity|<source>|function 'RenderSpan' expects 1 arguments but got 2|28:"
        ),
        "{stale}"
    );

    const LOCAL_SHADOWS: &str = r#"
fn One(a: int): int { return a }
fn Two(a: int, b: int): int { return a + b }
fn Parameter(One: fn(int, int): int): int { return One(1, 2) }
fn Binding(): int {
    One = Two
    return One(2, 3)
}
fn Nested(): int {
    fn One(a: int, b: int): int { return a - b }
    return One(8, 1)
}
fn main() { println(Parameter(Two) + Binding() + Nested() + One(6)) }
"#;
    fs::write(&main, LOCAL_SHADOWS).expect("write same-spelled local callable cases");
    fs::write(&manifest, "main.ku\n").unwrap();
    assert_success(&run(Command::new(ku).arg("check").arg(&main), cwd, 20));
    assert_success(&run(&mut Command::new(executable), cwd, 20));

    fs::write(&main, LOCAL_SHADOWS.replace("One(6)", "One()")).unwrap();
    let rust = run(Command::new(ku).arg("check").arg(&main), cwd, 20);
    assert_eq!(rust.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rust.stderr).contains("expects 1 arguments but got 0"));
    let rejected = run(&mut Command::new(executable), cwd, 20);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        message.contains("|call_arity|<source>|function 'One' expects 1 arguments but got 0|"),
        "{message}"
    );

    fs::write(
        project.join("parameters.ku"),
        r#"
fn RequireInt(value: int): int { return value }
fn Select(value: bool | null): bool | null { return value }
fn Ratio(value: float): float { return value }
"#,
    )
    .unwrap();
    fs::write(&manifest, "main.ku\nparameters.ku\n").unwrap();
    let scalar_calls = r#"
import { RequireInt as Amount, Select, Ratio } from "./parameters.ku"
fn Use(value: str): str { return value }
fn NameShadow(Amount: fn(str): str): str { return Amount("local") }
fn main() {
    a = Amount(-7)
    b = Select(null)
    c = Select(false)
    d = Ratio(-1.5)
    println(NameShadow(Use) + str(a) + str(b) + str(c) + str(d))
}
"#;
    fs::write(&main, scalar_calls).unwrap();
    assert_success(&run(Command::new(ku).arg("check").arg(&main), cwd, 20));
    assert_success(&run(&mut Command::new(executable), cwd, 20));
    for (original, replacement, argument, expected) in [
        ("Amount(-7)", "Amount(\"bad\")", "\"bad\"", "int"),
        ("Select(null)", "Select(0)", "0", r"bool \p null"),
        ("Ratio(-1.5)", "Ratio(-9)", "-9", "float"),
    ] {
        let source = scalar_calls.replace(original, replacement);
        fs::write(&main, &source).unwrap();
        let stage0 = run(Command::new(ku).arg("check").arg(&main), cwd, 20);
        assert_eq!(stage0.status.code(), Some(1));
        let rejected = run(&mut Command::new(executable), cwd, 20);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        let diagnostic = String::from_utf8(rejected.stderr).unwrap();
        assert!(
            diagnostic.starts_with("main.ku: ") && diagnostic.contains("|call_argument_type|"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains(&format!("expected {expected} but got ")),
            "{diagnostic}"
        );
        let position = source.find(replacement).unwrap() + replacement.find(argument).unwrap();
        assert!(diagnostic.contains(&format!("@{position}..")));
        assert!(diagnostic
            .trim_end()
            .ends_with(&format!("@{}", position + argument.len())));
    }

    let pattern_shadow = r#"
import { RequireInt as Amount } from "./parameters.ku"
struct Options { transform: fn(str): int }
fn Count(text: str): int { return text.len() }
fn Run(options: Options): int {
    {transform: Amount} = options
    return Amount("local")
}
fn main() { println(Run(Options { transform: Count })) }
"#;
    fs::write(&main, pattern_shadow)
        .expect("object pattern binds a local callable over the import");
    assert_success(&run(Command::new(ku).arg("check").arg(&main), cwd, 20));
    assert_success(&run(&mut Command::new(executable), cwd, 20));

    let return_contract = r#"
fn Failed(): int! { fail "recoverable" }
fn Decide(ready: bool): int | null {
    fn Display(): str { return "selected" }
    if (ready) { println(Display()); return 0 }
    try { Failed()? } catch (issue) { return null }
    return -7
}
fn main(): null! { println(Decide(true)); return ok(null) }
"#;
    fs::write(&main, return_contract).unwrap();
    assert_success(&run(Command::new(ku).arg("check").arg(&main), cwd, 20));
    assert_success(&run(&mut Command::new(executable), cwd, 20));
    for (original, replacement, literal, expectation) in [
        (
            "return 0",
            "return \"not an int\"",
            "\"not an int\"",
            r"int \p null",
        ),
        ("return null", "return false", "false", r"int \p null"),
        ("return \"selected\"", "return 3", "3", "str"),
    ] {
        let program = return_contract.replace(original, replacement);
        fs::write(&main, &program).unwrap();
        let stage0 = run(Command::new(ku).arg("check").arg(&main), cwd, 20);
        assert_eq!(stage0.status.code(), Some(1));
        let stage1 = run(&mut Command::new(executable), cwd, 20);
        assert_eq!(stage1.status.code(), Some(1));
        assert!(stage1.stdout.is_empty());
        let diagnostic = String::from_utf8(stage1.stderr).unwrap();
        assert!(
            diagnostic.contains(&format!(
                "|return_type|<source>|expected {expectation} but got "
            )),
            "{diagnostic}"
        );
        let offset = program.find(replacement).unwrap() + replacement.find(literal).unwrap();
        assert!(diagnostic.contains(&format!("@{offset}..")));
        assert!(diagnostic
            .trim_end()
            .ends_with(&format!("@{}", offset + literal.len())));
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(command: &mut Command, cwd: &Path, seconds: u64) -> BoundedOutput {
    command.current_dir(cwd);
    run_bounded(
        command,
        Duration::from_secs(seconds),
        OutputLimits::new(256 * 1024, 512 * 1024),
    )
    .expect("bounded bootstrap process")
}

fn assert_success(output: &BoundedOutput) {
    assert!(
        output.status.success(),
        "bootstrap failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn offset(source: &str, line: usize, column: usize) -> usize {
    let mut lines = source.split_inclusive('\n');
    let prefix: usize = (&mut lines).take(line - 1).map(str::len).sum();
    let line_text = lines.next().expect("source line from Stage0 diagnostic");
    prefix
        + line_text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(line_text.len()))
            .nth(column - 1)
            .expect("source column from Stage0 diagnostic")
}

fn stage0_detail(source: &str, result: &BoundedOutput, lexical: bool) -> String {
    assert_eq!(result.status.code(), Some(1));
    let document = String::from_utf8(result.stderr.clone()).expect("UTF-8 from Stage0");
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(document)], Span::default())
            .expect("Stage0 diagnostic parse")
    else {
        panic!("expected one JSON diagnostic from Stage0");
    };
    let Value::Object(diagnostic) = *value else {
        panic!("expected a Stage0 diagnostic object");
    };
    let coordinate = |field: &str| {
        let Value::Int(coord) = diagnostic[field] else {
            panic!("Stage0 coordinate {field}")
        };
        usize::try_from(coord).expect("nonnegative Stage0 coordinate")
    };
    let (line, column) = (coordinate("line"), coordinate("column"));
    let Value::String(message) = &diagnostic["message"] else {
        panic!("Stage0 message")
    };
    let start = offset(source, line, column);
    if lexical {
        return format!("{line}:{column}@{start}: {message}");
    }
    let (end_line, end_column) = (coordinate("endLine"), coordinate("endColumn"));
    let end = offset(source, end_line, end_column);
    format!("{message}|{line}:{column}@{start}..{end_line}:{end_column}@{end}")
}

const SPAN_EXPECTED: &str = concat!(
    "IMPORTS|1\n",
    "NAME|Struct|Span|15|5:1@193..12:2@312\n",
    "NAME|Function|SpanFromToken|39|14:1@314..23:2@573\n",
    "NAME|Function|SpanMerge|65|25:1@575..34:2@838\n",
    "NAME|Function|SpanCanonical|109|36:1@840..38:2@1039\n",
);

const OWNERSHIP: &str = r#"import { AstCanonical } from "../stage2/ast.ku"
import { Parse } from "../stage2/parser.ku"
import { ParseProgram } from "../stage3/parser.ku"
import { BuildNameTable, TopLevelName } from "./names.ku"

fn KeptName(): TopLevelName! {
    graph = ParseProgram("import { Item } from \"./missing.ku\"\nstruct Value { text: str }\nfn main() {}")?
    original = AstCanonical(graph.clone())
    named = BuildNameTable(graph)?
    table = named.names
    graph = named.parsed
    if (AstCanonical(graph.clone()) != original) { panic("name collection changed the AST") }
    arena = graph.arena
    nodes = arena.nodes
    import_node = nodes[table.imports[0] - 1].clone()
    if (import_node.kind != "Import" || import_node.text != "./missing.ku") { panic("lost deferred import NodeId") }
    return ok(table.declarations[0].clone())
}

fn main(): null! {
    entry = KeptName()?
    if (entry.name != "Value" || entry.kind != "Struct" || entry.span.line != 2) { panic("lost independently owned name") }
    caught = false
    try { BuildNameTable(Parse("1")?)? } catch(err) { caught = err.code == "invalid_module_tree" }
    if (!caught) { panic("expected rejection of an expression-root arena") }
    malformed = ParseProgram("fn main() {}")?
    malformed.root = 0
    caught = false
    try { BuildNameTable(malformed)? } catch(err) { caught = err.domain == "bootstrap.ast" && err.code == "invalid_root" }
    if (!caught) { panic("expected arena validation before name binding") }
    println("name-owner-ok")
    return ok(null)
}
"#;

#[test]
fn bootstrap_name_table_matches_stage0_and_keeps_graph_ownership() {
    let temporary = Temp::new();
    let cwd = temporary.0.join("invocation");
    let source = temporary.0.join("compiler");
    let relocated = temporary.0.join("relocated");
    for directory in [&cwd, &source, &relocated] {
        fs::create_dir(directory).expect("create isolated consumer directory");
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    for stage in ["stage1", "stage2", "stage3", "semantics"] {
        let target = source.join(stage);
        fs::create_dir(&target).expect("create copied compiler stage");
        for entry in fs::read_dir(repo.join("bootstrap").join(stage)).expect("read compiler stage")
        {
            let entry = entry.expect("read compiler stage file");
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "ku")
            {
                fs::copy(entry.path(), target.join(entry.file_name()))
                    .expect("copy Ku stage source");
            }
        }
    }
    let compiler = source.join("semantics");
    let input = compiler.join("input.ku");
    let entry = compiler.join("main.ku");
    let ownership_entry = compiler.join("ownership.ku");
    fs::write(&ownership_entry, OWNERSHIP).expect("write ownership witness");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));

    let library = include_str!("../bootstrap/stage2/span.ku").replace("\r\n", "\n");
    let namespace =
        "module __ku_allowed_module\nfn __ku_ns_internal(){}\nfn __ku_import42(){}\nfn main(){}";
    let deferred = "import { Item } from \"./missing.ku\"\nfn main() { Item() }";
    let errors = [
        (
            "duplicate_name",
            "// 前😀\r\nfn Shared() {}\r\nstruct Shared {}\r\nfn main() {}",
        ),
        (
            "duplicate_name",
            "module Shape\nstruct Shape {}\nfn main() {}",
        ),
        ("reserved_name", "fn __ku_named() {}\nfn main() {}"),
        ("reserved_name", "fn main(&__ku_arg: int) {}"),
        (
            "reserved_name",
            "struct Row { __ku_field: int }\nfn main() {}",
        ),
        ("reserved_name", "enum Flag { __ku_tag }\nfn main() {}"),
        (
            "reserved_name",
            "enum Flag { Tag(__ku_field: int) }\nfn main() {}",
        ),
        ("lexical", "// 前😀\r\nfn main() { @ }"),
    ];
    let mut expected_errors = Vec::new();
    for (code, text) in errors {
        fs::write(&input, text).expect("write isolated Stage0 fixture");
        let stage0 = run(
            Command::new(&ku).args(["check", "--json"]).arg(&input),
            &cwd,
            20,
        );
        let detail = stage0_detail(text, &stage0, code == "lexical");
        let want = if code == "lexical" {
            detail
        } else {
            format!("error|bootstrap.semantics.names|{code}|<source>|{detail}")
        };
        let source_run = run(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_eq!(source_run.status.code(), Some(1));
        assert!(source_run.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&source_run.stderr).contains(&want),
            "{want}: {:?}",
            source_run.stderr
        );
        expected_errors.push((text, want));
    }
    let mut expected_successes = Vec::new();
    for text in [library.as_str(), namespace, deferred] {
        fs::write(&input, text).expect("write isolated accepted source");
        let source_run = run(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_success(&source_run);
        expected_successes.push((
            text,
            String::from_utf8(source_run.stdout)
                .expect("canonical names output")
                .replace("\r\n", "\n"),
        ));
    }
    assert_eq!(expected_successes[0].1, SPAN_EXPECTED);
    assert!(expected_successes[1].1.contains("|Function|__ku_import42|"));
    assert!(expected_successes[2]
        .1
        .starts_with("IMPORTS|1\nNAME|Function|main|"));
    let owner = run(Command::new(&ku).arg("run").arg(&ownership_entry), &cwd, 20);
    assert_success(&owner);
    assert_eq!(
        String::from_utf8_lossy(&owner.stdout).trim(),
        "name-owner-ok"
    );

    for (entry, name) in [(&entry, "ku-names"), (&ownership_entry, "ku-owner")] {
        let executable = compiler.join(format!("{}{}", name, std::env::consts::EXE_SUFFIX));
        let built = run(
            Command::new(&ku)
                .args(["build", "--release"])
                .arg(entry)
                .arg("-o")
                .arg(&executable),
            &cwd,
            120,
        );
        assert_success(&built);
        fs::rename(
            &executable,
            relocated.join(executable.file_name().expect("native name")),
        )
        .expect("relocate built compiler");
    }
    fs::remove_dir_all(&source).expect("remove all compiler sources and staging");
    let exe = relocated.join(format!("ku-names{}", std::env::consts::EXE_SUFFIX));
    for (text, expected) in expected_successes {
        fs::write(relocated.join("input.ku"), text).expect("write source for relocated name pass");
        let native = run(&mut Command::new(&exe), &cwd, 20);
        assert_success(&native);
        assert_eq!(
            String::from_utf8_lossy(&native.stdout).replace("\r\n", "\n"),
            expected
        );
    }
    for (text, expected) in expected_errors {
        fs::write(relocated.join("input.ku"), text)
            .expect("write rejected source for relocated name pass");
        let native = run(&mut Command::new(&exe), &cwd, 20);
        assert_eq!(native.status.code(), Some(1));
        assert!(native.stdout.is_empty());
        assert_eq!(String::from_utf8_lossy(&native.stderr).trim_end(), expected);
    }
    let owner = run(
        &mut Command::new(relocated.join(format!("ku-owner{}", std::env::consts::EXE_SUFFIX))),
        &cwd,
        20,
    );
    assert_success(&owner);
    assert_eq!(
        String::from_utf8_lossy(&owner.stdout).trim(),
        "name-owner-ok"
    );
}

const IMPORT_BINDING: &str = r#"import "std.fs"
import { Node, ParseOutput } from "../stage2/ast.ku"
import { ParseProgram } from "../stage3/parser.ku"
import { BindSuppliedImports, ImportBinding, SuppliedModule } from "./imports.ku"
import { BuildNameTable } from "./names.ku"
import { BindTypeNames } from "./type_names.ku"

fn NodeAt(&graph: ParseOutput, id: int): Node {
    return graph.arena.nodes[id - 1].clone()
}

fn Witness(): ImportBinding! {
    importer = BuildNameTable(ParseProgram(fs.read("input.ku")?)?)?
    named = BuildNameTable(ParseProgram(fs.read("supplier.ku")?)?)?
    path = fs.read("path.txt")?
    supplied: [SuppliedModule] = [SuppliedModule { path: path, named: named }]
    type_uses = BindTypeNames(importer, supplied)?
    bindings = BindSuppliedImports(importer, supplied)?
    if (bindings.len() != 1) { panic("expected one binding") }
    binding = bindings[0].clone()
    request = NodeAt(importer.parsed, binding.request_node)
    target = NodeAt(supplied[binding.module_index - 1].named.parsed, binding.target_node)
    if (request.kind != "ImportName" || request.text != binding.source_name || target.kind != "Struct"
        || target.text != binding.source_name) { panic("import IDs lost their graph identity") }
    imported = 0
    for reference in type_uses {
        original = NodeAt(importer.parsed, reference.use_node)
        if (original.kind != "TypeName" || original.text != reference.name) { panic("lost type syntax identity") }
        if (reference.module_index == 1) {
            if (reference.name != binding.local_name || reference.declaration_kind != "Struct" || reference.declaration_node != binding.target_node) { panic("wrong imported type identity") }
            imported = imported + 1
        } else if (reference.declaration_node > 0) {
            local = NodeAt(importer.parsed, reference.declaration_node)
            if (local.kind != reference.declaration_kind || local.text != reference.name) { panic("wrong local type identity") }
        } else if (reference.declaration_kind != "Builtin") { panic("unexpected type namespace") }
    }
    if (type_uses.len() != 13 || imported != 1) { panic("unexpected span.ku type references") }
    return ok(binding)
}

fn main(): null! {
    binding = Witness()?
    println(binding.path + "|" + binding.source_name + "|" + binding.local_name + "|"
        + str(binding.module_index) + "|" + str(binding.request_node) + "|" + str(binding.target_node))
    return ok(null)
}
"#;

#[test]
fn bootstrap_binds_named_relative_imports_against_explicit_modules() {
    let temporary = Temp::new();
    let cwd = temporary.0.join("invocation");
    let source = temporary.0.join("compiler");
    let relocated = temporary.0.join("relocated");
    let oracle = temporary.0.join("oracle");
    let oracle_input = oracle.join("semantics");
    for directory in [
        &cwd,
        &source,
        &relocated,
        &oracle_input,
        &oracle.join("stage1"),
    ] {
        fs::create_dir_all(directory).expect("create private fixture directory");
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    for stage in ["stage1", "stage2", "stage3", "semantics"] {
        let target = source.join(stage);
        fs::create_dir(&target).expect("create copied compiler stage");
        for entry in fs::read_dir(repo.join("bootstrap").join(stage)).expect("read compiler stage")
        {
            let entry = entry.expect("read compiler stage file");
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "ku")
            {
                fs::copy(entry.path(), target.join(entry.file_name()))
                    .expect("copy Ku stage source");
            }
        }
    }
    let compiler = source.join("semantics");
    let entry = compiler.join("binding.ku");
    fs::write(&entry, IMPORT_BINDING).expect("write named binding witness");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let span = include_str!("../bootstrap/stage2/span.ku");
    let alias = span
        .replace("import { Token }", "import { Token as InputToken }")
        .replace("token: Token", "token: InputToken");
    let token = include_str!("../bootstrap/stage1/token.ku");
    let relative_path = "../stage1/token.ku";
    let install_case = |directory: &Path, input: &str, supplied: &str, path: &str| {
        fs::write(directory.join("input.ku"), input).expect("write isolated importer");
        fs::write(directory.join("supplier.ku"), supplied).expect("write isolated supplied module");
        fs::write(directory.join("path.txt"), path).expect("write explicit import mapping");
    };
    let successes = [
        (span, "../stage1/token.ku|Token|Token|1|1|19"),
        (&alias, "../stage1/token.ku|Token|InputToken|1|2|19"),
    ];
    for (input, expected) in successes {
        install_case(&compiler, input, token, relative_path);
        let ran = run(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_success(&ran);
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), expected);
    }
    let unknown_type = "fn main(): Missing { return null }";
    install_case(&compiler, unknown_type, token, relative_path);
    let rejected = run(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
    let unknown_diagnostic = "error|bootstrap.semantics.types|undefined_type|<source>|undefined type 'Missing'|1:12@11..1:19@18";
    assert_eq!(rejected.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains(unknown_diagnostic));

    let errors = [
        (
            "unresolved_import",
            "import { Token } from \"../stage1/token.ku\"\nfn main() {}",
            token,
            "./other.ku",
            "no module is supplied for '../stage1/token.ku'|1:1@0..1:43@42",
        ),
        (
            "missing_export",
            "import { Token } from \"../stage1/token.ku\"\nfn main() { Token() }",
            "module Token\nfn main() {}",
            relative_path,
            "'Token' is not exported by '../stage1/token.ku'|1:10@9..1:15@14",
        ),
        (
            "missing_export",
            "import { hidden } from \"../stage1/token.ku\"\nfn main() { hidden() }",
            "fn hidden() {}",
            relative_path,
            "'hidden' is not exported by '../stage1/token.ku'|1:10@9..1:16@15",
        ),
        (
            "duplicate_import_name",
            "import { One as A, Two as B, Two as A } from \"../stage1/token.ku\"\nfn main() { A() B() }",
            "fn One() {}\nfn Two() {}",
            relative_path,
            "duplicate import name 'Two'|1:30@29..1:33@32",
        ),
        (
            "duplicate_import_alias",
            "import { One as A, Two as A } from \"../stage1/token.ku\"\nfn main() { A() }",
            "fn One() {}\nfn Two() {}",
            relative_path,
            "duplicate import alias 'A'|1:20@19..1:23@22",
        ),
        (
            "import_name_conflict",
            "import { One as A } from \"../stage1/token.ku\"\nfn A() {}\nfn main() { A() }",
            "fn One() {}",
            relative_path,
            "imported name 'A' conflicts with another top-level name|1:10@9..1:13@12",
        ),
        (
            "unsupported_import_kind",
            "import \"../stage1/token.ku\"\nfn main() {}",
            token,
            relative_path,
            "supplied module binding accepts only named relative imports|1:1@0..1:28@27",
        ),
    ];
    for (code, input, supplied, path, detail) in errors {
        install_case(&compiler, input, supplied, path);
        let ran = run(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_eq!(ran.status.code(), Some(1));
        let want = format!("error|bootstrap.semantics.names|{code}|<source>|{detail}");
        let diagnostic = String::from_utf8_lossy(&ran.stderr);
        assert!(
            diagnostic.contains(&want),
            "{want} missing from {diagnostic}"
        );

        if matches!(
            code,
            "duplicate_import_name" | "duplicate_import_alias" | "import_name_conflict"
        ) {
            let path = oracle_input.join("input.ku");
            fs::write(&path, input).expect("write Stage0 oracle importer");
            fs::write(oracle.join("stage1/token.ku"), supplied)
                .expect("write Stage0 oracle dependency");
            let stage0 = run(
                Command::new(&ku).args(["check", "--json"]).arg(path),
                &cwd,
                20,
            );
            assert_eq!(stage0_detail(input, &stage0, false), detail);
        }
    }

    let executable = compiler.join(format!("ku-bind{}", std::env::consts::EXE_SUFFIX));
    let built = run(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&executable),
        &cwd,
        120,
    );
    assert_success(&built);
    let native = relocated.join(executable.file_name().expect("native name"));
    fs::rename(&executable, &native).expect("relocate named binding tool");
    fs::remove_dir_all(&source).expect("remove copied source tree and staging");
    for (input, expected) in successes {
        install_case(&relocated, input, token, relative_path);
        let ran = run(&mut Command::new(&native), &cwd, 20);
        assert_success(&ran);
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), expected);
    }
    install_case(&relocated, unknown_type, token, relative_path);
    let rejected = run(&mut Command::new(&native), &cwd, 20);
    assert_eq!(rejected.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&rejected.stderr).trim_end(),
        unknown_diagnostic
    );
    for (code, input, supplied, path, detail) in errors {
        install_case(&relocated, input, supplied, path);
        let ran = run(&mut Command::new(&native), &cwd, 20);
        assert_eq!(ran.status.code(), Some(1));
        assert!(ran.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&ran.stderr).trim_end(),
            format!("error|bootstrap.semantics.names|{code}|<source>|{detail}")
        );
    }
}
