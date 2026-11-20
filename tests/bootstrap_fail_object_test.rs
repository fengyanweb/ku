//! Compare the hosted compiler's object/fail syntax with the Stage0 AST.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use ku::ast::{Expr, ExprKind, Item, Literal, Stmt};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::span::{Position, Span};
use ku::token::{Token, TokenKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SEED: &str = r#"import "std.fs"
import { ParseModule } from "./parser.ku"
import { AstCanonical } from "../stage2/ast.ku"
fn main(): null! {
    source = fs.read("input.ku")?
    println(AstCanonical(ParseModule(source)?))
    return ok(null)
}
"#;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ku-bootstrap-fail-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create private parser test");
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn execute(command: &mut Command, cwd: &Path, seconds: u64) -> BoundedOutput {
    run_bounded(
        command.current_dir(cwd),
        Duration::from_secs(seconds),
        OutputLimits::new(256 * 1024, 512 * 1024),
    )
    .expect("bounded parser command")
}

fn staged_parser(compiler: &Path) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for stage in ["stage1", "stage2", "stage3"] {
        let target = compiler.join(stage);
        fs::create_dir_all(&target).expect("copied compiler stage");
        for entry in fs::read_dir(root.join("bootstrap").join(stage)).expect("read stage") {
            let entry = entry.expect("stage source");
            if entry.path().extension().is_some_and(|ext| ext == "ku") {
                fs::copy(entry.path(), target.join(entry.file_name()))
                    .expect("copy compiler module");
            }
        }
    }
    let seed = compiler.join("stage3/object_seed.ku");
    fs::write(&seed, SEED).expect("write seed launcher");
    seed
}

fn canonical_span(span: Span) -> String {
    let position = |p: Position| format!("{}:{}@{}", p.line, p.column, p.offset);
    format!("{}..{}", position(span.start), position(span.end))
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('|', "\\p")
}

#[derive(Debug, PartialEq, Eq)]
struct Row {
    kind: String,
    text: String,
    span: String,
    children: Vec<String>,
}

fn actual_rows(output: &[u8]) -> Vec<Row> {
    let ast = String::from_utf8_lossy(output);
    let mut nodes: Vec<Row> = Vec::new();
    let mut slices: Vec<(usize, usize)> = Vec::new();
    let mut edges: Vec<usize> = Vec::new();
    for line in ast.lines() {
        let columns: Vec<_> = line.split('|').collect();
        match columns[0] {
            "ROOT" => (),
            "NODE" => {
                assert_eq!(columns.len(), 8, "{line}");
                assert_eq!(columns[1].parse::<usize>().unwrap(), nodes.len() + 1);
                nodes.push(Row {
                    kind: columns[2].into(),
                    text: columns[3].into(),
                    span: columns[5].into(),
                    children: Vec::new(),
                });
                slices.push((columns[6].parse().unwrap(), columns[7].parse().unwrap()));
            }
            "EDGE" => {
                assert_eq!(columns[1].parse::<usize>().unwrap(), edges.len());
                edges.push(columns[2].parse::<usize>().unwrap() - 1);
            }
            _ => panic!("unexpected canonical row {line}"),
        }
    }
    for (index, (start, count)) in slices.into_iter().enumerate() {
        nodes[index].children = edges[start..start + count]
            .iter()
            .map(|child| nodes[*child].kind.clone())
            .collect();
    }
    nodes
        .into_iter()
        .filter(|row| {
            matches!(
                row.kind.as_str(),
                "Fail" | "ObjectLiteral" | "ObjectLiteralField"
            )
        })
        .collect()
}

fn kind(expr: &Expr) -> &'static str {
    match expr.kind {
        ExprKind::Literal(Literal::Int(_)) => "Int",
        ExprKind::Literal(Literal::String(_)) => "String",
        ExprKind::Variable(_) => "Variable",
        ExprKind::Array(_) => "Array",
        ExprKind::Call { .. } => "Call",
        ExprKind::ObjectLiteral { .. } => "ObjectLiteral",
        _ => panic!(
            "fixture has an unexpected object field or fail operand: {:?}",
            expr.kind
        ),
    }
}

fn expected_expr(expr: &Expr, tokens: &[Token], rows: &mut Vec<Row>) {
    match &expr.kind {
        ExprKind::ObjectLiteral { fields } => {
            let mut lower = expr.span.start.offset;
            let mut children = Vec::new();
            for (key, value) in fields {
                expected_expr(value, tokens, rows);
                let start = tokens.windows(2).find(|pair| {
                    pair[0].span.start.offset >= lower
                        && pair[1].span.end.offset <= value.span.start.offset
                        && matches!(&pair[0].kind, TokenKind::Ident(name) | TokenKind::String(name) if name == key)
                        && pair[1].kind == TokenKind::Colon
                }).expect("source key token preceding its Stage0 value")[0].span.start;
                rows.push(Row {
                    kind: "ObjectLiteralField".into(),
                    text: escape(key),
                    span: canonical_span(Span::new(start, value.span.end)),
                    children: vec![kind(value).into()],
                });
                lower = value.span.end.offset;
                children.push("ObjectLiteralField".into());
            }
            rows.push(Row {
                kind: "ObjectLiteral".into(),
                text: String::new(),
                span: canonical_span(expr.span),
                children,
            });
        }
        ExprKind::Array(entries) | ExprKind::Call { args: entries, .. } => {
            for entry in entries {
                expected_expr(entry, tokens, rows);
            }
        }
        _ => {}
    }
}

fn expected_statements(statements: &[Stmt], tokens: &[Token], rows: &mut Vec<Row>) {
    for statement in statements {
        match statement {
            Stmt::Fail { value, span } => {
                expected_expr(value, tokens, rows);
                rows.push(Row {
                    kind: "Fail".into(),
                    text: String::new(),
                    span: canonical_span(*span),
                    children: vec![kind(value).into()],
                });
            }
            Stmt::Assign { value, .. }
            | Stmt::Return {
                value: Some(value), ..
            }
            | Stmt::VarDecl { value, .. } => expected_expr(value, tokens, rows),
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                expected_statements(then_branch, tokens, rows);
                expected_statements(else_branch, tokens, rows);
            }
            _ => {}
        }
    }
}

fn expected_rows(source: &str) -> Vec<Row> {
    let tokens = Lexer::new(source).lex().expect("Stage0 tokens");
    let program = Parser::new(tokens.clone())
        .parse_program()
        .expect("Stage0 syntax");
    let mut rows = Vec::new();
    for item in program.items {
        if let Item::Function(function) = item {
            expected_statements(&function.body, &tokens, &mut rows);
        }
    }
    rows
}

#[test]
fn bootstrap_parser_accepts_actual_stage3_failure_payload() {
    let temp = Temp::new();
    let compiler = temp.0.join("compiler");
    let consumer = temp.0.join("consumer");
    fs::create_dir(&consumer).expect("isolated compiler invocation");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let executable = format!("object_seed{}", std::env::consts::EXE_SUFFIX);
    let entry = staged_parser(&compiler);
    let built = compiler.join("stage3").join(&executable);
    let seed = consumer.join(executable);
    let build = execute(
        Command::new(ku)
            .args(["build", "--release"])
            .arg(entry)
            .arg("-o")
            .arg(&built),
        &consumer,
        150,
    );
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    fs::rename(built, &seed).expect("relocate default native parser");
    fs::remove_dir_all(compiler).expect("remove staged compiler sources");
    let invoke = |source: &str| {
        fs::write(consumer.join("input.ku"), source).expect("write parser subject");
        execute(&mut Command::new(&seed), &consumer, 30)
    };

    // Use the actual definitions and their original absolute coordinates.
    // Later functions in support.ku exercise unrelated syntax.
    let (module, _) = include_str!("../bootstrap/stage3/support.ku")
        .split_once("fn ParseExpressionAt")
        .expect("real failure helpers before remaining support functions");
    let example = r#"fn Example(value): null! {
        payload = { "中文|": { rank: 0 }, empty: {}, values: [1, { a: 2 }] }
        if (true) { fail payload; } else { fail { domain: "x", code: "y", message: "z" } }
        return Wrap({ "title": value })
    }"#;
    for source in [module, example] {
        let expected = expected_rows(source);
        assert!(expected.iter().any(|row| row.kind == "Fail"));
        assert!(expected.iter().any(|row| row.kind == "ObjectLiteralField"));
        let output = invoke(source);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(actual_rows(&output.stdout), expected);
    }

    for source in [
        "fn Main(): null! { fail ; }",
        "fn Main(): null! { fail { domain \"missing colon\" } }",
        "fn Main(): null! { fail { message: \"trailing\", } }",
        "fn Main(): null! { fail { message: } }",
        "fn Main(): null! { fail { 1: \"not a key\" } }",
    ] {
        let rust = Parser::new(Lexer::new(source).lex().unwrap())
            .parse_program()
            .expect_err("Stage0 rejects malformed failure");
        let actual = invoke(source);
        assert!(
            !actual.status.success() && actual.stdout.is_empty(),
            "{source}"
        );
        let expected = format!(
            "<source>|{}|{}",
            escape(&rust.message),
            canonical_span(rust.span)
        );
        let stderr = String::from_utf8_lossy(&actual.stderr);
        assert!(
            stderr.contains(&expected),
            "{source}: {stderr}\nexpected {expected}"
        );
    }

    let deep = format!(
        "fn Main(): null! {{ fail {}0{} }}",
        "{ value: ".repeat(34),
        " }".repeat(34)
    );
    let rejected = invoke(&deep);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("depth_exceeded"));
}
