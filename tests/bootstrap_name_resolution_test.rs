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
