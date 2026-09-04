//! Stage-3 self-hosted statement/module parser differential gate.
//!
//! Stage 3 deliberately accepts a strict subset of production Ku syntax. Every
//! accepted fixture is parsed by the Rust frontend and projected into the same
//! bounded NodeId/edge arena used by the Ku implementation.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, OutputLimits};
use ku::ast::{
    BinaryOp, Expr, ExprKind, FnDecl, ImportDecl, ImportKind, Item, Literal, ModuleDecl, Stmt,
    TypeName, UnaryOp,
};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::span::Span;
use ku::token::{Token, TokenKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const NATIVE_BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const PROCESS_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(1024 * 1024, 2 * 1024 * 1024);

// Static ABI goldens are not produced by the Rust projector during the test.
// Both parser implementations are checked against these reviewed bytes.
const TYPED_UNION_GOLDEN_SOURCE: &str = "fn choose(value: int | str): int | null { return value }";
const TYPED_UNION_GOLDEN: &str = concat!(
    "ROOT|11\n",
    "NODE|1|TypeName|int|0|1:18@17..1:21@20|0|0\n",
    "NODE|2|TypeName|str|0|1:24@23..1:27@26|0|0\n",
    "NODE|3|TypeUnion||0|1:18@17..1:27@26|0|2\n",
    "NODE|4|Parameter|value|0|1:11@10..1:16@15|2|1\n",
    "NODE|5|TypeName|int|0|1:30@29..1:33@32|3|0\n",
    "NODE|6|TypeName|null|0|1:36@35..1:40@39|3|0\n",
    "NODE|7|TypeUnion||0|1:30@29..1:40@39|3|2\n",
    "NODE|8|Variable|value|0|1:50@49..1:55@54|5|0\n",
    "NODE|9|Return||0|1:43@42..1:55@54|5|1\n",
    "NODE|10|Function|choose|0|1:1@0..1:57@56|6|3\n",
    "NODE|11|Program||0|1:1@0..1:57@56|9|1\n",
    "EDGE|0|1\n",
    "EDGE|1|2\n",
    "EDGE|2|3\n",
    "EDGE|3|5\n",
    "EDGE|4|6\n",
    "EDGE|5|8\n",
    "EDGE|6|4\n",
    "EDGE|7|7\n",
    "EDGE|8|9\n",
    "EDGE|9|10",
);
const MODULE_GOLDEN_SOURCE: &str = "// 前😀\r\nmodule App;\r\nfn main() {}";
const MODULE_GOLDEN: &str = concat!(
    "ROOT|3\n",
    "NODE|1|Module|App|0|2:1@12..2:11@22|0|0\n",
    "NODE|2|Function|main|0|3:1@25..3:13@37|0|0\n",
    "NODE|3|Program||0|2:1@12..3:13@37|0|2\n",
    "EDGE|0|1\n",
    "EDGE|1|2",
);
const UNICODE_CRLF_ERROR_SOURCE: &str =
    "// 前😀\r\nfn main() {\r\n  text:str = \"中😀\"\r\n  value = 1 + ;\r\n}";
const UNICODE_CRLF_ERROR_CODE: &str = "unexpected_eof";
const UNICODE_CRLF_ERROR_DETAIL: &str = "expected expression|4:15@63..4:16@64";
const UNICODE_CRLF_ERROR_CANONICAL: &str = concat!(
    "error|bootstrap.parser.stage3|unexpected_eof|<source>|",
    "expected expression|4:15@63..4:16@64",
);
const MULTILINE_RELOCATED_ERROR_SOURCE: &str =
    "// 前😀\r\nfn main() {\r\n  value = 1 +\r\n  );\r\n}";
const MULTILINE_RELOCATED_ERROR_DETAIL: &str = "binary operator has no left operand|4:3@42..4:4@43";
const MULTILINE_RELOCATED_ERROR_CANONICAL: &str = concat!(
    "error|bootstrap.parser.stage3|invalid_expression|<source>|",
    "binary operator has no left operand|4:3@42..4:4@43",
);
const MULTILINE_BOUNDARY_ERROR_SOURCE: &str = "fn main() {\r\n  value = 1 +\r\n  ;\r\n}";
const MULTILINE_BOUNDARY_ERROR_DETAIL: &str = "expected expression|3:3@30..3:4@31";
const MULTILINE_BOUNDARY_ERROR_CANONICAL: &str = concat!(
    "error|bootstrap.parser.stage3|unexpected_eof|<source>|",
    "expected expression|3:3@30..3:4@31",
);

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

fn project_type(
    tokens: &[Token],
    cursor: &mut usize,
    ty: &TypeName,
    arena: &mut ProjectedArena,
) -> usize {
    match ty {
        TypeName::Array(inner) => {
            assert!(matches!(tokens[*cursor].kind, TokenKind::LBracket));
            let start = tokens[*cursor].span.start;
            *cursor += 1;
            let child = project_type(tokens, cursor, inner, arena);
            assert!(matches!(tokens[*cursor].kind, TokenKind::RBracket));
            let end = tokens[*cursor].span.end;
            *cursor += 1;
            push_node(
                arena,
                "TypeArray",
                String::new(),
                0,
                Span::new(start, end),
                &[child],
            )
        }
        TypeName::Result(inner) => {
            let child = project_type(tokens, cursor, inner, arena);
            assert!(matches!(tokens[*cursor].kind, TokenKind::Bang));
            let span = Span::new(arena.nodes[child - 1].span.start, tokens[*cursor].span.end);
            *cursor += 1;
            push_node(arena, "TypeResult", String::new(), 0, span, &[child])
        }
        TypeName::Union(types) => {
            assert!(
                types.len() > 1,
                "Rust union oracle must have at least two arms"
            );
            let mut children = Vec::with_capacity(types.len());
            for (index, ty) in types.iter().enumerate() {
                children.push(project_type(tokens, cursor, ty, arena));
                if index + 1 < types.len() {
                    assert!(matches!(tokens[*cursor].kind, TokenKind::Pipe));
                    *cursor += 1;
                }
            }
            let span = Span::new(
                arena.nodes[children[0] - 1].span.start,
                arena.nodes[*children.last().expect("union child") - 1]
                    .span
                    .end,
            );
            push_node(arena, "TypeUnion", String::new(), 0, span, &children)
        }
        TypeName::Int
        | TypeName::Float
        | TypeName::Bool
        | TypeName::String
        | TypeName::Null
        | TypeName::Custom(_) => {
            let text = match ty {
                TypeName::Int => "int".to_string(),
                TypeName::Float => "float".to_string(),
                TypeName::Bool => "bool".to_string(),
                TypeName::String => "str".to_string(),
                TypeName::Null => "null".to_string(),
                TypeName::Custom(name) => name.clone(),
                _ => unreachable!(),
            };
            let start = tokens[*cursor].span.start;
            let component_count = text.split('.').count();
            let mut end = tokens[*cursor].span.end;
            *cursor += 1;
            for _ in 1..component_count {
                assert!(matches!(tokens[*cursor].kind, TokenKind::Dot));
                *cursor += 1;
                assert!(matches!(tokens[*cursor].kind, TokenKind::Ident(_)));
                end = tokens[*cursor].span.end;
                *cursor += 1;
            }
            push_node(arena, "TypeName", text, 0, Span::new(start, end), &[])
        }
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
            let tokens = Lexer::new(source)
                .lex()
                .expect("declaration projection lex");
            let mut cursor = tokens
                .iter()
                .position(|token| token.span.start == span.start)
                .expect("declaration start token")
                + 2;
            let ty = project_type(&tokens, &mut cursor, ty, arena);
            let value = project_expr(source, value, arena);
            push_node(arena, "VarDecl", name.clone(), 0, *span, &[ty, value])
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

fn project_import(source: &str, import: &ImportDecl, arena: &mut ProjectedArena) -> usize {
    let tokens = Lexer::new(source).lex().expect("import projection lex");
    let mut cursor = tokens
        .iter()
        .position(|token| {
            matches!(token.kind, TokenKind::Import)
                && token.span.start.offset == import.span.start.offset
        })
        .expect("import start token");
    cursor += 1;
    let mut children = Vec::new();
    match &import.kind {
        ImportKind::Glob => {}
        ImportKind::Namespace(namespace) => {
            assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
            children.push(push_node(
                arena,
                "ImportNamespace",
                namespace.clone(),
                0,
                tokens[cursor].span,
                &[],
            ));
            cursor += 1;
            assert!(matches!(tokens[cursor].kind, TokenKind::From));
            cursor += 1;
        }
        ImportKind::Named(names) => {
            assert!(matches!(tokens[cursor].kind, TokenKind::LBrace));
            cursor += 1;
            for name in names {
                assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
                cursor += 1;
                let name_children = if let Some(alias) = &name.alias {
                    assert!(matches!(
                        &tokens[cursor].kind,
                        TokenKind::Ident(marker) if marker == "as"
                    ));
                    cursor += 1;
                    assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
                    let child = push_node(
                        arena,
                        "ImportAlias",
                        alias.clone(),
                        0,
                        tokens[cursor].span,
                        &[],
                    );
                    cursor += 1;
                    vec![child]
                } else {
                    Vec::new()
                };
                children.push(push_node(
                    arena,
                    "ImportName",
                    name.source.clone(),
                    0,
                    name.span,
                    &name_children,
                ));
                if matches!(tokens[cursor].kind, TokenKind::Comma) {
                    cursor += 1;
                }
            }
            assert!(matches!(tokens[cursor].kind, TokenKind::RBrace));
            cursor += 1;
            assert!(matches!(tokens[cursor].kind, TokenKind::From));
            cursor += 1;
        }
    }
    assert!(matches!(&tokens[cursor].kind, TokenKind::String(path) if path == &import.path));
    push_node(
        arena,
        "Import",
        import.path.clone(),
        0,
        import.span,
        &children,
    )
}

fn project_function(source: &str, function: &FnDecl, arena: &mut ProjectedArena) -> usize {
    assert!(!function.is_async, "stage-3 fixture must be synchronous");
    assert!(function.type_params.is_empty());
    let tokens = Lexer::new(source).lex().expect("signature projection lex");
    let mut cursor = tokens
        .iter()
        .position(|token| {
            matches!(token.kind, TokenKind::Fn)
                && token.span.start.offset == function.span.start.offset
        })
        .expect("function start token");
    cursor += 1;
    assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
    cursor += 1;
    assert!(matches!(tokens[cursor].kind, TokenKind::LParen));
    cursor += 1;

    let mut children = Vec::new();
    for param in &function.params {
        assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
        cursor += 1;
        let param_children = if let Some(ty) = &param.ty {
            assert!(matches!(tokens[cursor].kind, TokenKind::Colon));
            cursor += 1;
            vec![project_type(&tokens, &mut cursor, ty, arena)]
        } else {
            Vec::new()
        };
        children.push(push_node(
            arena,
            "Parameter",
            param.name.clone(),
            0,
            param.span,
            &param_children,
        ));
        if matches!(tokens[cursor].kind, TokenKind::Comma) {
            cursor += 1;
        }
    }
    assert!(matches!(tokens[cursor].kind, TokenKind::RParen));
    cursor += 1;
    if let Some(ty) = &function.return_type {
        assert!(matches!(tokens[cursor].kind, TokenKind::Colon));
        cursor += 1;
        children.push(project_type(&tokens, &mut cursor, ty, arena));
    }
    assert!(matches!(tokens[cursor].kind, TokenKind::LBrace));

    children.extend(
        function
            .body
            .iter()
            .map(|statement| project_stmt(source, statement, arena)),
    );
    push_node(
        arena,
        "Function",
        function.name.clone(),
        0,
        function.span,
        &children,
    )
}

fn project_module(module: &ModuleDecl, arena: &mut ProjectedArena) -> usize {
    push_node(arena, "Module", module.name.clone(), 0, module.span, &[])
}

fn rust_canonical(source: &str) -> String {
    let tokens = Lexer::new(source).lex().expect("Rust oracle lex");
    let eof_span = tokens.last().expect("EOF token").span;
    let program = Parser::new(tokens)
        .parse_program()
        .expect("Rust oracle parse");
    let mut arena = ProjectedArena::default();
    let items = program
        .items
        .iter()
        .map(|item| match item {
            Item::Import(import) => project_import(source, import, &mut arena),
            Item::Function(function) => project_function(source, function, &mut arena),
            Item::Module(module) => project_module(module, &mut arena),
            other => panic!("stage-3 oracle received unsupported item: {other:?}"),
        })
        .collect::<Vec<_>>();
    let program_span = match (items.first(), items.last()) {
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
        &items,
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

fn rust_diagnostic_canonical(source: &str) -> String {
    format!(
        "error|bootstrap.parser.stage3|unexpected_eof|<source>|{}",
        rust_error_canonical(source)
    )
}

#[test]
fn checked_in_stage3_goldens_pin_rust_projection_and_diagnostic_span() {
    assert_eq!(
        rust_canonical(TYPED_UNION_GOLDEN_SOURCE),
        TYPED_UNION_GOLDEN
    );
    assert_eq!(rust_canonical(MODULE_GOLDEN_SOURCE), MODULE_GOLDEN);
    assert_eq!(
        rust_diagnostic_canonical(UNICODE_CRLF_ERROR_SOURCE),
        UNICODE_CRLF_ERROR_CANONICAL
    );
    assert_eq!(
        format!(
            "error|bootstrap.parser.stage3|invalid_expression|<source>|{}",
            diagnostic_at(
                MULTILINE_RELOCATED_ERROR_SOURCE,
                Lexer::new(MULTILINE_RELOCATED_ERROR_SOURCE)
                    .lex()
                    .expect("multiline relocation fixture lex")
                    .iter()
                    .rposition(|token| token.kind == TokenKind::RParen)
                    .expect("multiline relocation closing parenthesis"),
                "binary operator has no left operand"
            )
        ),
        MULTILINE_RELOCATED_ERROR_CANONICAL
    );
    assert_eq!(
        rust_diagnostic_canonical(MULTILINE_BOUNDARY_ERROR_SOURCE),
        MULTILINE_BOUNDARY_ERROR_CANONICAL
    );
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
        "import \"./setup.ku\";\nfn before() {}\nimport math from \"./math.ku\"\nimport { Add, User as Person } from \"./api.ku\"\nfn main() { return Add(1, 2) }",
        "// 导入😀\r\nimport { Add as Plus } from \"./中😀.ku\"\r\nimport \"./tab\\tname.ku\"\r\nfn main() {}",
        "module App",
        "module App;",
        MODULE_GOLDEN_SOURCE,
        "import \"./setup.ku\"\nmodule App;\nfn main() {}",
        "module Same\nmodule Same;",
        "fn main() {}",
        "fn typed(): int { return 1 }",
        "fn add(left: int, right: int): int { return left + right }\nfn echo(value): str { return \"ok\" }",
        "// 签名😀\r\nfn load(path: str, cached: bool, prior: int!): str! {\r\n  return path\r\n}",
        "fn typed(values: [pkg.model.User!]!): [[int]!]! { local:[pkg.Item!] = [] return values }",
        "fn choose(value: int | str): int | str { local:int | str = value return local }",
        "fn nested(value: [int | str], other: [int] | str!): [int | str]! { local:[int] | str! = other return value }",
        "fn duplicate(value: int | int | null): int | int | null { return value }",
        "// 前😀\r\nfn dotted(value: pkg.model.User | other.Type!): [pkg.model.User | other.Type!]! {\r\n  local:pkg.model.User | other.Type! = value\r\n  return local\r\n}",
        "fn helper() { return 7 }\nfn main() {\n  answer:int = 40 + 2\n  answer = answer + 1\n  println(answer)\n  return\n}",
        "fn main(){\n  a:float = 1.250\n  b:bool = true\n  c:str = \"中😀\"\n  d:null = null\n  return c\n}",
        "// 前置😀\nfn main() {\n  // gap 中\n  text:str = \"中😀\"\n  text = text + \"!\"\n  println(text)\n}",
        "fn main(){ values:[int] = [1] }",
    ];
    let accepted = &cases;

    let let_source = "fn main() { let value = 1; }";
    let missing_body = "fn main() {";
    let return_outside = "return 1";
    let top_level_expression = "42";
    let deep_expression = format!(
        "fn main(){{ value = {}1{} }}",
        "(".repeat(32),
        ")".repeat(32)
    );
    let broken_expression = "fn main(){ value = 1 + ; }";
    let empty_rhs_expression = "fn main(){ value = ; }";
    let unicode_broken_expression = UNICODE_CRLF_ERROR_SOURCE;
    let missing_param_name = "fn f(: int) {}";
    let missing_param_type = "fn f(value:) {}";
    let missing_param_comma = "fn f(left:int right:int) {}";
    let trailing_param_comma = "fn f(value:int,) {}";
    let missing_return_type = "fn f(): {}";
    let missing_signature_body = "fn f(): int!";
    let legacy_type_alias = "fn f(value: string) {}";
    let unicode_broken_signature = "// 前😀\r\nfn f(value: str, : int): null! {}";
    let unsupported_signature_type = "fn f(op: fn(int): int) {}";
    let leading_union_pipe = "fn f(value: | int) {}";
    let trailing_union_pipe = "fn f(value: int |) {}";
    let nested_trailing_union_pipe = "fn f(value: [int |]) {}";
    let return_trailing_union_pipe = "fn f(): int | {}";
    let trailing_union_at_eof = "fn f(value: int |";
    let nested_trailing_union_at_eof = "fn f(value: [int |";
    let double_pipe_union = "fn f(value: int || str) {}";
    let repeated_result_union = "fn f(value: T!! | U) {}";
    let primitive_dotted_parameter = "fn f(value: int.foo) {}";
    let primitive_dotted_return = "fn f(): str.X {}";
    let primitive_dotted_local = "fn f() { value:null.Y = null }";
    let missing_array_close = "fn f(value: [pkg.User) {}";
    let missing_dotted_name = "fn f(value: pkg.) {}";
    let repeated_result = "fn f(value: int!!) {}";
    let accepted_type_depth = format!("fn f(value: {}int{}) {{}}", "[".repeat(32), "]".repeat(32));
    let rejected_type_depth = format!("fn f(value: {}int{}) {{}}", "[".repeat(33), "]".repeat(33));
    let accepted_mixed_type_depth =
        format!("fn f(value: {}int!{}) {{}}", "[".repeat(31), "]".repeat(31));
    let rejected_mixed_type_depth =
        format!("fn f(value: {}int!{}) {{}}", "[".repeat(32), "]".repeat(32));
    let empty_named_import = "import {} from \"./math.ku\"";
    let trailing_import_comma = "import { Add, } from \"./math.ku\"";
    let missing_import_alias = "import { Add as } from \"./math.ku\"";
    let missing_named_from = "import { Add } \"./math.ku\"";
    let missing_namespace_from = "import math \"./math.ku\"";
    let legacy_import_from = "import from \"./math.ku\"";
    let invalid_import_namespace = "import 1";
    let invalid_import_path = "import math from 1";
    let missing_import_path = "import math from";
    let missing_import_target = "import";
    let missing_import_alias_at_eof = "import { Add as";
    let unicode_broken_import = "// 前😀\r\nimport { Add as } from \"./中.ku\"";
    let early_import_name_string = "import { \"early\"";
    let early_import_alias_string = "import { Add as \"early\"";
    let early_namespace_path = "import ns \"early\"";
    let second_string_item = "import \"first\" \"second\"";
    let second_missing_import = "import { A } from \"first\"; import";
    let missing_module_name = "module";
    let keyword_module_name = "module fn";
    let dotted_module_name = "module pkg.name";
    let module_body = "module App {}";
    let module_alias = "module App as Other";
    let double_module_semicolon = "module App;;";
    let nested_module = "fn main() { module Inner }";
    let unsupported_item = "struct User {}";
    let parameter_boundary = format!(
        "fn many({}): null {{ return null }}",
        (0..32)
            .map(|index| format!("p{index}: int"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let over_parameter_limit = format!(
        "fn many({}) {{}}",
        (0..33)
            .map(|index| format!("p{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let union_boundary = format!(
        "fn choose(value: {}) {{}}",
        (0..64)
            .map(|index| format!("T{index}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let over_union_limit = format!(
        "fn choose(value: {}) {{}}",
        (0..65)
            .map(|index| format!("T{index}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let array_union_boundary = format!(
        "fn choose(value: {}) {{}}",
        (0..64)
            .map(|index| format!("[T{index}]"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let over_array_union_limit = format!(
        "fn choose(value: {}) {{}}",
        (0..65)
            .map(|index| format!("[T{index}]"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let import_boundary = "import \"m\"\n".repeat(64);
    let over_import_limit = "import \"m\"\n".repeat(65);
    let imports_then_function = format!("{import_boundary}fn tail() {{}}");
    let named_import_boundary = format!(
        "import {{ {} }} from \"m\";",
        (0..64)
            .map(|index| format!("N{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let named_import_alias_boundary = format!(
        "import {{ {} }} from \"m\";",
        (0..64)
            .map(|index| format!("N{index} as A{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let over_import_name_limit = format!(
        "import {{ {} }} from \"m\"",
        (0..65)
            .map(|index| format!("N{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let over_import_alias_name_limit = format!(
        "import {{ {} }} from \"m\";",
        (0..65)
            .map(|index| format!("N{index} as A{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(
        Lexer::new(&named_import_alias_boundary)
            .lex()
            .expect("64-name alias import lex")
            .len(),
        262,
        "64 aliases must fit the bounded import window"
    );
    assert_eq!(
        Lexer::new(&over_import_alias_name_limit)
            .lex()
            .expect("65-name alias import lex")
            .len(),
        266,
        "65 aliases must reach the name-limit diagnostic before the window limit"
    );
    let large_program = format!("fn main(){{{}}}", "x;".repeat(128));
    let over_statement_limit = format!("fn main(){{{}}}", "x;".repeat(129));
    let function_boundary = "fn f(){}".repeat(64);
    let over_function_limit = "fn f(){}".repeat(65);
    let functions_then_import = format!("{function_boundary}import \"tail\"");
    let module_boundary = "module M\n".repeat(255);
    let over_module_token_limit = "module M\n".repeat(256);
    assert_eq!(
        Lexer::new(&module_boundary)
            .lex()
            .expect("255-module boundary lex")
            .len(),
        511,
        "255 modules must fit the complete Stage-3 token budget"
    );
    assert_eq!(
        Lexer::new(&over_module_token_limit)
            .lex()
            .expect("256-module over-limit lex")
            .len(),
        513,
        "256 modules must exceed the complete Stage-3 token budget"
    );
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
    let over_type_window_source = "T ".repeat(512);
    let direct_import_over_window = "x ".repeat(266);
    let direct_import_over_tokens = Lexer::new(&direct_import_over_window)
        .lex()
        .expect("direct import over-window fixture lex");
    assert_eq!(
        direct_import_over_tokens.len(),
        267,
        "direct ReadImport fixture must exceed its 266-token window by one"
    );
    let direct_import_limit_message = diagnostic_at(
        &over_import_alias_name_limit,
        2 + 64 * 4,
        "stage-3 named imports accept at most 64 names",
    );
    let direct_import_over_message = diagnostic_at(
        &direct_import_over_window,
        265,
        "stage-3 import inspection budget exceeded",
    );
    let direct_map_token_limit_source = "x ".repeat(511);
    assert_eq!(
        Lexer::new(&direct_map_token_limit_source)
            .lex()
            .expect("direct token-map boundary fixture lex")
            .len(),
        512,
        "direct MapTokenCharacters fixture must reach its exact token limit"
    );
    let direct_map_token_over_source = "x ".repeat(512);
    assert_eq!(
        Lexer::new(&direct_map_token_over_source)
            .lex()
            .expect("direct token-map over-limit fixture lex")
            .len(),
        513,
        "direct MapTokenCharacters fixture must exceed its token limit by one"
    );
    let direct_map_token_over_message = diagnostic_at(
        &direct_map_token_over_source,
        0,
        "stage-3 parser accepts at most 512 tokens",
    );

    let mut body = "import { Token } from \"../stage1/token.ku\"\nimport { Scan } from \"../stage1/lexer.ku\"\nimport { AstCanonical } from \"../stage2/ast.ku\"\nimport { ParseProgram } from \"./parser.ku\"\nimport { ReadImport } from \"./imports.ku\"\nimport { MapTokenCharacters } from \"./support.ku\"\nimport { ParseTypeWindow } from \"./signature.ku\"\n\n".to_string();
    body.push_str(
        "fn AssertCase(source: str, expected: str): null! {\n    actual = AstCanonical(ParseProgram(source.clone())?)\n    if (actual != expected) { panic(\"stage-3 differential mismatch: \" + source + \"\\n\" + actual + \"\\nEXPECTED\\n\" + expected) }\n    return ok(null)\n}\n\n",
    );
    body.push_str(
        r#"fn ExpectEmptyImportWindowError(): null! {
    empty: [Token] = []
    caught = false
    try { ReadImport(empty)? } catch(err) {
        caught = true
        expected = "error|bootstrap.parser.stage3|invalid_token_stream|<source>|import window must start with import and end with EOF|1:1@0..1:1@0"
        if (err.domain != "bootstrap.parser.stage3" || err.code != "invalid_token_stream" || err.message != expected) {
            panic("wrong empty import-window diagnostic: " + err.domain + "/" + err.code + "/" + err.message)
        }
    }
    if (!caught) { panic("expected empty import-window error") }
    return ok(null)
}

"#,
    );
    body.push_str(
        r#"fn ExpectImportWindowError(tokens: [Token], expected_code: str, expected_detail: str): null! {
    expected_message = "error|bootstrap.parser.stage3|" + expected_code.clone() + "|<source>|" + expected_detail
    caught = false
    try { ReadImport(tokens)? } catch(err) {
        caught = true
        if (err.domain != "bootstrap.parser.stage3" || err.code != expected_code || err.message != expected_message) {
            panic("wrong direct import-window diagnostic: " + err.domain + "/" + err.code + "/" + err.message + " EXPECTED " + expected_message)
        }
    }
    if (!caught) { panic("expected direct import-window error") }
    return ok(null)
}

fn ExpectTokenMapError(source: str, tokens: [Token], expected_code: str, expected_detail: str): null! {
    expected_message = "error|bootstrap.parser.stage3|" + expected_code.clone() + "|<source>|" + expected_detail
    caught = false
    try { MapTokenCharacters(source, tokens)? } catch(err) {
        caught = true
        if (err.domain != "bootstrap.parser.stage3" || err.code != expected_code || err.message != expected_message) {
            panic("wrong direct token-map diagnostic: " + err.domain + "/" + err.code + "/" + err.message + " EXPECTED " + expected_message)
        }
    }
    if (!caught) { panic("expected direct token-map error") }
    return ok(null)
}

"#,
    );
    body.push_str(
        "fn ExpectError(source: str, expected_code: str, expected_detail: str): null! {\n    expected_message = \"error|bootstrap.parser.stage3|\" + expected_code.clone() + \"|<source>|\" + expected_detail\n    caught = false\n    try { ParseProgram(source.clone())? } catch(err) {\n        caught = true\n        if (err.domain != \"bootstrap.parser.stage3\" || err.code != expected_code || err.message != expected_message) { panic(\"wrong stage-3 diagnostic for \" + source + \": \" + err.domain + \"/\" + err.code + \"/\" + err.message + \" EXPECTED \" + expected_code + \"/\" + expected_message) }\n    }\n    if (!caught) { panic(\"expected stage-3 parser error\") }\n    return ok(null)\n}\n\nfn ExpectTypeWindowError(tokens: [Token], expected_detail: str): null! {\n    expected_message = \"error|bootstrap.parser.stage3|invalid_token_stream|<source>|\" + expected_detail\n    caught = false\n    try { ParseTypeWindow(tokens)? } catch(err) {\n        caught = true\n        if (err.domain != \"bootstrap.parser.stage3\" || err.code != \"invalid_token_stream\" || err.message != expected_message) { panic(\"wrong type-window diagnostic: \" + err.domain + \"/\" + err.code + \"/\" + err.message + \" EXPECTED \" + expected_message) }\n    }\n    if (!caught) { panic(\"expected type-window error\") }\n    return ok(null)\n}\n\nfn main(): null! {\n",
    );
    body.push_str(&format!(
        "    ExpectEmptyImportWindowError()?\n    direct_tokens = Scan(\"int\")?\n    direct = ParseTypeWindow(direct_tokens.clone())?\n    if (direct.work != 4) {{ panic(\"type-window validation work was not counted\") }}\n    empty_window: [Token] = []\n    ExpectTypeWindowError(empty_window, \"type window must contain one final non-type boundary token|1:1@0..1:1@0\")?\n    missing_boundary: [Token] = [direct_tokens[0].clone()]\n    ExpectTypeWindowError(missing_boundary, \"type window is missing its final non-type boundary|1:1@0..1:4@3\")?\n    early_boundary = direct_tokens.push(direct_tokens[direct_tokens.len() - 1].clone())\n    ExpectTypeWindowError(early_boundary, \"type window boundary must be final|1:4@3..1:4@3\")?\n    too_many_type_tokens = Scan({})?\n    ExpectTypeWindowError(too_many_type_tokens, \"type window accepts at most 512 tokens|1:1@0..1:2@1\")?\n",
        ku_string(&over_type_window_source)
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(TYPED_UNION_GOLDEN_SOURCE),
        ku_string(TYPED_UNION_GOLDEN)
    ));
    for source in accepted {
        body.push_str(&format!(
            "    AssertCase({}, {})?\n",
            ku_string(source),
            ku_string(&rust_canonical(source))
        ));
    }
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&accepted_type_depth),
        ku_string(&rust_canonical(&accepted_type_depth))
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&accepted_mixed_type_depth),
        ku_string(&rust_canonical(&accepted_mixed_type_depth))
    ));
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
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&parameter_boundary),
        ku_string(&rust_canonical(&parameter_boundary))
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&union_boundary),
        ku_string(&rust_canonical(&union_boundary))
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&array_union_boundary),
        ku_string(&rust_canonical(&array_union_boundary))
    ));
    for source in [
        &import_boundary,
        &named_import_boundary,
        &named_import_alias_boundary,
        &imports_then_function,
        &functions_then_import,
    ] {
        body.push_str(&format!(
            "    AssertCase({}, {})?\n",
            ku_string(source),
            ku_string(&rust_canonical(source))
        ));
    }
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
    body.push_str(&format!(
        "    many_imports = ParseProgram({})?\n    if (many_imports.root != 65 || many_imports.arena.nodes.len() != 65 || many_imports.arena.edges.len() != 64) {{ panic(\"stage-3 import boundary mismatch\") }}\n",
        ku_string(&import_boundary)
    ));
    body.push_str(&format!(
        "    many_names = ParseProgram({})?\n    if (many_names.root != 66 || many_names.arena.nodes.len() != 66 || many_names.arena.edges.len() != 65) {{ panic(\"stage-3 import-name arena boundary mismatch\") }}\n",
        ku_string(&named_import_boundary)
    ));
    body.push_str(&format!(
        "    many_aliases = ParseProgram({})?\n    if (many_aliases.root != 130 || many_aliases.arena.nodes.len() != 130 || many_aliases.arena.edges.len() != 129) {{ panic(\"stage-3 import-alias arena boundary mismatch\") }}\n",
        ku_string(&named_import_alias_boundary)
    ));
    body.push_str(&format!(
        "    direct_import_limit = Scan({})?\n    ExpectImportWindowError(direct_import_limit, \"import_name_limit\", {})?\n    direct_import_over = Scan({})?\n    ExpectImportWindowError(direct_import_over, \"work_limit\", {})?\n",
        ku_string(&over_import_alias_name_limit),
        ku_string(&direct_import_limit_message),
        ku_string(&direct_import_over_window),
        ku_string(&direct_import_over_message),
    ));
    body.push_str(&format!(
        "    direct_map_limit_source = {}\n    direct_map_limit_tokens = Scan(direct_map_limit_source.clone())?\n    direct_map_limit = MapTokenCharacters(direct_map_limit_source.clone(), direct_map_limit_tokens)?\n    if (direct_map_limit.characters.len() != direct_map_limit_source.len() || direct_map_limit.starts.len() != 512 || direct_map_limit.ends.len() != 512) {{ panic(\"direct token-map exact token boundary mismatch\") }}\n    direct_map_over_source = {}\n    direct_map_over_tokens = Scan(direct_map_over_source.clone())?\n    ExpectTokenMapError(direct_map_over_source, direct_map_over_tokens, \"invalid_token_stream\", {})?\n",
        ku_string(&direct_map_token_limit_source),
        ku_string(&direct_map_token_over_source),
        ku_string(&direct_map_token_over_message),
    ));
    body.push_str(
        "    map_source = \" \"\n    map_source_round = 0\n    while (map_source_round < 15) {\n        map_source_copy = map_source.clone()\n        map_source += map_source_copy\n        map_source_round = map_source_round + 1\n    }\n    if (map_source.len() != 32768 || map_source.byte_len() != 32768) { panic(\"direct token-map source boundary fixture mismatch\") }\n    map_source_tokens = Scan(map_source.clone())?\n    map_source_limit = MapTokenCharacters(map_source.clone(), map_source_tokens)?\n    if (map_source_limit.characters.len() != 32768 || map_source_limit.starts.len() != 1 || map_source_limit.starts[0] != 32768) { panic(\"direct token-map exact source boundary mismatch\") }\n    map_source_over = map_source + \" \"\n    map_point_tokens = Scan(\"\")?\n    ExpectTokenMapError(map_source_over, map_point_tokens, \"work_limit\", \"stage-3 token mapping budget exceeded|1:1@0..1:1@0\")?\n    wide_source = \"😀\"\n    wide_source_round = 0\n    while (wide_source_round < 15) {\n        wide_source_copy = wide_source.clone()\n        wide_source += wide_source_copy\n        wide_source_round = wide_source_round + 1\n    }\n    if (wide_source.len() != 32768 || wide_source.byte_len() != 131072) { panic(\"direct token-map byte boundary fixture mismatch\") }\n    wide_point_tokens = Scan(\"\")?\n    wide_source_limit = MapTokenCharacters(wide_source.clone(), wide_point_tokens.clone())?\n    if (wide_source_limit.characters.len() != 32768) { panic(\"direct token-map exact byte boundary mismatch\") }\n    wide_source_over = wide_source + \"😀\"\n    ExpectTokenMapError(wide_source_over, wide_point_tokens, \"work_limit\", \"stage-3 token mapping budget exceeded|1:1@0..1:1@0\")?\n",
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
    let top_level_message = rust_error_canonical(top_level_expression);
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
    let unicode_broken_message = UNICODE_CRLF_ERROR_DETAIL.to_string();
    let multiline_relocated_message = MULTILINE_RELOCATED_ERROR_DETAIL.to_string();
    let multiline_boundary_message = MULTILINE_BOUNDARY_ERROR_DETAIL.to_string();
    let missing_param_name_message = rust_error_canonical(missing_param_name);
    let missing_param_type_message = rust_error_canonical(missing_param_type);
    let missing_param_comma_message = rust_error_canonical(missing_param_comma);
    let trailing_param_comma_message = rust_error_canonical(trailing_param_comma);
    let missing_return_type_message = rust_error_canonical(missing_return_type);
    let missing_signature_body_message = rust_error_canonical(missing_signature_body);
    let legacy_type_alias_message = rust_error_canonical(legacy_type_alias);
    let unicode_broken_signature_message = rust_error_canonical(unicode_broken_signature);
    let empty_named_import_message = rust_error_canonical(empty_named_import);
    let trailing_import_comma_message = rust_error_canonical(trailing_import_comma);
    let missing_import_alias_message = rust_error_canonical(missing_import_alias);
    let missing_named_from_message = rust_error_canonical(missing_named_from);
    let missing_namespace_from_message = rust_error_canonical(missing_namespace_from);
    let legacy_import_from_message = rust_error_canonical(legacy_import_from);
    let invalid_import_namespace_message = rust_error_canonical(invalid_import_namespace);
    let invalid_import_path_message = rust_error_canonical(invalid_import_path);
    let missing_import_path_message = rust_error_canonical(missing_import_path);
    let missing_import_target_message = rust_error_canonical(missing_import_target);
    let missing_import_alias_at_eof_message = rust_error_canonical(missing_import_alias_at_eof);
    let unicode_broken_import_message = rust_error_canonical(unicode_broken_import);
    let early_import_name_string_message = rust_error_canonical(early_import_name_string);
    let early_import_alias_string_message = rust_error_canonical(early_import_alias_string);
    let early_namespace_path_message = rust_error_canonical(early_namespace_path);
    let second_string_item_message = rust_error_canonical(second_string_item);
    let second_missing_import_message = rust_error_canonical(second_missing_import);
    let missing_module_name_message = rust_error_canonical(missing_module_name);
    assert_eq!(
        missing_module_name_message,
        diagnostic_at(missing_module_name, 0, "expected module name"),
        "the Rust parser currently relocates a missing module name to the consumed module token"
    );
    let keyword_module_name_message = rust_error_canonical(keyword_module_name);
    let dotted_module_name_message = rust_error_canonical(dotted_module_name);
    let module_body_message = rust_error_canonical(module_body);
    let module_alias_message = rust_error_canonical(module_alias);
    let double_module_semicolon_message = rust_error_canonical(double_module_semicolon);
    let nested_module_message = diagnostic_for_kind(
        nested_module,
        TokenKind::Module,
        "expression form is outside the stage-2 bootstrap subset",
    );
    let unsupported_item_message = diagnostic_for_kind(
        unsupported_item,
        TokenKind::Struct,
        "stage-3 supports import, module, and ordinary function items only",
    );
    let unsupported_signature_type_message = diagnostic_at(
        unsupported_signature_type,
        5,
        "stage-3 types do not support function or async function types",
    );
    let leading_union_pipe_message = rust_error_canonical(leading_union_pipe);
    let trailing_union_pipe_message = rust_error_canonical(trailing_union_pipe);
    let nested_trailing_union_pipe_message = rust_error_canonical(nested_trailing_union_pipe);
    let return_trailing_union_pipe_message = rust_error_canonical(return_trailing_union_pipe);
    let trailing_union_at_eof_message = rust_error_canonical(trailing_union_at_eof);
    let nested_trailing_union_at_eof_message = rust_error_canonical(nested_trailing_union_at_eof);
    let double_pipe_union_message = rust_error_canonical(double_pipe_union);
    let repeated_result_union_message = rust_error_canonical(repeated_result_union);
    let dotted_message = |source| {
        diagnostic_for_kind(
            source,
            TokenKind::Dot,
            "dotted type names must start with a custom identifier",
        )
    };
    let missing_array_close_message = rust_error_canonical(missing_array_close);
    let missing_dotted_name_message = rust_error_canonical(missing_dotted_name);
    let repeated_result_message = rust_error_canonical(repeated_result);
    let rejected_type_depth_message = diagnostic_at(
        &rejected_type_depth,
        5 + 32,
        "stage-3 type nesting exceeds 32 levels",
    );
    let rejected_mixed_type_depth_message = diagnostic_at(
        &rejected_mixed_type_depth,
        5,
        "stage-3 type nesting exceeds 32 levels",
    );
    let parameter_limit_message = diagnostic_at(
        &over_parameter_limit,
        3 + 32 * 2,
        "stage-3 functions accept at most 32 parameters",
    );
    let union_limit_message = diagnostic_at(
        &over_union_limit,
        5 + 64 * 2,
        "stage-3 union types accept at most 64 members",
    );
    let array_union_limit_message = diagnostic_at(
        &over_array_union_limit,
        5 + 64 * 4,
        "stage-3 union types accept at most 64 members",
    );
    let import_limit_message = diagnostic_at(
        &over_import_limit,
        64 * 2,
        "stage-3 parser accepts at most 64 imports",
    );
    let import_name_limit_message = diagnostic_at(
        &over_import_name_limit,
        2 + 64 * 2,
        "stage-3 named imports accept at most 64 names",
    );
    let import_alias_name_limit_message = diagnostic_at(
        &over_import_alias_name_limit,
        2 + 64 * 4,
        "stage-3 named imports accept at most 64 names",
    );
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
    let module_token_limit_message = diagnostic_at(
        &over_module_token_limit,
        0,
        "stage-3 parser accepts at most 512 tokens",
    );

    for (source, code, message) in [
        (let_source, "unsupported_statement", let_message),
        (missing_body, "unexpected_eof", missing_message),
        (
            return_outside,
            "return_outside_function",
            return_outside_message,
        ),
        (top_level_expression, "expected_item", top_level_message),
        (&deep_expression, "depth_exceeded", deep_message),
        (broken_expression, "unexpected_eof", broken_message),
        (empty_rhs_expression, "unexpected_eof", empty_rhs_message),
        (
            unicode_broken_expression,
            UNICODE_CRLF_ERROR_CODE,
            unicode_broken_message,
        ),
        (
            MULTILINE_RELOCATED_ERROR_SOURCE,
            "invalid_expression",
            multiline_relocated_message,
        ),
        (
            MULTILINE_BOUNDARY_ERROR_SOURCE,
            "unexpected_eof",
            multiline_boundary_message,
        ),
        (
            missing_param_name,
            "unexpected_token",
            missing_param_name_message,
        ),
        (
            missing_param_type,
            "unexpected_token",
            missing_param_type_message,
        ),
        (
            missing_param_comma,
            "unexpected_token",
            missing_param_comma_message,
        ),
        (
            trailing_param_comma,
            "unexpected_token",
            trailing_param_comma_message,
        ),
        (
            missing_return_type,
            "unexpected_token",
            missing_return_type_message,
        ),
        (
            missing_signature_body,
            "unexpected_token",
            missing_signature_body_message,
        ),
        (
            legacy_type_alias,
            "unexpected_token",
            legacy_type_alias_message,
        ),
        (
            unicode_broken_signature,
            "unexpected_token",
            unicode_broken_signature_message,
        ),
        (
            empty_named_import,
            "unexpected_token",
            empty_named_import_message,
        ),
        (
            trailing_import_comma,
            "unexpected_token",
            trailing_import_comma_message,
        ),
        (
            missing_import_alias,
            "unexpected_token",
            missing_import_alias_message,
        ),
        (
            missing_named_from,
            "unexpected_token",
            missing_named_from_message,
        ),
        (
            missing_namespace_from,
            "unexpected_token",
            missing_namespace_from_message,
        ),
        (
            legacy_import_from,
            "unexpected_token",
            legacy_import_from_message,
        ),
        (
            invalid_import_namespace,
            "unexpected_token",
            invalid_import_namespace_message,
        ),
        (
            invalid_import_path,
            "unexpected_token",
            invalid_import_path_message,
        ),
        (
            missing_import_path,
            "unexpected_token",
            missing_import_path_message,
        ),
        (
            missing_import_target,
            "unexpected_token",
            missing_import_target_message,
        ),
        (
            missing_import_alias_at_eof,
            "unexpected_token",
            missing_import_alias_at_eof_message,
        ),
        (
            unicode_broken_import,
            "unexpected_token",
            unicode_broken_import_message,
        ),
        (
            early_import_name_string,
            "unexpected_token",
            early_import_name_string_message,
        ),
        (
            early_import_alias_string,
            "unexpected_token",
            early_import_alias_string_message,
        ),
        (
            early_namespace_path,
            "unexpected_token",
            early_namespace_path_message,
        ),
        (
            second_string_item,
            "expected_item",
            second_string_item_message,
        ),
        (
            second_missing_import,
            "unexpected_token",
            second_missing_import_message,
        ),
        (
            missing_module_name,
            "unexpected_token",
            missing_module_name_message,
        ),
        (
            keyword_module_name,
            "unexpected_token",
            keyword_module_name_message,
        ),
        (
            dotted_module_name,
            "expected_item",
            dotted_module_name_message,
        ),
        (module_body, "expected_item", module_body_message),
        (module_alias, "expected_item", module_alias_message),
        (
            double_module_semicolon,
            "expected_item",
            double_module_semicolon_message,
        ),
        (
            nested_module,
            "unsupported_expression",
            nested_module_message,
        ),
        (
            unsupported_item,
            "unsupported_item",
            unsupported_item_message,
        ),
        (
            unsupported_signature_type,
            "unsupported_type",
            unsupported_signature_type_message,
        ),
        (
            leading_union_pipe,
            "unexpected_token",
            leading_union_pipe_message,
        ),
        (
            trailing_union_pipe,
            "unexpected_token",
            trailing_union_pipe_message,
        ),
        (
            nested_trailing_union_pipe,
            "unexpected_token",
            nested_trailing_union_pipe_message,
        ),
        (
            return_trailing_union_pipe,
            "unexpected_token",
            return_trailing_union_pipe_message,
        ),
        (
            trailing_union_at_eof,
            "unexpected_token",
            trailing_union_at_eof_message,
        ),
        (
            nested_trailing_union_at_eof,
            "unexpected_token",
            nested_trailing_union_at_eof_message,
        ),
        (
            double_pipe_union,
            "unexpected_token",
            double_pipe_union_message,
        ),
        (
            repeated_result_union,
            "unexpected_token",
            repeated_result_union_message,
        ),
        (
            primitive_dotted_parameter,
            "unsupported_type",
            dotted_message(primitive_dotted_parameter),
        ),
        (
            primitive_dotted_return,
            "unsupported_type",
            dotted_message(primitive_dotted_return),
        ),
        (
            primitive_dotted_local,
            "unsupported_type",
            dotted_message(primitive_dotted_local),
        ),
        (
            missing_array_close,
            "unexpected_token",
            missing_array_close_message,
        ),
        (
            missing_dotted_name,
            "unexpected_token",
            missing_dotted_name_message,
        ),
        (repeated_result, "unexpected_token", repeated_result_message),
        (
            &over_parameter_limit,
            "parameter_limit",
            parameter_limit_message,
        ),
        (&over_union_limit, "type_union_limit", union_limit_message),
        (
            &over_array_union_limit,
            "type_union_limit",
            array_union_limit_message,
        ),
        (&over_import_limit, "import_limit", import_limit_message),
        (
            &over_import_name_limit,
            "import_name_limit",
            import_name_limit_message,
        ),
        (
            &over_import_alias_name_limit,
            "import_name_limit",
            import_alias_name_limit_message,
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
        "    ExpectError({}, \"type_depth_exceeded\", {})?\n",
        ku_string(&rejected_type_depth),
        ku_string(&rejected_type_depth_message)
    ));
    body.push_str(&format!(
        "    ExpectError({}, \"type_depth_exceeded\", {})?\n",
        ku_string(&rejected_mixed_type_depth),
        ku_string(&rejected_mixed_type_depth_message)
    ));
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
    for name in ["imports.ku", "parser.ku", "signature.ku", "support.ku"] {
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

    // Keep the exact complete-module token boundary out of the already broad
    // differential process. The interpreter's captured-array implementation
    // makes the 255-node arena materially more expensive than the small
    // semantic cases, and combining both workloads leaves no watchdog margin
    // on slower CI hosts. This focused process still exercises both sides of
    // the boundary through the same self-hosted ParseProgram implementation.
    let mut module_boundary_body =
        "import { ParseProgram } from \"./parser.ku\"\n\nfn main(): null! {\n".to_string();
    module_boundary_body.push_str(&format!(
        "    parsed = ParseProgram({})?\n    if (parsed.root != 256 || parsed.arena.nodes.len() != 256 || parsed.arena.edges.len() != 255) {{ panic(\"stage-3 module arena boundary mismatch\") }}\n",
        ku_string(&module_boundary)
    ));
    module_boundary_body.push_str(&format!(
        "    caught = false\n    try {{ ParseProgram({})? }} catch(err) {{\n        caught = true\n        if (err.domain != \"bootstrap.parser.stage3\" || err.code != \"invalid_token_stream\" || err.message != {}) {{ panic(\"wrong stage-3 module token-limit diagnostic\") }}\n    }}\n    if (!caught) {{ panic(\"expected stage-3 module token-limit error\") }}\n    return ok(null)\n}}\n",
        ku_string(&over_module_token_limit),
        ku_string(&format!(
            "error|bootstrap.parser.stage3|invalid_token_stream|<source>|{module_token_limit_message}"
        ))
    ));
    let module_boundary_entry = stage3.join("module_boundary.ku");
    fs::write(&module_boundary_entry, module_boundary_body)
        .expect("write focused stage-3 module boundary harness");
    let module_boundary_arg = module_boundary_entry.to_string_lossy().to_string();
    run_ku(&["check", &module_boundary_arg]);
    run_ku(&["run", &module_boundary_arg]);

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
