//! Stage-3 self-hosted statement/declaration parser differential gate.
//!
//! Stage 3 deliberately accepts a strict subset of production Ku syntax. Every
//! accepted fixture is parsed by the Rust frontend and projected into the same
//! bounded NodeId/edge arena used by the Ku implementation.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, OutputLimits};
use ku::ast::{
    BinaryOp, EnumDecl, Expr, ExprKind, FnDecl, ImportDecl, ImportKind, Item, Literal, ModuleDecl,
    Stmt, StructDecl, TypeName, UnaryOp,
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
const STRUCT_GOLDEN_SOURCE: &str =
    "// 前😀\r\nstruct User {\r\n  name: str,\r\n  tags: [pkg.Tag!]!\r\n}";
const STRUCT_GOLDEN: &str = concat!(
    "ROOT|9\n",
    "NODE|1|TypeName|str|0|3:9@35..3:12@38|0|0\n",
    "NODE|2|StructField|name|0|3:3@29..3:7@33|0|1\n",
    "NODE|3|TypeName|pkg.Tag|0|4:10@50..4:17@57|1|0\n",
    "NODE|4|TypeResult||0|4:10@50..4:18@58|1|1\n",
    "NODE|5|TypeArray||0|4:9@49..4:19@59|2|1\n",
    "NODE|6|TypeResult||0|4:9@49..4:20@60|3|1\n",
    "NODE|7|StructField|tags|0|4:3@43..4:7@47|4|1\n",
    "NODE|8|Struct|User|0|2:1@12..5:2@63|5|2\n",
    "NODE|9|Program||0|2:1@12..5:2@63|7|1\n",
    "EDGE|0|1\n",
    "EDGE|1|3\n",
    "EDGE|2|4\n",
    "EDGE|3|5\n",
    "EDGE|4|6\n",
    "EDGE|5|2\n",
    "EDGE|6|7\n",
    "EDGE|7|8",
);
const ENUM_GOLDEN_SOURCE: &str =
    "// 前😀\r\nenum Maybe {\r\n  None,\r\n  Some(value: [pkg.Item!]!)\r\n}";
const ENUM_GOLDEN: &str = concat!(
    "ROOT|9\n",
    "NODE|1|EnumVariant|None|0|3:3@28..3:7@32|0|0\n",
    "NODE|2|TypeName|pkg.Item|0|4:16@50..4:24@58|0|0\n",
    "NODE|3|TypeResult||0|4:16@50..4:25@59|0|1\n",
    "NODE|4|TypeArray||0|4:15@49..4:26@60|1|1\n",
    "NODE|5|TypeResult||0|4:15@49..4:27@61|2|1\n",
    "NODE|6|EnumVariantField|value|0|4:8@42..4:13@47|3|1\n",
    "NODE|7|EnumVariant|Some|0|4:3@37..4:28@62|4|1\n",
    "NODE|8|Enum|Maybe|0|2:1@12..5:2@65|5|2\n",
    "NODE|9|Program||0|2:1@12..5:2@65|7|1\n",
    "EDGE|0|2\n",
    "EDGE|1|3\n",
    "EDGE|2|4\n",
    "EDGE|3|5\n",
    "EDGE|4|6\n",
    "EDGE|5|1\n",
    "EDGE|6|7\n",
    "EDGE|7|8",
);
const IF_GOLDEN_SOURCE: &str = concat!(
    "// 前😀\r\n",
    "fn main() {\r\n",
    "  if (true) {\r\n",
    "    text:str = \"中😀\"\r\n",
    "  } else {\r\n",
    "    println(text)\r\n",
    "  };\r\n",
    "}",
);
const IF_GOLDEN: &str = concat!(
    "ROOT|11\n",
    "NODE|1|Bool|true|0|3:7@31..3:11@35|0|0\n",
    "NODE|2|TypeName|str|0|4:10@49..4:13@52|0|0\n",
    "NODE|3|String|中😀|0|4:16@55..4:20@64|0|0\n",
    "NODE|4|VarDecl|text|0|4:5@44..4:20@64|0|2\n",
    "NODE|5|Variable|println|0|6:5@82..6:12@89|2|0\n",
    "NODE|6|Variable|text|0|6:13@90..6:17@94|2|0\n",
    "NODE|7|Call||0|6:5@82..6:18@95|2|2\n",
    "NODE|8|ExprStmt||0|6:5@82..6:18@95|4|1\n",
    "NODE|9|If||1|3:3@27..7:4@100|5|3\n",
    "NODE|10|Function|main|0|2:1@12..8:2@104|8|1\n",
    "NODE|11|Program||0|2:1@12..8:2@104|9|1\n",
    "EDGE|0|2\n",
    "EDGE|1|3\n",
    "EDGE|2|5\n",
    "EDGE|3|6\n",
    "EDGE|4|7\n",
    "EDGE|5|1\n",
    "EDGE|6|4\n",
    "EDGE|7|8\n",
    "EDGE|8|9\n",
    "EDGE|9|10",
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
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            span,
        } => {
            let mut children = Vec::with_capacity(1 + then_branch.len() + else_branch.len());
            children.push(project_expr(source, condition, arena));
            children.extend(
                then_branch
                    .iter()
                    .map(|statement| project_stmt(source, statement, arena)),
            );
            children.extend(
                else_branch
                    .iter()
                    .map(|statement| project_stmt(source, statement, arena)),
            );
            push_node(
                arena,
                "If",
                String::new(),
                then_branch.len() as i64,
                *span,
                &children,
            )
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

fn project_struct(source: &str, structure: &StructDecl, arena: &mut ProjectedArena) -> usize {
    let tokens = Lexer::new(source).lex().expect("struct projection lex");
    let mut cursor = tokens
        .iter()
        .position(|token| {
            matches!(token.kind, TokenKind::Struct)
                && token.span.start.offset == structure.span.start.offset
        })
        .expect("struct start token");
    cursor += 1;
    assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
    cursor += 1;
    assert!(matches!(tokens[cursor].kind, TokenKind::LBrace));
    cursor += 1;

    let mut children = Vec::with_capacity(structure.fields.len());
    for field in &structure.fields {
        assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
        assert_eq!(tokens[cursor].span, field.span);
        let field_span = tokens[cursor].span;
        cursor += 1;
        assert!(matches!(tokens[cursor].kind, TokenKind::Colon));
        cursor += 1;
        let ty = project_type(
            &tokens,
            &mut cursor,
            field.ty.as_ref().expect("struct field type"),
            arena,
        );
        children.push(push_node(
            arena,
            "StructField",
            field.name.clone(),
            0,
            field_span,
            &[ty],
        ));
        if matches!(tokens[cursor].kind, TokenKind::Comma) {
            cursor += 1;
        }
    }
    assert!(matches!(tokens[cursor].kind, TokenKind::RBrace));
    push_node(
        arena,
        "Struct",
        structure.name.clone(),
        0,
        structure.span,
        &children,
    )
}

fn project_enum(source: &str, declaration: &EnumDecl, arena: &mut ProjectedArena) -> usize {
    let tokens = Lexer::new(source).lex().expect("enum projection lex");
    let mut cursor = tokens
        .iter()
        .position(|token| {
            matches!(token.kind, TokenKind::Enum)
                && token.span.start.offset == declaration.span.start.offset
        })
        .expect("enum start token");
    cursor += 1;
    assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
    cursor += 1;
    assert!(matches!(tokens[cursor].kind, TokenKind::LBrace));
    cursor += 1;

    let mut variants = Vec::with_capacity(declaration.variants.len());
    for variant in &declaration.variants {
        assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
        let variant_start = tokens[cursor].span;
        assert_eq!(variant_start.start, variant.span.start);
        cursor += 1;

        let mut fields = Vec::with_capacity(variant.fields.len());
        let variant_span = if matches!(tokens[cursor].kind, TokenKind::LParen) {
            cursor += 1;
            for (index, field) in variant.fields.iter().enumerate() {
                assert!(matches!(tokens[cursor].kind, TokenKind::Ident(_)));
                assert_eq!(tokens[cursor].span, field.span);
                let field_span = tokens[cursor].span;
                cursor += 1;
                assert!(matches!(tokens[cursor].kind, TokenKind::Colon));
                cursor += 1;
                let ty = project_type(
                    &tokens,
                    &mut cursor,
                    field.ty.as_ref().expect("enum variant field type"),
                    arena,
                );
                fields.push(push_node(
                    arena,
                    "EnumVariantField",
                    field.name.clone(),
                    0,
                    field_span,
                    &[ty],
                ));
                if index + 1 < variant.fields.len() {
                    assert!(matches!(tokens[cursor].kind, TokenKind::Comma));
                    cursor += 1;
                }
            }
            assert!(matches!(tokens[cursor].kind, TokenKind::RParen));
            let span = Span::new(variant_start.start, tokens[cursor].span.end);
            cursor += 1;
            span
        } else {
            variant_start
        };
        assert_eq!(variant_span, variant.span);
        variants.push(push_node(
            arena,
            "EnumVariant",
            variant.name.clone(),
            0,
            variant_span,
            &fields,
        ));
        if matches!(tokens[cursor].kind, TokenKind::Comma) {
            cursor += 1;
        }
    }
    assert!(matches!(tokens[cursor].kind, TokenKind::RBrace));
    assert_eq!(tokens[cursor].span.end, declaration.span.end);
    push_node(
        arena,
        "Enum",
        declaration.name.clone(),
        0,
        declaration.span,
        &variants,
    )
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
            Item::Struct(structure) => project_struct(source, structure, &mut arena),
            Item::Enum(declaration) => project_enum(source, declaration, &mut arena),
            Item::Module(module) => project_module(module, &mut arena),
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
    assert_eq!(rust_canonical(STRUCT_GOLDEN_SOURCE), STRUCT_GOLDEN);
    assert_eq!(rust_canonical(ENUM_GOLDEN_SOURCE), ENUM_GOLDEN);
    assert_eq!(rust_canonical(IF_GOLDEN_SOURCE), IF_GOLDEN);
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
        "struct Empty {}",
        "struct WithoutCommas { id:int name:str active:bool }",
        "struct WithCommas { id:int, name:str }",
        "struct WithTrailingComma { id:int, }",
        "struct Complex { dotted:pkg.model.User, unioned:int | str, arrayed:[pkg.Item!]!, nested:[[int | pkg.Value!]!]! }",
        "struct Adjacent { dotted:pkg.model.User unioned:int | str arrayed:[pkg.Item!]! tail:bool }",
        "struct Duplicate { value:int value:str }",
        "enum Empty {}",
        "enum Unit { Ready Pending }",
        "enum Commas { First, Second, }",
        "enum EmptyPayload { Bare Empty() }",
        "enum Payload { Number(value:int) Pair(left:int, right:int) }",
        "enum Complex { Value(value:[pkg.Item!]! | null), Other(flag:bool, text:str) }",
        "enum Duplicate { Same Same(value:int, value:str) }",
        "module App\nimport \"./types.ku\"\nstruct User { id:int }\nenum State { Ready User(value:User) }\nfn main() {}",
        STRUCT_GOLDEN_SOURCE,
        ENUM_GOLDEN_SOURCE,
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
        "fn main() { if (true) {} }",
        "fn main() { if (value > 0) { left = 1 right = 2 } else { other = 3 return other } }",
        "fn main() { if (outer) { if (inner) { value = 1 } else { value = 2 } } else { if (fallback) { value = 3 } } }",
        "fn main() { if ((left + 1) * 2 >= right && ready || fallback) { return left } else {}; }",
        "fn main() { if (Check(1, (2 + 3))) {} }",
        "fn main() { before = 0 if (left) {} middle = 1 if (right) {} after = 2 }",
        "fn main() { if (outer) { before = 0 if (inner) {} after = 1 } else { fallback = 2 } }",
        "fn main() {\r\n  if (\"中😀\" == \"中😀\") {}\r\n}",
        IF_GOLDEN_SOURCE,
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
    let missing_if_lparen = "fn main() { if true {} }";
    let missing_if_rparen = "fn main() { if (true {} }";
    let empty_if_condition = "fn main() { if () {} }";
    let missing_if_close = "fn main() { if (true) { value = 1";
    let missing_else_close = "fn main() { if (true) {} else { value = 1";
    let single_if_body = "fn main() { if (true) value = 1 }";
    let single_else_body = "fn main() { if (true) {} else value = 1 }";
    let unsupported_else_if = "fn main() { if (true) {} else if (false) {} }";
    let separated_else = "fn main() { if (true) {}; else {} }";
    let stray_else = "fn main() { else {} }";
    let accepted_if_depth = format!(
        "fn main() {{{}value = 1{}}}",
        "if (true) {".repeat(32),
        "}".repeat(32)
    );
    let rejected_if_depth = format!(
        "fn main() {{{}value = 1{}}}",
        "if (true) {".repeat(33),
        "}".repeat(33)
    );
    let aggregate_statement_boundary =
        format!("fn main() {{ if (true) {{ {} }} }}", "value;".repeat(127));
    let over_aggregate_statement_limit =
        format!("fn main() {{ if (true) {{ {} }} }}", "value;".repeat(128));
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
    let accepted_enum_type_depth = format!(
        "enum Deep {{ Value(value: {}int{}) }}",
        "[".repeat(32),
        "]".repeat(32)
    );
    let rejected_enum_type_depth = format!(
        "enum Deep {{ Value(value: {}int{}) }}",
        "[".repeat(33),
        "]".repeat(33)
    );
    let enum_union_boundary = format!(
        "enum Choice {{ Value(value: {}) }}",
        (0..64)
            .map(|index| format!("T{index}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let over_enum_union_limit = format!(
        "enum Choice {{ Value(value: {}) }}",
        (0..65)
            .map(|index| format!("T{index}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
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
    let missing_struct_name = "struct";
    let missing_struct_open = "struct User";
    let missing_struct_field_name = "struct User { : int }";
    let missing_struct_field_colon = "struct User { age int }";
    let missing_struct_field_type = "struct User { age: }";
    let missing_struct_close = "struct User { age: int";
    let double_struct_field_comma = "struct User { age: int,, name: str }";
    let struct_semicolon = "struct User {};";
    let missing_enum_name = "enum";
    let missing_enum_open = "enum State";
    let missing_enum_variant_name = "enum State { : }";
    let missing_enum_field_name = "enum State { Ready(: int) }";
    let missing_enum_field_name_at_eof = "enum State { Ready(";
    let missing_enum_field_name_after_comma_at_eof = "enum State { Ready(value:int,";
    let missing_enum_field_colon = "enum State { Ready(value int) }";
    let missing_enum_field_type = "enum State { Ready(value:) }";
    let missing_enum_payload_comma = "enum State { Ready(left:int right:int) }";
    let trailing_enum_payload_comma = "enum State { Ready(value:int,) }";
    let missing_enum_payload_close = "enum State { Ready(value:int }";
    let missing_enum_close = "enum State { Ready";
    let double_enum_variant_comma = "enum State { Ready,, Pending }";
    let enum_semicolon = "enum State {};";
    let unsupported_enum_type = "enum State { Ready(op: fn(int): int) }";
    let unsupported_async_enum_type = "enum State { Ready(op: async fn(int): int) }";
    let unsupported_item = "async fn main(): null! { return ok(null) }";
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
    let struct_boundary = format!("{}module Tail;", "struct S {}\n".repeat(127));
    let over_struct_token_limit = "struct S {}\n".repeat(128);
    assert_eq!(
        Lexer::new(&struct_boundary)
            .lex()
            .expect("complete struct token boundary lex")
            .len(),
        512,
        "127 empty structs plus one terminated module must exactly fill the Stage-3 token budget"
    );
    assert_eq!(
        Lexer::new(&over_struct_token_limit)
            .lex()
            .expect("struct token over-limit lex")
            .len(),
        513,
        "128 empty structs must exceed the complete Stage-3 token budget"
    );
    let struct_field_window_boundary = format!(
        "struct Wide {{ {} }}",
        (0..169)
            .map(|index| format!("field{index}: int"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(
        Lexer::new(&struct_field_window_boundary)
            .lex()
            .expect("complete struct-field token boundary lex")
            .len(),
        512,
        "169 unseparated fields must exactly fill the direct struct token window"
    );
    let enum_boundary = format!("{}module Tail;", "enum E {}\n".repeat(127));
    let over_enum_token_limit = "enum E {}\n".repeat(128);
    assert_eq!(
        Lexer::new(&enum_boundary)
            .lex()
            .expect("complete enum token boundary lex")
            .len(),
        512,
        "127 empty enums plus one terminated module must exactly fill the Stage-3 token budget"
    );
    assert_eq!(
        Lexer::new(&over_enum_token_limit)
            .lex()
            .expect("enum token over-limit lex")
            .len(),
        513,
        "128 empty enums must exceed the complete Stage-3 token budget"
    );
    let enum_variant_window_boundary = format!(
        "enum Wide {{ {} }}",
        (0..507)
            .map(|index| format!("V{index}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(
        Lexer::new(&enum_variant_window_boundary)
            .lex()
            .expect("complete enum-variant token boundary lex")
            .len(),
        512,
        "507 unseparated unit variants must exactly fill the direct enum token window"
    );
    let enum_field_window_boundary = format!(
        "enum Wide {{ Only({}), }}",
        (0..126)
            .map(|index| format!("field{index}: int"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(
        Lexer::new(&enum_field_window_boundary)
            .lex()
            .expect("complete enum-field token boundary lex")
            .len(),
        512,
        "126 payload fields plus the outer trailing comma must exactly fill the direct enum token window"
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
    let direct_struct_source = "struct S {}";
    let direct_struct_tokens = Lexer::new(direct_struct_source)
        .lex()
        .expect("direct struct-window fixture lex");
    assert_eq!(direct_struct_tokens.len(), 5);
    let wrong_struct_start = "fn f() {}";
    let direct_struct_over_source = format!("struct {}", "x ".repeat(511));
    assert_eq!(
        Lexer::new(&direct_struct_over_source)
            .lex()
            .expect("direct struct-window over-limit fixture lex")
            .len(),
        513,
        "direct ReadStruct fixture must exceed its token window by one"
    );
    let direct_struct_early_eof_message = diagnostic_at(
        direct_struct_source,
        direct_struct_tokens.len() - 1,
        "EOF must be final in struct window",
    );
    let direct_struct_missing_eof_message = diagnostic_at(
        direct_struct_source,
        0,
        "struct window must start with struct and end with EOF",
    );
    let direct_struct_wrong_start_message = diagnostic_at(
        wrong_struct_start,
        0,
        "struct window must start with struct and end with EOF",
    );
    let direct_struct_trailing_source = "struct S {} struct T {}";
    let direct_struct_trailing_message = diagnostic_at(
        direct_struct_trailing_source,
        4,
        "struct window must contain exactly one declaration before EOF",
    );
    let direct_enum_source = "enum E { A }";
    let direct_enum_tokens = Lexer::new(direct_enum_source)
        .lex()
        .expect("direct enum-window fixture lex");
    assert_eq!(direct_enum_tokens.len(), 6);
    let wrong_enum_start = "struct S {}";
    let direct_enum_over_source = format!("enum {}", "x ".repeat(511));
    assert_eq!(
        Lexer::new(&direct_enum_over_source)
            .lex()
            .expect("direct enum-window over-limit fixture lex")
            .len(),
        513,
        "direct ReadEnum fixture must exceed its token window by one"
    );
    let direct_enum_early_eof_message = diagnostic_at(
        direct_enum_source,
        direct_enum_tokens.len() - 1,
        "EOF must be final in enum window",
    );
    let direct_enum_missing_eof_message = diagnostic_at(
        direct_enum_source,
        0,
        "enum window must start with enum and end with EOF",
    );
    let direct_enum_wrong_start_message = diagnostic_at(
        wrong_enum_start,
        0,
        "enum window must start with enum and end with EOF",
    );
    let direct_enum_trailing_source = "enum E { A } enum F { B }";
    let direct_enum_trailing_message = diagnostic_at(
        direct_enum_trailing_source,
        5,
        "enum window must contain exactly one declaration before EOF",
    );
    let direct_function_source = "fn f() {}";
    let direct_function_tokens = Lexer::new(direct_function_source)
        .lex()
        .expect("direct function-window fixture lex");
    assert_eq!(direct_function_tokens.len(), 7);
    let wrong_function_start = "struct S {}";
    let direct_function_over_source = format!("fn {}", "x ".repeat(511));
    assert_eq!(
        Lexer::new(&direct_function_over_source)
            .lex()
            .expect("direct function-window over-limit fixture lex")
            .len(),
        513,
        "direct ReadFunction fixture must exceed its token window by one"
    );
    let direct_function_early_eof_message = diagnostic_at(
        direct_function_source,
        direct_function_tokens.len() - 1,
        "EOF must be final in function window",
    );
    let direct_function_missing_eof_message = diagnostic_at(
        direct_function_source,
        0,
        "function window must start with fn and end with EOF",
    );
    let direct_function_wrong_start_message = diagnostic_at(
        wrong_function_start,
        0,
        "function window must start with fn and end with EOF",
    );
    let direct_function_trailing_source = "fn f() {} fn g() {}";
    let direct_function_trailing_message = diagnostic_at(
        direct_function_trailing_source,
        6,
        "function window must contain exactly one declaration before EOF",
    );

    let mut body = "import { Token } from \"../stage1/token.ku\"\nimport { Scan } from \"../stage1/lexer.ku\"\nimport { Node, Arena, AstCanonical, ParseOutput } from \"../stage2/ast.ku\"\nimport { ParseProgram } from \"./parser.ku\"\nimport { ReadEnum } from \"./enums.ku\"\nimport { ReadFunction } from \"./functions.ku\"\nimport { ReadImport } from \"./imports.ku\"\nimport { ReadStruct } from \"./structs.ku\"\nimport { MapTokenCharacters } from \"./support.ku\"\nimport { ParseTypeWindow } from \"./signature.ku\"\n\n".to_string();
    body.push_str(
        "fn AssertCase(source: str, expected: str): null! {\n    actual = AstCanonical(ParseProgram(source.clone())?)\n    if (actual != expected) { panic(\"stage-3 differential mismatch: \" + source + \"\\n\" + actual + \"\\nEXPECTED\\n\" + expected) }\n    return ok(null)\n}\n\n",
    );
    body.push_str(
        r#"fn RejectFunctionExpression(start: int, end: int): ParseOutput! {
    fail { domain: "bootstrap.parser.stage3.test", code: "unexpected_callback", message: "ReadFunction invoked its expression callback before validating the window" }
}

fn MalformedFunctionExpression(start: int, end: int): ParseOutput! {
    node = Node {
        kind: "Invalid", text: "", int_value: 0,
        line: 1, column: 1, offset: 0,
        end_line: 1, end_column: 2, end_offset: 1,
        first_edge: 0, edge_count: 1
    }
    nodes: [Node] = [node]
    edges: [int] = []
    return ok(ParseOutput { arena: Arena { nodes: nodes, edges: edges }, root: 1 })
}

fn ExpectFunctionWindowError(tokens: [Token], expected_detail: str): null! {
    expected_message = "error|bootstrap.parser.stage3|invalid_token_stream|<source>|" + expected_detail
    caught = false
    try { ReadFunction(tokens, 0, RejectFunctionExpression)? } catch(err) {
        caught = true
        if (err.domain != "bootstrap.parser.stage3" || err.code != "invalid_token_stream" || err.message != expected_message) {
            panic("wrong direct function-window diagnostic: " + err.domain + "/" + err.code + "/" + err.message + " EXPECTED " + expected_message)
        }
    }
    if (!caught) { panic("expected direct function-window error") }
    return ok(null)
}

fn ExpectFunctionCallbackOutputError(tokens: [Token]): null! {
    caught = false
    try { ReadFunction(tokens, 0, MalformedFunctionExpression)? } catch(err) {
        caught = true
        if (err.domain != "bootstrap.ast" || err.code != "invalid_edge_slice" || err.message != "bootstrap AST node edge slice lies outside the edge arena") {
            panic("wrong function callback-output diagnostic: " + err.domain + "/" + err.code + "/" + err.message)
        }
    }
    if (!caught) { panic("expected malformed function callback output to be rejected") }
    return ok(null)
}

"#,
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

fn ExpectStructWindowError(tokens: [Token], expected_detail: str): null! {
    expected_message = "error|bootstrap.parser.stage3|invalid_token_stream|<source>|" + expected_detail
    caught = false
    try { ReadStruct(tokens)? } catch(err) {
        caught = true
        if (err.domain != "bootstrap.parser.stage3" || err.code != "invalid_token_stream" || err.message != expected_message) {
            panic("wrong direct struct-window diagnostic: " + err.domain + "/" + err.code + "/" + err.message + " EXPECTED " + expected_message)
        }
    }
    if (!caught) { panic("expected direct struct-window error") }
    return ok(null)
}

fn ExpectEnumWindowError(tokens: [Token], expected_detail: str): null! {
    expected_message = "error|bootstrap.parser.stage3|invalid_token_stream|<source>|" + expected_detail
    caught = false
    try { ReadEnum(tokens)? } catch(err) {
        caught = true
        if (err.domain != "bootstrap.parser.stage3" || err.code != "invalid_token_stream" || err.message != expected_message) {
            panic("wrong direct enum-window diagnostic: " + err.domain + "/" + err.code + "/" + err.message + " EXPECTED " + expected_message)
        }
    }
    if (!caught) { panic("expected direct enum-window error") }
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
        "    empty_struct_window: [Token] = []\n    ExpectStructWindowError(empty_struct_window, \"struct window must start with struct and end with EOF|1:1@0..1:1@0\")?\n    direct_struct_tokens = Scan({})?\n    missing_struct_eof: [Token] = [direct_struct_tokens[0].clone(), direct_struct_tokens[1].clone(), direct_struct_tokens[2].clone(), direct_struct_tokens[3].clone()]\n    ExpectStructWindowError(missing_struct_eof, {})?\n    early_struct_eof: [Token] = [direct_struct_tokens[0].clone(), direct_struct_tokens[1].clone(), direct_struct_tokens[2].clone(), direct_struct_tokens[4].clone(), direct_struct_tokens[3].clone(), direct_struct_tokens[4].clone()]\n    ExpectStructWindowError(early_struct_eof, {})?\n    wrong_struct_start = Scan({})?\n    ExpectStructWindowError(wrong_struct_start, {})?\n    trailing_struct_tokens = Scan({})?\n    ExpectStructWindowError(trailing_struct_tokens, {})?\n    struct_window_over = Scan({})?\n    ExpectStructWindowError(struct_window_over, \"struct window accepts at most 512 tokens|1:1@0..1:1@0\")?\n",
        ku_string(direct_struct_source),
        ku_string(&direct_struct_missing_eof_message),
        ku_string(&direct_struct_early_eof_message),
        ku_string(wrong_struct_start),
        ku_string(&direct_struct_wrong_start_message),
        ku_string(direct_struct_trailing_source),
        ku_string(&direct_struct_trailing_message),
        ku_string(&direct_struct_over_source),
    ));
    body.push_str(&format!(
        "    empty_enum_window: [Token] = []\n    ExpectEnumWindowError(empty_enum_window, \"enum window must start with enum and end with EOF|1:1@0..1:1@0\")?\n    direct_enum_tokens = Scan({})?\n    missing_enum_eof: [Token] = [direct_enum_tokens[0].clone(), direct_enum_tokens[1].clone(), direct_enum_tokens[2].clone(), direct_enum_tokens[3].clone(), direct_enum_tokens[4].clone()]\n    ExpectEnumWindowError(missing_enum_eof, {})?\n    early_enum_eof: [Token] = [direct_enum_tokens[0].clone(), direct_enum_tokens[1].clone(), direct_enum_tokens[2].clone(), direct_enum_tokens[3].clone(), direct_enum_tokens[5].clone(), direct_enum_tokens[4].clone(), direct_enum_tokens[5].clone()]\n    ExpectEnumWindowError(early_enum_eof, {})?\n    wrong_enum_start = Scan({})?\n    ExpectEnumWindowError(wrong_enum_start, {})?\n    trailing_enum_tokens = Scan({})?\n    ExpectEnumWindowError(trailing_enum_tokens, {})?\n    enum_window_over = Scan({})?\n    ExpectEnumWindowError(enum_window_over, \"enum window accepts at most 512 tokens|1:1@0..1:1@0\")?\n",
        ku_string(direct_enum_source),
        ku_string(&direct_enum_missing_eof_message),
        ku_string(&direct_enum_early_eof_message),
        ku_string(wrong_enum_start),
        ku_string(&direct_enum_wrong_start_message),
        ku_string(direct_enum_trailing_source),
        ku_string(&direct_enum_trailing_message),
        ku_string(&direct_enum_over_source),
    ));
    body.push_str(&format!(
        "    empty_function_window: [Token] = []\n    ExpectFunctionWindowError(empty_function_window, \"function window must start with fn and end with EOF|1:1@0..1:1@0\")?\n    direct_function_tokens = Scan({})?\n    direct_function = ReadFunction(direct_function_tokens.clone(), 0, RejectFunctionExpression)?\n    if (direct_function.output.root != 1 || direct_function.output.arena.nodes.len() != 1 || direct_function.output.arena.edges.len() != 0 || direct_function.consumed != 6) {{ panic(\"direct empty function-window mismatch\") }}\n    missing_function_eof: [Token] = [direct_function_tokens[0].clone(), direct_function_tokens[1].clone(), direct_function_tokens[2].clone(), direct_function_tokens[3].clone(), direct_function_tokens[4].clone(), direct_function_tokens[5].clone()]\n    ExpectFunctionWindowError(missing_function_eof, {})?\n    early_function_eof: [Token] = [direct_function_tokens[0].clone(), direct_function_tokens[1].clone(), direct_function_tokens[2].clone(), direct_function_tokens[3].clone(), direct_function_tokens[4].clone(), direct_function_tokens[6].clone(), direct_function_tokens[5].clone(), direct_function_tokens[6].clone()]\n    ExpectFunctionWindowError(early_function_eof, {})?\n    wrong_function_start = Scan({})?\n    ExpectFunctionWindowError(wrong_function_start, {})?\n    trailing_function_tokens = Scan({})?\n    ExpectFunctionWindowError(trailing_function_tokens, {})?\n    function_window_over = Scan({})?\n    ExpectFunctionWindowError(function_window_over, \"function window accepts at most 512 tokens|1:1@0..1:1@0\")?\n",
        ku_string(direct_function_source),
        ku_string(&direct_function_missing_eof_message),
        ku_string(&direct_function_early_eof_message),
        ku_string(wrong_function_start),
        ku_string(&direct_function_wrong_start_message),
        ku_string(direct_function_trailing_source),
        ku_string(&direct_function_trailing_message),
        ku_string(&direct_function_over_source),
    ));
    body.push_str(
        "    callback_function_tokens = Scan(\"fn f() { x }\")?\n    ExpectFunctionCallbackOutputError(callback_function_tokens)?\n",
    );
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(TYPED_UNION_GOLDEN_SOURCE),
        ku_string(TYPED_UNION_GOLDEN)
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(ENUM_GOLDEN_SOURCE),
        ku_string(ENUM_GOLDEN)
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
        "    AssertCase({}, {})?\n",
        ku_string(&accepted_enum_type_depth),
        ku_string(&rust_canonical(&accepted_enum_type_depth))
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&accepted_if_depth),
        ku_string(&rust_canonical(&accepted_if_depth))
    ));
    body.push_str(&format!(
        "    AssertCase({}, {})?\n",
        ku_string(&enum_union_boundary),
        ku_string(&rust_canonical(&enum_union_boundary))
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
        "    nested_large = ParseProgram({})?\n    if (nested_large.root != 258 || nested_large.arena.nodes.len() != 258 || nested_large.arena.edges.len() != 257) {{ panic(\"stage-3 aggregate nested-statement arena boundary mismatch\") }}\n",
        ku_string(&aggregate_statement_boundary)
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
    let missing_if_lparen_message = diagnostic_at(missing_if_lparen, 6, "expected '(' after 'if'");
    let missing_if_rparen_message =
        diagnostic_at(missing_if_rparen, 8, "expected ')' after if condition");
    let empty_if_condition_message = diagnostic_at(empty_if_condition, 7, "expected expression");
    let missing_if_close_message = diagnostic_for_kind(
        missing_if_close,
        TokenKind::Eof,
        "expected '}' after if body",
    );
    let missing_else_close_message = diagnostic_for_kind(
        missing_else_close,
        TokenKind::Eof,
        "expected '}' after else body",
    );
    let single_if_body_message =
        diagnostic_at(single_if_body, 9, "stage-3 if bodies must use braces");
    let single_else_body_message =
        diagnostic_at(single_else_body, 12, "stage-3 else bodies must use braces");
    let unsupported_else_if_message = diagnostic_at(
        unsupported_else_if,
        12,
        "stage-3 does not support else-if yet; use 'else { if (...) { ... } }'",
    );
    let separated_else_message = diagnostic_at(
        separated_else,
        12,
        "stage-3 else must immediately follow an if body",
    );
    let stray_else_message = diagnostic_at(
        stray_else,
        5,
        "stage-3 else must immediately follow an if body",
    );
    let rejected_if_depth_token = Lexer::new(&rejected_if_depth)
        .lex()
        .expect("statement-depth fixture lex")
        .iter()
        .enumerate()
        .filter(|(_, token)| token.kind == TokenKind::If)
        .nth(32)
        .map(|(index, _)| index)
        .expect("33rd nested if token");
    let rejected_if_depth_message = diagnostic_at(
        &rejected_if_depth,
        rejected_if_depth_token,
        "stage-3 statement nesting exceeds 32 levels",
    );
    let over_aggregate_token = Lexer::new(&over_aggregate_statement_limit)
        .lex()
        .expect("aggregate statement-limit fixture lex")
        .iter()
        .enumerate()
        .filter(|(_, token)| matches!(&token.kind, TokenKind::Ident(name) if name == "value"))
        .nth(127)
        .map(|(index, _)| index)
        .expect("128th nested statement token");
    let over_aggregate_statement_message = diagnostic_at(
        &over_aggregate_statement_limit,
        over_aggregate_token,
        "stage-3 parser accepts at most 128 statements per function",
    );
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
    let missing_struct_name_message = rust_error_canonical(missing_struct_name);
    assert_eq!(
        missing_struct_name_message,
        diagnostic_at(missing_struct_name, 0, "expected struct name"),
        "the Rust parser currently relocates a missing struct name to the consumed struct token"
    );
    let missing_struct_open_message = rust_error_canonical(missing_struct_open);
    let missing_struct_field_name_message = rust_error_canonical(missing_struct_field_name);
    let missing_struct_field_colon_message = rust_error_canonical(missing_struct_field_colon);
    let missing_struct_field_type_message = rust_error_canonical(missing_struct_field_type);
    let missing_struct_close_message = rust_error_canonical(missing_struct_close);
    let double_struct_field_comma_message = rust_error_canonical(double_struct_field_comma);
    let struct_semicolon_message = rust_error_canonical(struct_semicolon);
    let missing_enum_name_message = rust_error_canonical(missing_enum_name);
    assert_eq!(
        missing_enum_name_message,
        diagnostic_at(missing_enum_name, 0, "expected enum name"),
        "the Rust parser currently relocates a missing enum name to the consumed enum token"
    );
    let missing_enum_open_message = rust_error_canonical(missing_enum_open);
    let missing_enum_variant_name_message = rust_error_canonical(missing_enum_variant_name);
    let missing_enum_field_name_message = rust_error_canonical(missing_enum_field_name);
    let missing_enum_field_name_at_eof_message =
        rust_error_canonical(missing_enum_field_name_at_eof);
    assert_eq!(
        missing_enum_field_name_at_eof_message,
        diagnostic_for_kind(
            missing_enum_field_name_at_eof,
            TokenKind::LParen,
            "expected enum variant field name"
        ),
        "the Rust parser currently relocates a missing enum field name to the consumed opening parenthesis"
    );
    let missing_enum_field_name_after_comma_at_eof_message =
        rust_error_canonical(missing_enum_field_name_after_comma_at_eof);
    assert_eq!(
        missing_enum_field_name_after_comma_at_eof_message,
        diagnostic_for_kind(
            missing_enum_field_name_after_comma_at_eof,
            TokenKind::Comma,
            "expected enum variant field name"
        ),
        "the Rust parser currently relocates a missing enum field name to the consumed field comma"
    );
    let missing_enum_field_colon_message = rust_error_canonical(missing_enum_field_colon);
    let missing_enum_field_type_message = rust_error_canonical(missing_enum_field_type);
    let missing_enum_payload_comma_message = rust_error_canonical(missing_enum_payload_comma);
    let trailing_enum_payload_comma_message = rust_error_canonical(trailing_enum_payload_comma);
    let missing_enum_payload_close_message = rust_error_canonical(missing_enum_payload_close);
    let missing_enum_close_message = rust_error_canonical(missing_enum_close);
    let double_enum_variant_comma_message = rust_error_canonical(double_enum_variant_comma);
    let enum_semicolon_message = rust_error_canonical(enum_semicolon);
    let unsupported_enum_type_message = diagnostic_for_kind(
        unsupported_enum_type,
        TokenKind::Fn,
        "stage-3 types do not support function or async function types",
    );
    let unsupported_async_enum_type_message = diagnostic_for_kind(
        unsupported_async_enum_type,
        TokenKind::Async,
        "stage-3 types do not support function or async function types",
    );
    let nested_module_message = diagnostic_for_kind(
        nested_module,
        TokenKind::Module,
        "expression form is outside the stage-2 bootstrap subset",
    );
    let unsupported_item_message = diagnostic_for_kind(
        unsupported_item,
        TokenKind::Async,
        "stage-3 supports import, module, struct, enum, and ordinary function items only",
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
    let rejected_enum_type_depth_message = diagnostic_at(
        &rejected_enum_type_depth,
        7 + 32,
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
    let enum_union_limit_message = diagnostic_at(
        &over_enum_union_limit,
        7 + 64 * 2,
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
    let struct_token_limit_message = diagnostic_at(
        &over_struct_token_limit,
        0,
        "stage-3 parser accepts at most 512 tokens",
    );
    let enum_token_limit_message = diagnostic_at(
        &over_enum_token_limit,
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
            missing_if_lparen,
            "unexpected_token",
            missing_if_lparen_message,
        ),
        (
            missing_if_rparen,
            "unexpected_token",
            missing_if_rparen_message,
        ),
        (
            empty_if_condition,
            "unexpected_eof",
            empty_if_condition_message,
        ),
        (missing_if_close, "unexpected_eof", missing_if_close_message),
        (
            missing_else_close,
            "unexpected_eof",
            missing_else_close_message,
        ),
        (
            single_if_body,
            "unsupported_statement",
            single_if_body_message,
        ),
        (
            single_else_body,
            "unsupported_statement",
            single_else_body_message,
        ),
        (
            unsupported_else_if,
            "unsupported_statement",
            unsupported_else_if_message,
        ),
        (
            separated_else,
            "unsupported_statement",
            separated_else_message,
        ),
        (stray_else, "unsupported_statement", stray_else_message),
        (
            &rejected_if_depth,
            "statement_depth_exceeded",
            rejected_if_depth_message,
        ),
        (
            &over_aggregate_statement_limit,
            "statement_limit",
            over_aggregate_statement_message,
        ),
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
            missing_struct_name,
            "unexpected_token",
            missing_struct_name_message,
        ),
        (
            missing_struct_open,
            "unexpected_token",
            missing_struct_open_message,
        ),
        (
            missing_struct_field_name,
            "unexpected_token",
            missing_struct_field_name_message,
        ),
        (
            missing_struct_field_colon,
            "unexpected_token",
            missing_struct_field_colon_message,
        ),
        (
            missing_struct_field_type,
            "unexpected_token",
            missing_struct_field_type_message,
        ),
        (
            missing_struct_close,
            "unexpected_token",
            missing_struct_close_message,
        ),
        (
            double_struct_field_comma,
            "unexpected_token",
            double_struct_field_comma_message,
        ),
        (struct_semicolon, "expected_item", struct_semicolon_message),
        (
            missing_enum_name,
            "unexpected_token",
            missing_enum_name_message,
        ),
        (
            missing_enum_open,
            "unexpected_token",
            missing_enum_open_message,
        ),
        (
            missing_enum_variant_name,
            "unexpected_token",
            missing_enum_variant_name_message,
        ),
        (
            missing_enum_field_name,
            "unexpected_token",
            missing_enum_field_name_message,
        ),
        (
            missing_enum_field_name_at_eof,
            "unexpected_token",
            missing_enum_field_name_at_eof_message,
        ),
        (
            missing_enum_field_name_after_comma_at_eof,
            "unexpected_token",
            missing_enum_field_name_after_comma_at_eof_message,
        ),
        (
            missing_enum_field_colon,
            "unexpected_token",
            missing_enum_field_colon_message,
        ),
        (
            missing_enum_field_type,
            "unexpected_token",
            missing_enum_field_type_message,
        ),
        (
            missing_enum_payload_comma,
            "unexpected_token",
            missing_enum_payload_comma_message,
        ),
        (
            trailing_enum_payload_comma,
            "unexpected_token",
            trailing_enum_payload_comma_message,
        ),
        (
            missing_enum_payload_close,
            "unexpected_token",
            missing_enum_payload_close_message,
        ),
        (
            missing_enum_close,
            "unexpected_token",
            missing_enum_close_message,
        ),
        (
            double_enum_variant_comma,
            "unexpected_token",
            double_enum_variant_comma_message,
        ),
        (enum_semicolon, "expected_item", enum_semicolon_message),
        (
            unsupported_item,
            "unsupported_item",
            unsupported_item_message,
        ),
        (
            unsupported_enum_type,
            "unsupported_type",
            unsupported_enum_type_message,
        ),
        (
            unsupported_async_enum_type,
            "unsupported_type",
            unsupported_async_enum_type_message,
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
        (
            &over_enum_union_limit,
            "type_union_limit",
            enum_union_limit_message,
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
        "    ExpectError({}, \"type_depth_exceeded\", {})?\n",
        ku_string(&rejected_enum_type_depth),
        ku_string(&rejected_enum_type_depth_message)
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
    for name in [
        "enums.ku",
        "functions.ku",
        "imports.ku",
        "parser.ku",
        "signature.ku",
        "structs.ku",
        "support.ku",
    ] {
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

    // Exercise the exact complete-token boundary with more than 64 struct
    // declarations in a focused process. Keeping this out of the broad
    // differential harness preserves the fixed per-process watchdog while
    // proving there is no second, struct-specific item-count limit.
    let mut struct_boundary_body =
        "import { ParseProgram } from \"./parser.ku\"\n\nfn main(): null! {\n".to_string();
    struct_boundary_body.push_str(&format!(
        "    parsed = ParseProgram({})?\n    if (parsed.root != 129 || parsed.arena.nodes.len() != 129 || parsed.arena.edges.len() != 128) {{ panic(\"stage-3 struct arena boundary mismatch\") }}\n",
        ku_string(&struct_boundary)
    ));
    struct_boundary_body.push_str(&format!(
        "    caught = false\n    try {{ ParseProgram({})? }} catch(err) {{\n        caught = true\n        if (err.domain != \"bootstrap.parser.stage3\" || err.code != \"invalid_token_stream\" || err.message != {}) {{ panic(\"wrong stage-3 struct token-limit diagnostic\") }}\n    }}\n    if (!caught) {{ panic(\"expected stage-3 struct token-limit error\") }}\n    return ok(null)\n}}\n",
        ku_string(&over_struct_token_limit),
        ku_string(&format!(
            "error|bootstrap.parser.stage3|invalid_token_stream|<source>|{struct_token_limit_message}"
        ))
    ));
    let struct_boundary_entry = stage3.join("struct_boundary.ku");
    fs::write(&struct_boundary_entry, struct_boundary_body)
        .expect("write focused stage-3 struct boundary harness");
    let struct_boundary_arg = struct_boundary_entry.to_string_lossy().to_string();
    run_ku(&["check", &struct_boundary_arg]);
    run_ku(&["run", &struct_boundary_arg]);

    // The same global token ceiling permits 169 no-comma fields in a single
    // valid helper window. This focused gate catches accidental field-count
    // limits and boundary scans that absorb the following field identifier.
    let mut struct_field_boundary_body = "import { Scan } from \"../stage1/lexer.ku\"\nimport { ReadStruct } from \"./structs.ku\"\n\nfn main(): null! {\n".to_string();
    struct_field_boundary_body.push_str(&format!(
        "    parsed = ReadStruct(Scan({})?)?\n    if (parsed.output.root != 339 || parsed.output.arena.nodes.len() != 339 || parsed.output.arena.edges.len() != 338 || parsed.consumed != 511) {{ panic(\"direct struct-field token boundary mismatch\") }}\n    return ok(null)\n}}\n",
        ku_string(&struct_field_window_boundary)
    ));
    let struct_field_boundary_entry = stage3.join("struct_field_boundary.ku");
    fs::write(&struct_field_boundary_entry, struct_field_boundary_body)
        .expect("write focused direct struct-field boundary harness");
    let struct_field_boundary_arg = struct_field_boundary_entry.to_string_lossy().to_string();
    run_ku(&["check", &struct_field_boundary_arg]);
    run_ku(&["run", &struct_field_boundary_arg]);

    // Empty enums consume the same four declaration tokens as empty structs.
    // Keep the exact module token boundary in its own process so the broad
    // differential gate retains watchdog margin while still proving that enum
    // declarations do not acquire a hidden item-count limit.
    let mut enum_boundary_body =
        "import { ParseProgram } from \"./parser.ku\"\n\nfn main(): null! {\n".to_string();
    enum_boundary_body.push_str(&format!(
        "    parsed = ParseProgram({})?\n    if (parsed.root != 129 || parsed.arena.nodes.len() != 129 || parsed.arena.edges.len() != 128) {{ panic(\"stage-3 enum arena boundary mismatch\") }}\n",
        ku_string(&enum_boundary)
    ));
    enum_boundary_body.push_str(&format!(
        "    caught = false\n    try {{ ParseProgram({})? }} catch(err) {{\n        caught = true\n        if (err.domain != \"bootstrap.parser.stage3\" || err.code != \"invalid_token_stream\" || err.message != {}) {{ panic(\"wrong stage-3 enum token-limit diagnostic\") }}\n    }}\n    if (!caught) {{ panic(\"expected stage-3 enum token-limit error\") }}\n    return ok(null)\n}}\n",
        ku_string(&over_enum_token_limit),
        ku_string(&format!(
            "error|bootstrap.parser.stage3|invalid_token_stream|<source>|{enum_token_limit_message}"
        ))
    ));
    let enum_boundary_entry = stage3.join("enum_boundary.ku");
    fs::write(&enum_boundary_entry, enum_boundary_body)
        .expect("write focused stage-3 enum boundary harness");
    let enum_boundary_arg = enum_boundary_entry.to_string_lossy().to_string();
    run_ku(&["check", &enum_boundary_arg]);
    run_ku(&["run", &enum_boundary_arg]);

    // Direct helper boundaries independently pin the absence of variant and
    // payload-field limits. Both inputs fill all 512 tokens including EOF.
    let mut enum_variant_boundary_body = "import { Scan } from \"../stage1/lexer.ku\"\nimport { ReadEnum } from \"./enums.ku\"\n\nfn main(): null! {\n".to_string();
    enum_variant_boundary_body.push_str(&format!(
        "    parsed = ReadEnum(Scan({})?)?\n    if (parsed.output.root != 508 || parsed.output.arena.nodes.len() != 508 || parsed.output.arena.edges.len() != 507 || parsed.consumed != 511) {{ panic(\"direct enum-variant token boundary mismatch\") }}\n    return ok(null)\n}}\n",
        ku_string(&enum_variant_window_boundary)
    ));
    let enum_variant_boundary_entry = stage3.join("enum_variant_boundary.ku");
    fs::write(&enum_variant_boundary_entry, enum_variant_boundary_body)
        .expect("write focused direct enum-variant boundary harness");
    let enum_variant_boundary_arg = enum_variant_boundary_entry.to_string_lossy().to_string();
    run_ku(&["check", &enum_variant_boundary_arg]);
    run_ku(&["run", &enum_variant_boundary_arg]);

    let mut enum_field_boundary_body = "import { Scan } from \"../stage1/lexer.ku\"\nimport { ReadEnum } from \"./enums.ku\"\n\nfn main(): null! {\n".to_string();
    enum_field_boundary_body.push_str(&format!(
        "    parsed = ReadEnum(Scan({})?)?\n    if (parsed.output.root != 254 || parsed.output.arena.nodes.len() != 254 || parsed.output.arena.edges.len() != 253 || parsed.consumed != 511) {{ panic(\"direct enum-field token boundary mismatch\") }}\n    return ok(null)\n}}\n",
        ku_string(&enum_field_window_boundary)
    ));
    let enum_field_boundary_entry = stage3.join("enum_field_boundary.ku");
    fs::write(&enum_field_boundary_entry, enum_field_boundary_body)
        .expect("write focused direct enum-field boundary harness");
    let enum_field_boundary_arg = enum_field_boundary_entry.to_string_lossy().to_string();
    run_ku(&["check", &enum_field_boundary_arg]);
    run_ku(&["run", &enum_field_boundary_arg]);

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
