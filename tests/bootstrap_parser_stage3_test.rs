//! Stage-3 self-hosted statement/module parser differential gate.
//!
//! Stage 3 deliberately accepts a strict subset of production Ku syntax. Every
//! accepted fixture is parsed by the Rust frontend and projected into the same
//! bounded NodeId/edge arena used by the Ku implementation.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, OutputLimits};
use ku::ast::{BinaryOp, Expr, ExprKind, FnDecl, Item, Literal, Stmt, TypeName, UnaryOp};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::span::Span;
use ku::token::TokenKind;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const NATIVE_BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const PROCESS_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(1024 * 1024, 2 * 1024 * 1024);

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!("ku-{label}-{}-{nonce}", std::process::id()))
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).ok();
    }
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
    span: Span,
    first_edge: usize,
    edge_count: usize,
}

#[derive(Default)]
struct ProjectedArena {
    nodes: Vec<ProjectedNode>,
    edges: Vec<usize>,
}

fn push_node(
    arena: &mut ProjectedArena,
    kind: &'static str,
    text: String,
    int_value: i64,
    span: Span,
    children: &[usize],
) -> usize {
    let first_edge = arena.edges.len();
    arena.edges.extend_from_slice(children);
    arena.nodes.push(ProjectedNode {
        kind,
        text,
        int_value,
        span,
        first_edge,
        edge_count: children.len(),
    });
    arena.nodes.len()
}

fn project_expr(source: &str, expr: &Expr, arena: &mut ProjectedArena) -> usize {
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
        other => panic!("stage-3 oracle received unsupported Rust expression: {other:?}"),
    };
    let child_ids = children
        .into_iter()
        .map(|child| project_expr(source, child, arena))
        .collect::<Vec<_>>();
    push_node(arena, kind, text, int_value, expr.span, &child_ids)
}

fn type_name_text(ty: &TypeName) -> &'static str {
    match ty {
        TypeName::Int => "int",
        TypeName::Float => "float",
        TypeName::Bool => "bool",
        TypeName::String => "str",
        TypeName::Null => "null",
        other => panic!("stage-3 oracle received unsupported type: {other:?}"),
    }
}

fn project_stmt(source: &str, statement: &Stmt, arena: &mut ProjectedArena) -> usize {
    match statement {
        Stmt::VarDecl {
            name,
            ty: Some(ty),
            value,
            span,
            ..
        } => {
            let value = project_expr(source, value, arena);
            push_node(
                arena,
                "VarDecl",
                format!("{name}:{}", type_name_text(ty)),
                0,
                *span,
                &[value],
            )
        }
        Stmt::Assign { name, value, span } => {
            let value = project_expr(source, value, arena);
            push_node(arena, "Assign", name.clone(), 0, *span, &[value])
        }
        Stmt::Expr { expr, span } => {
            let expr = project_expr(source, expr, arena);
            push_node(arena, "ExprStmt", String::new(), 0, *span, &[expr])
        }
        Stmt::Return { value, span } => {
            let children = value
                .as_ref()
                .map(|value| vec![project_expr(source, value, arena)])
                .unwrap_or_default();
            push_node(arena, "Return", String::new(), 0, *span, &children)
        }
        other => panic!("stage-3 oracle received unsupported Rust statement: {other:?}"),
    }
}

fn project_function(source: &str, function: &FnDecl, arena: &mut ProjectedArena) -> usize {
    assert!(!function.is_async, "stage-3 fixture must be synchronous");
    assert!(function.type_params.is_empty());
    assert!(function.params.is_empty());
    assert!(function.return_type.is_none());
    let children = function
        .body
        .iter()
        .map(|statement| project_stmt(source, statement, arena))
        .collect::<Vec<_>>();
    push_node(
        arena,
        "Function",
        function.name.clone(),
        0,
        function.span,
        &children,
    )
}

fn rust_canonical(source: &str) -> String {
    let tokens = Lexer::new(source).lex().expect("Rust oracle lex");
    let eof_span = tokens.last().expect("EOF token").span;
    let program = Parser::new(tokens)
        .parse_program()
        .expect("Rust oracle parse");
    let mut arena = ProjectedArena::default();
    let functions = program
        .items
        .iter()
        .map(|item| match item {
            Item::Function(function) => project_function(source, function, &mut arena),
            other => panic!("stage-3 oracle received unsupported item: {other:?}"),
        })
        .collect::<Vec<_>>();
    let program_span = match (functions.first(), functions.last()) {
        (Some(first), Some(last)) => Span::new(
            arena.nodes[*first - 1].span.start,
            arena.nodes[*last - 1].span.end,
        ),
        _ => eof_span,
    };
    let root = push_node(
        &mut arena,
        "Program",
        String::new(),
        0,
        program_span,
        &functions,
    );
    let mut output = format!("ROOT|{root}");
    for (index, node) in arena.nodes.iter().enumerate() {
        output.push_str(&format!(
            "\nNODE|{}|{}|{}|{}|{}:{}@{}..{}:{}@{}|{}|{}",
            index + 1,
            node.kind,
            escape_canonical(&node.text),
            node.int_value,
            node.span.start.line,
            node.span.start.column,
            node.span.start.offset,
            node.span.end.line,
            node.span.end.column,
            node.span.end.offset,
            node.first_edge,
            node.edge_count
        ));
    }
    for (index, child) in arena.edges.iter().enumerate() {
        output.push_str(&format!("\nEDGE|{index}|{child}"));
    }
    output
}

fn diagnostic_at(source: &str, token_index: usize, message: &str) -> String {
    let tokens = Lexer::new(source).lex().expect("diagnostic fixture lex");
    let span = tokens[token_index].span;
    format!(
        "{}|{}:{}@{}..{}:{}@{}",
        escape_canonical(message),
        span.start.line,
        span.start.column,
        span.start.offset,
        span.end.line,
        span.end.column,
        span.end.offset
    )
}

fn diagnostic_for_kind(source: &str, kind: TokenKind, message: &str) -> String {
    let tokens = Lexer::new(source).lex().expect("diagnostic fixture lex");
    let token = tokens
        .iter()
        .find(|token| std::mem::discriminant(&token.kind) == std::mem::discriminant(&kind))
        .expect("diagnostic token kind");
    let span = token.span;
    format!(
        "{}|{}:{}@{}..{}:{}@{}",
        escape_canonical(message),
        span.start.line,
        span.start.column,
        span.start.offset,
        span.end.line,
        span.end.column,
        span.end.offset
    )
}

fn rust_error_canonical(source: &str) -> String {
    let tokens = Lexer::new(source)
        .lex()
        .expect("Rust diagnostic fixture lex");
    let error = Parser::new(tokens)
        .parse_program()
        .expect_err("Rust diagnostic fixture must fail");
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
        .expect("stage-3 CLI process must remain bounded");
    assert!(
        output.status.success(),
        "ku {arguments:?} failed with {:?}:\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bootstrap_parser_stage3_matches_rust_and_stays_bounded() {
    let cases = [
        "",
        "fn main() {}",
        "fn helper() { return 7 }\nfn main() {\n  answer:int = 40 + 2\n  answer = answer + 1\n  println(answer)\n  return\n}",
        "fn main(){\n  a:float = 1.250\n  b:bool = true\n  c:str = \"中😀\"\n  d:null = null\n  return c\n}",
        "// 前置😀\nfn main() {\n  // gap 中\n  text:str = \"中😀\"\n  text = text + \"!\"\n  println(text)\n}",
        "fn main(){ values:[int] = [1] }",
    ];
    // The final case is intentionally filtered out below: it is a production
    // syntax example used to assert Stage 3's explicit subset boundary.
    let accepted = &cases[..cases.len() - 1];

    let param_source = "fn add(value) {}";
    let let_source = "fn main() { let value = 1; }";
    let missing_body = "fn main() {";
    let return_outside = "return 1";
    let top_level_expression = "42";
    let unsupported_type = cases[cases.len() - 1];
    let deep_expression = format!(
        "fn main(){{ value = {}1{} }}",
        "(".repeat(32),
        ")".repeat(32)
    );
    let broken_expression = "fn main(){ value = 1 + ; }";
    let empty_rhs_expression = "fn main(){ value = ; }";
    let unicode_broken_expression =
        "// 前😀\r\nfn main() {\r\n  text:str = \"中😀\"\r\n  value = 1 + ;\r\n}";
    let large_program = format!("fn main(){{{}}}", "x;".repeat(128));
    let over_statement_limit = format!("fn main(){{{}}}", "x;".repeat(129));
    let function_boundary = "fn f(){}".repeat(64);
    let over_function_limit = "fn f(){}".repeat(65);
    let over_token_limit = format!("fn main(){{ value = {}1 }}", "1 + ".repeat(252));
    let comment_chunk = "x".repeat(3750);
    let near_limit_source = format!(
        "fn main() {{\n  value = 1 + /*{}*/ 2\n}}",
        comment_chunk.repeat(8)
    );
    let slice_budget_source = format!(
        "fn main(){{/*{}*/\n{}}}",
        comment_chunk.repeat(2),
        "x\n".repeat(32)
    );
    let slice_budget_length = slice_budget_source.chars().count();
    let allowed_slices = 131_072 / slice_budget_length;
    assert!(allowed_slices < 32, "slice budget fixture must overflow");
    let slice_budget_tokens = Lexer::new(&slice_budget_source)
        .lex()
        .expect("slice budget fixture lex");
    let slice_budget_token_index = 5 + allowed_slices;
    assert!(matches!(
        slice_budget_tokens[slice_budget_token_index].kind,
        TokenKind::Ident(_)
    ));
    let slice_budget_message = diagnostic_at(
        &slice_budget_source,
        slice_budget_token_index,
        "stage-3 expression slicing budget exceeded",
    );

    let mut body = "import { ParseProgram } from \"./parser.ku\"\nimport { ProgramCanonical } from \"./ast.ku\"\n\n".to_string();
    body.push_str(
        "fn AssertCase(source: str, expected: str): null! {\n    actual = ProgramCanonical(ParseProgram(source.clone())?)\n    if (actual != expected) { panic(\"stage-3 differential mismatch: \" + source + \"\\n\" + actual + \"\\nEXPECTED\\n\" + expected) }\n    return ok(null)\n}\n\n",
    );
    body.push_str(
        "fn ExpectError(source: str, expected_code: str, expected_message: str): null! {\n    caught = false\n    try { ParseProgram(source)? } catch(err) {\n        caught = true\n        if (err.domain != \"bootstrap.parser.stage3\" || err.code != expected_code || err.message != expected_message) { panic(\"wrong stage-3 diagnostic: \" + err.domain + \"/\" + err.code + \"/\" + err.message) }\n    }\n    if (!caught) { panic(\"expected stage-3 parser error\") }\n    return ok(null)\n}\n\nfn main(): null! {\n",
    );
    for source in accepted {
        body.push_str(&format!(
            "    AssertCase({}, {})?\n",
            ku_string(source),
            ku_string(&rust_canonical(source))
        ));
    }
    body.push_str(&format!(
        "    comment_chunk = {}\n",
        ku_string(&comment_chunk)
    ));
    body.push_str("    long_gap = \"\"\n    long_index = 0\n    while (long_index < 8) {\n");
    body.push_str("        long_gap += comment_chunk.clone()\n");
    body.push_str(
        "        long_index = long_index + 1\n    }\n    near_limit_source = \"fn main() {\\n  value = 1 + /*\" + long_gap + \"*/ 2\\n}\"\n",
    );
    body.push_str(&format!(
        "    AssertCase(near_limit_source, {})?\n",
        ku_string(&rust_canonical(&near_limit_source))
    ));
    body.push_str(
        "    budget_gap = comment_chunk.clone() + comment_chunk.clone()\n    budget_source = \"fn main(){/*\" + budget_gap + \"*/\\n\"\n    budget_index = 0\n    while (budget_index < 32) {\n        budget_source += \"x\\n\"\n        budget_index = budget_index + 1\n    }\n    budget_source += \"}\"\n",
    );
    body.push_str(&format!(
        "    large = ParseProgram({})?\n    if (large.root != 258 || large.arena.nodes.len() != 258 || large.arena.edges.len() != 257) {{ panic(\"stage-3 statement arena boundary mismatch\") }}\n",
        ku_string(&large_program)
    ));
    body.push_str(&format!(
        "    many_functions = ParseProgram({})?\n    if (many_functions.root != 65 || many_functions.arena.nodes.len() != 65 || many_functions.arena.edges.len() != 64) {{ panic(\"stage-3 function boundary mismatch\") }}\n",
        ku_string(&function_boundary)
    ));

    let param_message = diagnostic_at(
        param_source,
        3,
        "stage-3 functions do not yet accept parameters",
    );
    let let_message = diagnostic_for_kind(
        let_source,
        TokenKind::Let,
        "'let' is not supported in Ku; use a typed declaration or assignment",
    );
    let missing_message = diagnostic_for_kind(
        missing_body,
        TokenKind::Eof,
        "expected '}' after function body",
    );
    let return_outside_message = diagnostic_at(return_outside, 0, "return outside function");
    let top_level_message =
        diagnostic_at(top_level_expression, 0, "expected a function declaration");
    let type_message = diagnostic_for_kind(
        unsupported_type,
        TokenKind::LBracket,
        "stage-3 declarations accept only int, float, bool, str, or null",
    );
    let deep_tokens = Lexer::new(&deep_expression)
        .lex()
        .expect("deep expression lex");
    let deep_origin = deep_tokens
        .iter()
        .position(|token| token.kind == TokenKind::Equal)
        .expect("deep assignment")
        + 1;
    let deep_message = diagnostic_at(
        &deep_expression,
        deep_origin + 31,
        "maximum parse depth exceeded; expression is too deeply nested",
    );
    let broken_message = rust_error_canonical(broken_expression);
    let empty_rhs_message = rust_error_canonical(empty_rhs_expression);
    let unicode_broken_message = rust_error_canonical(unicode_broken_expression);
    let statement_tokens = Lexer::new(&over_statement_limit)
        .lex()
        .expect("statement limit fixture lex");
    let statement_limit_index = 5 + 128 * 2;
    assert!(matches!(
        statement_tokens[statement_limit_index].kind,
        TokenKind::Ident(_)
    ));
    let statement_message = diagnostic_at(
        &over_statement_limit,
        statement_limit_index,
        "stage-3 parser accepts at most 128 statements per function",
    );
    let function_message = diagnostic_at(
        &over_function_limit,
        64 * 6,
        "stage-3 parser accepts at most 64 functions",
    );
    let token_message = diagnostic_at(
        &over_token_limit,
        0,
        "stage-3 parser accepts at most 512 tokens",
    );

    for (source, code, message) in [
        (
            param_source,
            "unsupported_function_signature",
            param_message,
        ),
        (let_source, "unsupported_statement", let_message),
        (missing_body, "unexpected_eof", missing_message),
        (
            return_outside,
            "return_outside_function",
            return_outside_message,
        ),
        (top_level_expression, "expected_item", top_level_message),
        (unsupported_type, "unsupported_type", type_message),
        (&deep_expression, "depth_exceeded", deep_message),
        (broken_expression, "unexpected_eof", broken_message),
        (empty_rhs_expression, "unexpected_eof", empty_rhs_message),
        (
            unicode_broken_expression,
            "unexpected_eof",
            unicode_broken_message,
        ),
        (&over_statement_limit, "statement_limit", statement_message),
        (&over_function_limit, "function_limit", function_message),
        (&over_token_limit, "invalid_token_stream", token_message),
    ] {
        body.push_str(&format!(
            "    ExpectError({}, {}, {})?\n",
            ku_string(source),
            ku_string(code),
            ku_string(&message)
        ));
    }
    body.push_str(&format!(
        "    ExpectError(budget_source, \"work_limit\", {})?\n",
        ku_string(&slice_budget_message)
    ));
    body.push_str("    return ok(null)\n}\n");

    let dir = unique_temp_dir("bootstrap-parser-stage3");
    let _cleanup = TempDirGuard::new(dir.clone());
    let source_root = dir.join("source");
    let stage1 = source_root.join("stage1");
    let stage2 = source_root.join("stage2");
    let stage3 = source_root.join("stage3");
    fs::create_dir_all(&stage1).expect("create copied stage-1 directory");
    fs::create_dir_all(&stage2).expect("create copied stage-2 directory");
    fs::create_dir_all(&stage3).expect("create copied stage-3 directory");
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
        .expect("copy stage-2 parser dependency");
    }
    for name in ["ast.ku", "parser.ku"] {
        fs::copy(
            repository_bootstrap.join("stage3").join(name),
            stage3.join(name),
        )
        .expect("copy stage-3 parser module");
    }
    let entry = stage3.join("main.ku");
    fs::write(&entry, body).expect("write stage-3 differential harness");
    let entry_arg = entry.to_string_lossy().to_string();
    run_ku(&["check", &entry_arg]);
    run_ku(&["run", &entry_arg]);
    run_ku(&["build", "--native", &entry_arg]);
    let native_c = fs::read_to_string(entry.with_extension("c"))
        .expect("stage-3 native build must emit a C artifact");
    assert!(native_c.contains("int main("));
    assert!(!native_c.contains("const SOURCE"));
    assert!(!native_c.contains("run_source"));
    assert!(
        native_c.contains("ku_string_slice("),
        "Stage 3 expression extraction must retain the bounded slice path"
    );

    let native_name = if cfg!(windows) {
        "stage3-native.exe"
    } else {
        "stage3-native"
    };
    let native_path = dir.join(native_name);
    let native_arg = native_path.to_string_lossy().to_string();
    let mut build = Command::new(ku_binary());
    build.args(["build", "--backend", "c", "-o", &native_arg, &entry_arg]);
    let built = run_bounded(&mut build, NATIVE_BUILD_TIMEOUT, PROCESS_OUTPUT_LIMITS)
        .expect("stage-3 native link must remain bounded");
    let build_log = format!(
        "{}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    if built.status.success() {
        fs::remove_dir_all(&source_root).expect("remove complete stage-3 source graph");
        let mut native = Command::new(&native_path);
        let ran = run_bounded(&mut native, PROCESS_TIMEOUT, PROCESS_OUTPUT_LIMITS)
            .expect("stage-3 native executable must remain bounded");
        assert!(
            ran.status.success(),
            "stage-3 native executable failed with {:?}:\n{}{}",
            ran.status.code(),
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&ran.stderr)
        );
    } else if !build_log.contains("C compiler not found") {
        panic!("stage-3 native link failed unexpectedly:\n{build_log}");
    }
}
