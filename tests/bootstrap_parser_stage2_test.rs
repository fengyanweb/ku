//! Stage-2 self-hosted expression parser differential gate.
//!
//! The harness invokes the ordinary `ku` executable, so Windows exercises the
//! production CLI worker stack rather than receiving a test-only larger stack.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, OutputLimits};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use ku::ast::{BinaryOp, Expr, ExprKind, Literal, UnaryOp};
use ku::lexer::Lexer;
use ku::parser::Parser;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const NATIVE_BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const PROCESS_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(1024 * 1024, 2 * 1024 * 1024);

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!("ku-{label}-{}-{nonce}", std::process::id()))
}

fn escape_canonical(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('|', "\\p")
}

fn ku_string(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\r', "\\r")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}

#[derive(Clone)]
struct ProjectedNode {
    kind: &'static str,
    text: String,
    int_value: i64,
    expr: Expr,
    first_edge: usize,
    edge_count: usize,
}

fn project_expr(
    source: &str,
    expr: &Expr,
    nodes: &mut Vec<ProjectedNode>,
    edges: &mut Vec<usize>,
) -> usize {
    let (kind, text, int_value, children): (&str, String, i64, Vec<&Expr>) = match &expr.kind {
        ExprKind::Literal(Literal::Int(value)) => ("Int", String::new(), *value, vec![]),
        ExprKind::Literal(Literal::Float(_)) => (
            "Float",
            source[expr.span.start.offset..expr.span.end.offset].to_string(),
            0,
            vec![],
        ),
        ExprKind::Literal(Literal::Bool(value)) => ("Bool", value.to_string(), 0, vec![]),
        ExprKind::Literal(Literal::String(value)) => ("String", value.clone(), 0, vec![]),
        ExprKind::Literal(Literal::TemplateString(value)) => {
            ("TemplateString", value.clone(), 0, vec![])
        }
        ExprKind::Literal(Literal::Null) => ("Null", String::new(), 0, vec![]),
        ExprKind::Variable(name) => ("Variable", name.clone(), 0, vec![]),
        ExprKind::Unary { op, expr } => (
            "Unary",
            match op {
                UnaryOp::Negate => "-",
                UnaryOp::Not => "!",
            }
            .to_string(),
            0,
            vec![expr],
        ),
        ExprKind::Binary { left, op, right } => (
            "Binary",
            match op {
                BinaryOp::Add => "+",
                BinaryOp::Subtract => "-",
                BinaryOp::Multiply => "*",
                BinaryOp::Divide => "/",
                BinaryOp::Remainder => "%",
                BinaryOp::Equal => "==",
                BinaryOp::NotEqual => "!=",
                BinaryOp::Less => "<",
                BinaryOp::LessEqual => "<=",
                BinaryOp::Greater => ">",
                BinaryOp::GreaterEqual => ">=",
                BinaryOp::And => "&&",
                BinaryOp::Or => "||",
            }
            .to_string(),
            0,
            vec![left, right],
        ),
        ExprKind::Call { callee, args } => {
            let mut children = vec![callee.as_ref()];
            children.extend(args.iter());
            ("Call", String::new(), 0, children)
        }
        ExprKind::Array(items) => ("Array", String::new(), 0, items.iter().collect()),
        ExprKind::Index { target, index } => ("Index", String::new(), 0, vec![target, index]),
        ExprKind::Field { target, name } => ("Field", name.clone(), 0, vec![target]),
        ExprKind::OptionalField { target, name } => {
            ("OptionalField", name.clone(), 0, vec![target])
        }
        ExprKind::TryUnwrap { expr } => ("TryUnwrap", String::new(), 0, vec![expr]),
        other => panic!("stage-2 oracle received unsupported Rust AST: {other:?}"),
    };
    let child_ids = children
        .into_iter()
        .map(|child| project_expr(source, child, nodes, edges))
        .collect::<Vec<_>>();
    let first_edge = edges.len();
    edges.extend(child_ids);
    nodes.push(ProjectedNode {
        kind,
        text,
        int_value,
        expr: expr.clone(),
        first_edge,
        edge_count: edges.len() - first_edge,
    });
    nodes.len()
}

fn rust_canonical(source: &str) -> String {
    let tokens = Lexer::new(source).lex().expect("Rust oracle lex");
    let expr = Parser::new(tokens)
        .parse_expression_only()
        .expect("Rust oracle parse");
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let root = project_expr(source, &expr, &mut nodes, &mut edges);
    let mut output = format!("ROOT|{root}");
    for (index, node) in nodes.iter().enumerate() {
        let span = node.expr.span;
        output.push_str(&format!(
            "\nNODE|{}|{}|{}|{}|{}:{}@{}..{}:{}@{}|{}|{}",
            index + 1,
            node.kind,
            escape_canonical(&node.text),
            node.int_value,
            span.start.line,
            span.start.column,
            span.start.offset,
            span.end.line,
            span.end.column,
            span.end.offset,
            node.first_edge,
            node.edge_count
        ));
    }
    for (index, child) in edges.iter().enumerate() {
        output.push_str(&format!("\nEDGE|{index}|{child}"));
    }
    output
}

fn rust_error_canonical(source: &str) -> String {
    let tokens = Lexer::new(source).lex().expect("Rust oracle lex");
    let error = Parser::new(tokens)
        .parse_expression_only()
        .expect_err("Rust oracle expression must fail");
    format!(
        "{}|{}:{}@{}..{}:{}@{}",
        escape_canonical(&error.message),
        error.span.start.line,
        error.span.start.column,
        error.span.start.offset,
        error.span.end.line,
        error.span.end.column,
        error.span.end.offset
    )
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = std::env::var("KU_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    let executable = if cfg!(windows) { "ku.exe" } else { "ku" };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug")
        .join(executable)
}

fn run_ku(arguments: &[&str]) {
    let mut command = Command::new(ku_binary());
    command.args(arguments);
    let output = run_bounded(&mut command, PROCESS_TIMEOUT, PROCESS_OUTPUT_LIMITS)
        .expect("stage-2 CLI process must remain bounded");
    assert!(
        output.status.success(),
        "ku {arguments:?} failed with {:?}:\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_worker_preserves_normal_error_exit_and_stderr() {
    let missing = unique_temp_dir("bootstrap-parser-missing").join("missing.ku");
    let missing_arg = missing.to_string_lossy().to_string();
    let mut command = Command::new(ku_binary());
    command.args(["check", &missing_arg]);
    let output = run_bounded(&mut command, PROCESS_TIMEOUT, PROCESS_OUTPUT_LIMITS)
        .expect("failing CLI process must remain bounded");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "errors must not move to stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.is_empty(), "ordinary CLI errors must reach stderr");
    assert!(
        !stderr.contains("panicked at"),
        "ordinary KuError must not be converted to a panic: {stderr}"
    );
}

#[test]
fn bootstrap_parser_stage2_matches_rust_and_has_stable_diagnostics() {
    let mut cases = vec![
        "42".to_string(),
        "1.250".to_string(),
        "true".to_string(),
        "false".to_string(),
        "null".to_string(),
        "name".to_string(),
        "\"中😀\"".to_string(),
        "`raw`".to_string(),
        "!-name".to_string(),
        "1 + 2 * 3 - 4 / 2".to_string(),
        "1 < 2 == true && false || true".to_string(),
        "[]".to_string(),
        "[1, true, null, 1.250]".to_string(),
        "apply()".to_string(),
        "apply(1, 2 + 3)".to_string(),
        "items[1 + 2]".to_string(),
        "user.name".to_string(),
        "user?.name".to_string(),
        "value?".to_string(),
        "user.items[0]?.name?".to_string(),
        "apply([true, null], -value).field?".to_string(),
        "(1 + 2) * 3".to_string(),
    ];
    cases.push(format!("{}1{}", "(".repeat(30), ")".repeat(30)));
    let long_flat = (0..256).map(|_| "1").collect::<Vec<_>>().join(" + ");
    let mut body = "import { Parse, ParseWithContext } from \"./parser.ku\"\nimport { ParseContext } from \"./context.ku\"\nimport { AstCanonical } from \"./ast.ku\"\nimport { Span } from \"./span.ku\"\n\n".to_string();
    body.push_str(
        "fn AssertCase(source: str, expected: str): null! {\n    actual = AstCanonical(Parse(source.clone())?)\n    if (actual != expected) { panic(\"stage-2 differential mismatch: \" + source + \"\\n\" + actual + \"\\nEXPECTED\\n\" + expected) }\n    return ok(null)\n}\n\n",
    );
    body.push_str(
        "fn ExpectError(source: str, expected_code: str, expected_message: str): null! {\n    caught = false\n    try { Parse(source)? } catch(err) {\n        caught = true\n        if (err.domain != \"bootstrap.parser\" || err.code != expected_code || err.message != expected_message) { panic(\"wrong diagnostic: \" + err.domain + \"/\" + err.code + \"/\" + err.message) }\n    }\n    if (!caught) { panic(\"expected parser error\") }\n    return ok(null)\n}\n\nfn ExpectContextError(source: str, origin: Span, boundary: Span, has_boundary: bool): null! {\n    caught = false\n    try { ParseWithContext(source, ParseContext { origin: origin, boundary: boundary, has_boundary: has_boundary })? } catch(err) {\n        caught = true\n        if (err.domain != \"bootstrap.parser\" || err.code != \"invalid_parse_context\") { panic(\"wrong context diagnostic: \" + err.domain + \"/\" + err.code + \"/\" + err.message) }\n    }\n    if (!caught) { panic(\"expected invalid parse context\") }\n    return ok(null)\n}\n\nfn main(): null! {\n",
    );
    body.push_str(
        "    context_origin = Span { line: 7, column: 5, offset: 40, end_line: 7, end_column: 5, end_offset: 40 }\n    context_boundary = Span { line: 7, column: 12, offset: 47, end_line: 7, end_column: 13, end_offset: 48 }\n    context_caught = false\n    try { ParseWithContext(\"1 +\", ParseContext { origin: context_origin.clone(), boundary: context_boundary, has_boundary: true })? } catch(err) {\n        context_caught = true\n        if (err.domain != \"bootstrap.parser\" || err.code != \"unexpected_eof\" || err.message != \"expected expression|7:12@47..7:13@48\") { panic(\"wrong relocated diagnostic: \" + err.domain + \"/\" + err.code + \"/\" + err.message) }\n    }\n    if (!context_caught) { panic(\"expected relocated parser error\") }\n",
    );
    body.push_str(
        "    point = Span { line: 1, column: 1, offset: 0, end_line: 1, end_column: 1, end_offset: 0 }\n    max_line = Span { line: 9223372036854775807, column: 1, offset: 0, end_line: 9223372036854775807, end_column: 1, end_offset: 0 }\n    max_column = Span { line: 1, column: 9223372036854775807, offset: 0, end_line: 1, end_column: 9223372036854775807, end_offset: 0 }\n    max_offset = Span { line: 1, column: 1, offset: 9223372036854775807, end_line: 1, end_column: 1, end_offset: 9223372036854775807 }\n    ExpectContextError(\"1\", max_line, point.clone(), false)?\n    ExpectContextError(\"1\", max_column, point.clone(), false)?\n    ExpectContextError(\"1\", max_offset, point.clone(), false)?\n    overlap = Span { line: 7, column: 7, offset: 42, end_line: 7, end_column: 8, end_offset: 43 }\n    ExpectContextError(\"1 +\", context_origin.clone(), overlap, true)?\n    malformed_end = Span { line: 7, column: 12, offset: 47, end_line: 0, end_column: 0, end_offset: 48 }\n    ExpectContextError(\"1 +\", context_origin.clone(), malformed_end, true)?\n    no_boundary_caught = false\n    try { ParseWithContext(\"1 +\", ParseContext { origin: context_origin.clone(), boundary: point.clone(), has_boundary: false })? } catch(err) {\n        no_boundary_caught = true\n        if (err.code != \"unexpected_eof\" || err.message != \"expected expression|7:8@43..7:8@43\") { panic(\"wrong mathematical EOF relocation: \" + err.code + \"/\" + err.message) }\n    }\n    if (!no_boundary_caught) { panic(\"expected mathematical EOF diagnostic\") }\n    exact_boundary = Span { line: 7, column: 8, offset: 43, end_line: 7, end_column: 8, end_offset: 43 }\n    exact_boundary_caught = false\n    try { ParseWithContext(\"1 +\", ParseContext { origin: context_origin.clone(), boundary: exact_boundary, has_boundary: true })? } catch(err) {\n        exact_boundary_caught = true\n        if (err.code != \"unexpected_eof\" || err.message != \"expected expression|7:8@43..7:8@43\") { panic(\"wrong exact boundary relocation: \" + err.code + \"/\" + err.message) }\n    }\n    if (!exact_boundary_caught) { panic(\"expected exact boundary diagnostic\") }\n",
    );
    for source in &cases {
        body.push_str(&format!(
            "    AssertCase({}, {})?\n",
            ku_string(source),
            ku_string(&rust_canonical(source))
        ));
    }
    let too_deep = format!("{}1{}", "(".repeat(32), ")".repeat(32));
    body.push_str(&format!(
        "    large = Parse({})?\n    if (large.root != 511 || large.arena.nodes.len() != 511 || large.arena.edges.len() != 510) {{ panic(\"flat binary parser is not iterative/bounded\") }}\n",
        ku_string(&long_flat)
    ));
    body.push_str(&format!(
        "    ExpectError(\"\", \"unexpected_eof\", {})?\n    ExpectError(\"1 +\", \"unexpected_eof\", {})?\n    ExpectError(\"()\", \"unexpected_token\", \"expected expression before closing delimiter|1:2@1..1:3@2\")?\n    ExpectError(\"a.\", \"unexpected_token\", \"expected field name after field operator|1:3@2..1:3@2\")?\n    ExpectError(\"a)\", \"unexpected_token\", \"closing delimiter has no matching opener|1:2@1..1:3@2\")?\n    ExpectError(\"[1)\", \"unexpected_token\", \"closing delimiter does not match opener|1:3@2..1:4@3\")?\n    ExpectError(\"a[]\", \"unexpected_token\", \"expected expression before closing delimiter|1:3@2..1:4@3\")?\n    ExpectError(\"f(1,)\", \"unexpected_token\", \"expected expression before closing delimiter|1:5@4..1:6@5\")?\n",
        ku_string(&rust_error_canonical("")),
        ku_string(&rust_error_canonical("1 +"))
    ));
    body.push_str(&format!(
        "    ExpectError({}, \"depth_exceeded\", \"maximum parse depth exceeded; expression is too deeply nested|1:32@31..1:33@32\")?\n    return ok(null)\n}}\n",
        ku_string(&too_deep)
    ));

    let dir = unique_temp_dir("bootstrap-parser-stage2");
    let source_root = dir.join("source");
    let stage1 = source_root.join("stage1");
    let stage2 = source_root.join("stage2");
    fs::create_dir_all(&stage1).expect("create copied stage-1 directory");
    fs::create_dir_all(&stage2).expect("create copied stage-2 directory");
    let repository_bootstrap = Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap");
    for name in ["token.ku", "lexer.ku"] {
        fs::copy(
            repository_bootstrap.join("stage1").join(name),
            stage1.join(name),
        )
        .expect("copy stage-1 parser dependency");
    }
    for name in [
        "span.ku",
        "diagnostic.ku",
        "ast.ku",
        "context.ku",
        "parser.ku",
    ] {
        fs::copy(
            repository_bootstrap.join("stage2").join(name),
            stage2.join(name),
        )
        .expect("copy stage-2 parser module");
    }
    let entry = stage2.join("main.ku");
    fs::write(&entry, body).expect("write stage-2 differential harness");
    let entry_arg = entry.to_string_lossy().to_string();
    run_ku(&["check", &entry_arg]);
    run_ku(&["run", &entry_arg]);
    run_ku(&["build", "--native", &entry_arg]);
    let native_c = fs::read_to_string(entry.with_extension("c"))
        .expect("stage-2 native build must emit a C artifact");
    assert!(native_c.contains("int main("));
    assert!(!native_c.contains("const SOURCE"));
    assert!(!native_c.contains("run_source"));

    let native_name = if cfg!(windows) {
        "stage2-native.exe"
    } else {
        "stage2-native"
    };
    let native_path = dir.join(native_name);
    let native_arg = native_path.to_string_lossy().to_string();
    let mut build = Command::new(ku_binary());
    build.args(["build", "--backend", "c", "-o", &native_arg, &entry_arg]);
    let built = run_bounded(&mut build, NATIVE_BUILD_TIMEOUT, PROCESS_OUTPUT_LIMITS)
        .expect("stage-2 native link must remain bounded");
    let build_log = format!(
        "{}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    if built.status.success() {
        fs::remove_dir_all(&source_root).expect("remove complete stage-2 source graph");
        let mut native = Command::new(&native_path);
        let ran = run_bounded(&mut native, PROCESS_TIMEOUT, PROCESS_OUTPUT_LIMITS)
            .expect("stage-2 native executable must remain bounded");
        assert!(
            ran.status.success(),
            "stage-2 native executable failed with {:?}:\n{}{}",
            ran.status.code(),
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&ran.stderr)
        );
    } else if !build_log.contains("C compiler not found") {
        panic!("stage-2 native link failed unexpectedly:\n{build_log}");
    }
    fs::remove_dir_all(&dir).ok();
}
