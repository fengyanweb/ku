//! Stage0 differential checks for the Ku declaration name pass.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use ku::{span::Span, stdlib::json, value::Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ku-bootstrap-names-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create private bootstrap directory");
        Self(path)
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

fn NodeAt(&graph: ParseOutput, id: int): Node {
    return graph.arena.nodes[id - 1].clone()
}

fn Witness(): ImportBinding! {
    importer = BuildNameTable(ParseProgram(fs.read("input.ku")?)?)?
    named = BuildNameTable(ParseProgram(fs.read("supplier.ku")?)?)?
    path = fs.read("path.txt")?
    supplied: [SuppliedModule] = [SuppliedModule { path: path, named: named }]
    bindings = BindSuppliedImports(importer, supplied)?
    if (bindings.len() != 1) { panic("expected one binding") }
    binding = bindings[0].clone()
    request = NodeAt(importer.parsed, binding.request_node)
    target = NodeAt(supplied[binding.module_index - 1].named.parsed, binding.target_node)
    if (request.kind != "ImportName" || request.text != binding.source_name || target.kind != "Struct"
        || target.text != binding.source_name) { panic("import IDs lost their graph identity") }
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
