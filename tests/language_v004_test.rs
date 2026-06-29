use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::ast::{ExprKind, Item, Stmt};
use ku::cli::{check_source, run_cli, run_source};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::token::TokenKind;

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should work")
            .as_nanos()
    ))
}

fn ku_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

fn check_err(source: &str) -> String {
    check_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

fn run_err(source: &str) -> String {
    run_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

#[test]
fn lexer_tokenizes_v004_symbols_without_breaking_float() {
    let tokens = Lexer::new(
        "struct S { xs:[int] } enum E { A } for x in [1.5] { fs.read i++ i-- break continue x?.y }",
    )
    .tokenize()
    .expect("lex should pass");

    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Struct)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Enum)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::For)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::In)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::LBracket)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::RBracket)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Dot)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::PlusPlus)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::MinusMinus)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Break)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Continue)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::QuestionDot)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Float(value) if value == 1.5)));
}

#[test]
fn parser_builds_arrays_for_structs_and_top_level_types() {
    let source = r#"
module demo
struct Token { kind: str line: int }
enum TokenKind { Ident Number }
fn main() {
    token = Token { kind: "Ident", line: 1 }
    xs:[int] = [1, 2]
    print(token.kind)
    print(xs[0])
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex should pass");
    let program = Parser::new(tokens).parse().expect("parse should pass");

    assert!(matches!(program.items[0], Item::Module(_)));
    assert!(matches!(program.items[1], Item::Struct(_)));
    assert!(matches!(program.items[2], Item::Enum(_)));
    let Item::Function(function) = &program.items[3] else {
        panic!("expected function item");
    };
    assert!(function.body.iter().any(|stmt| matches!(
        stmt,
        Stmt::VarDecl {
            value,
            ..
        } if matches!(value.kind, ExprKind::Array(_))
    )));
}

#[test]
fn checker_and_interpreter_accept_arrays_for_and_structs() {
    let source = r#"
struct Token {
    kind: str
    line: int
}

fn main() {
    values:[int] = [1, 2, 3]
    total:int = 0
    for value in values {
        total = total + value
    }
    token = Token { kind: "Ident", line: total }
    print(token.kind)
    print(values[1])
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");
}

#[test]
fn empty_arrays_and_empty_blocks_keep_their_context() {
    let source = r#"
fn main() {
    values:[int] = []
    more:[int]
    ok = true
    if (ok) {
    };
    while (false) {
    };
    for value in values {
    };
    print(len(values) + len(more))
}
"#;

    check_source("inline.ku", source).expect("empty arrays and blocks should check");
    run_source("inline.ku", source).expect("empty arrays and blocks should run");
}

#[test]
fn checker_rejects_invalid_arrays_and_structs() {
    for source in [
        r#"fn main() { xs:[int] = [1, "bad"] }"#,
        r#"fn main() { xs:[int] = [1]; print(xs["0"]) }"#,
        r#"struct User { name: str } fn main() { user = User { name: "Ku", age: 1 } }"#,
        r#"struct User { name: str age: int } fn main() { user = User { name: "Ku" } }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("error:"), "unexpected error: {err}");
    }
}

#[test]
fn enum_unit_variants_are_values() {
    let source = r#"
enum TokenKind {
    Ident
    Eof
}

fn main() {
    kind = TokenKind.Ident
    print(kind)
    print(kind == TokenKind.Ident)
}
"#;

    check_source("inline.ku", source).expect("enum should check");
    run_source("inline.ku", source).expect("enum should run");
}

#[test]
fn enum_type_annotations_and_payload_variants_work() {
    let source = r#"
enum TokenKind {
    Ident
}

fn main() {
    kind:TokenKind = TokenKind.Ident
    print(kind)
}
"#;
    check_source("inline.ku", source).expect("enum annotation should check");
    run_source("inline.ku", source).expect("enum annotation should run");

    let payload = r#"
enum Expr {
    Number(value: int)
    Text(value: str)
}

fn main() {
    expr = Expr.Number(7)
    text = match expr {
        Expr.Number(value) => str(value)
        Expr.Text(value) => value
        _ => "none"
    }
    print(text)
}
"#;
    check_source("inline.ku", payload).expect("payload enum should check");
    run_source("inline.ku", payload).expect("payload enum should run");

    let err = check_err(
        r#"
enum Expr { Number(value: int) }
fn main() { expr = Expr.Number("bad") }
"#,
    );
    assert!(
        err.contains("expected int"),
        "unexpected payload enum error: {err}"
    );
}

#[test]
fn top_level_names_and_unknown_types_are_rejected() {
    for source in [
        r#"struct Same { x:int } enum Same { A } fn main() { print(1) }"#,
        r#"fn Same() {} struct Same { x:int } fn main() { print(1) }"#,
        r#"struct Box { value: Missing } fn main() { print(1) }"#,
        r#"fn use_missing(value: Missing) {} fn main() { print(1) }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("error:"), "unexpected error: {err}");
    }
}

#[test]
fn local_values_shadow_builtin_modules() {
    let source = r#"
struct Reader {
    read: int
}

fn main() {
    fs = Reader { read: 7 }
    print(fs.read)
}
"#;

    check_source("inline.ku", source).expect("local fs value should shadow builtin module");
    run_source("inline.ku", source).expect("local fs value should run");

    let err = check_err(
        r#"
struct Reader {
    read: int
}

fn main() {
    fs = Reader { read: 7 }
    fs.read("x")
}
"#,
    );
    assert!(
        err.contains("cannot call"),
        "unexpected shadowing error: {err}"
    );
}

#[test]
fn builtin_compiler_pipeline_and_fs_read_work() {
    let path = unique_temp_path("read.txt");
    fs::write(&path, "fn main() { print(1) }").expect("write temp file");
    let source = format!(
        r#"
import "std.fs"

fn main() {{
    text = fs.read("{}")
    tokens = lexer.scan(text)
    ast = parser.parse(tokens)
    print(len(tokens))
    print(ast)
}}
"#,
        ku_string(&path)
    );

    check_source("inline.ku", &source).expect("pipeline should check");
    run_source("inline.ku", &source).expect("pipeline should run");
    let _ = fs::remove_file(path);
}

#[test]
fn fs_read_resolves_paths_relative_to_source_file() {
    let dir = unique_temp_path("relative-read");
    fs::create_dir_all(&dir).expect("create temp dir");
    let data = dir.join("data.txt");
    let main = dir.join("main.ku");
    fs::write(&data, "relative-ok").expect("write data");
    fs::write(
        &main,
        r#"
import "std.fs"

fn main() {
    print(fs.read("data.txt"))
}
"#,
    )
    .expect("write ku file");

    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("relative fs.read should run");

    let _ = fs::remove_file(data);
    let _ = fs::remove_file(main);
    let _ = fs::remove_dir(dir);
}

#[test]
fn runtime_rejects_array_bounds_and_missing_files() {
    let err = run_err("fn main() { xs:[int] = [1]; print(xs[2]) }");
    assert!(
        err.contains("array index out of bounds"),
        "unexpected error: {err}"
    );

    let err = run_err(r#"import "std.fs" fn main() { print(fs.read("definitely-missing.ku")) }"#);
    assert!(err.contains("failed to read"), "unexpected error: {err}");
}

#[test]
fn runtime_rejects_integer_overflow_without_panicking() {
    for source in [
        "fn main() { print(9223372036854775807 + 1) }",
        "fn main() { print(-(-9223372036854775807 - 1)) }",
        "fn main() { print((-9223372036854775807 - 1) / -1) }",
        "fn main() { print((-9223372036854775807 - 1) % -1) }",
    ] {
        let err = run_err(source);
        assert!(err.contains("integer overflow"), "unexpected error: {err}");
    }
}

#[test]
fn compiler_builtins_have_resource_limits() {
    let huge_source = "x ".repeat(100_001);
    let source = format!(
        r#"
fn main() {{
    text = "{}"
    print(lexer.scan(text))
}}
"#,
        huge_source
    );
    let err = run_err(&source);
    assert!(
        err.contains("too large") || err.contains("too many tokens"),
        "unexpected lexer.scan resource error: {err}"
    );

    let source = format!(
        r#"
fn main() {{
    text = "{}"
    print(parser.parse(text))
}}
"#,
        huge_source
    );
    let err = run_err(&source);
    assert!(
        err.contains("too large") || err.contains("too many tokens"),
        "unexpected parser.parse resource error: {err}"
    );
}

#[test]
fn if_and_while_require_parenthesized_conditions() {
    for source in [
        "fn main() { if true { print(1) } }",
        "fn main() { while false { print(1) } }",
    ] {
        let err = check_err(source);
        assert!(err.contains("expected '('"), "unexpected error: {err}");
    }

    let source = r#"
fn main() {
    if (true) {
        print(1)
    }
    while (false) {
        print(2)
    }
}
"#;
    check_source("inline.ku", source).expect("parenthesized conditions should check");
    run_source("inline.ku", source).expect("parenthesized conditions should run");
}

#[test]
fn object_literals_support_fields_and_errors() {
    let source = r#"
fn main() {
    user = { name: "Ku", age: 1 }
    print(user.name)
    print(user.age)
}
"#;
    check_source("inline.ku", source).expect("object literal should check");
    run_source("inline.ku", source).expect("object literal should run");

    let err = check_err(r#"fn main() { user = { name: "Ku", name: "Lang" } }"#);
    assert!(err.contains("duplicate field"), "unexpected error: {err}");

    let err = check_err(r#"fn main() { user = { name: "Ku" }; print(user.age) }"#);
    assert!(
        err.contains("object has no field"),
        "unexpected error: {err}"
    );
}

#[test]
fn objects_work_as_string_keyed_maps_and_strings_are_indexable() {
    let source = r##"
fn main() {
    user = { name: "Ku" }
    if (user["name"] != "Ku") {
        panic("bad map read")
    }
    if (user["missing"]? != null) {
        panic("missing map key should be null")
    }
    user["age"] = 12
    if (user["age"] != 12) {
        panic("bad map write")
    }
    text = "Ku"
    if (text[0] != "K") {
        panic("bad string index")
    }
}
"##;

    check_source("inline.ku", source).expect("map/string index should check");
    run_source("inline.ku", source).expect("map/string index should run");

    for source in [
        r#"fn main() { value = { name: "Ku" }; print(value[0]) }"#,
        r##"fn main() { text = "Ku"; print(text["0"]) }"##,
        r#"fn main() { value = 1; print(value[0]) }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("type error:"), "unexpected index error: {err}");
    }
}

#[test]
fn closures_capture_outer_locals_by_reference() {
    let source = r#"
fn main() {
    prefix = "Hi "
    say = (name) => {
        return prefix + name
    }
    prefix = "Bye "
    print(say("Ku"))

    base = 10
    fn add(value: int): int {
        return base + value
    }
    base = 20
    print(add(5))

    count = 0
    inc = () => {
        count = count + 1
        return count
    }
    inc()
    inc()
    if (count != 2) {
        panic("closure did not update outer variable")
    }
}
"#;

    check_source("inline.ku", source).expect("closures should check");
    run_source("inline.ku", source).expect("closures should run");
}

#[test]
fn local_function_can_call_itself() {
    let source = r#"
fn main() {
    fn fact(n: int): int {
        if (n <= 1) {
            return 1
        }
        return n * fact(n - 1)
    }
    value = fact(5)
    if (value != 120) {
        panic("local recursion failed")
    }
}
"#;

    check_source("inline.ku", source).expect("local recursive function should check");
    run_source("inline.ku", source).expect("local recursive function should run");
}

#[test]
fn arrays_structs_and_objects_support_field_assignment() {
    let source = r#"
struct User {
    name: str
    age: int
}

fn main() {
    values:[int] = [1, 2, 3]
    values[1] = 9
    user = User { name: "Ku", age: 3 }
    user.age = values[1]
    obj = { name: "old" }
    obj.name = "new"
    print(values[1])
    print(user.age)
    print(obj.name)
}
"#;
    check_source("inline.ku", source).expect("assignment targets should check");
    run_source("inline.ku", source).expect("assignment targets should run");

    for source in [
        r#"fn main() { xs:[int] = [1]; xs[0] = "bad" }"#,
        r#"fn main() { xs:[int] = [1]; xs["0"] = 2 }"#,
        r#"struct User { age:int } fn main() { user = User { age: 1 }; user.name = "bad" }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("error:"), "unexpected assignment error: {err}");
    }
}

#[test]
fn match_supports_literals_wildcard_and_enum_payloads() {
    let source = r#"
enum Result {
    Ok(value: int)
    Err(message: str)
}

fn main() {
    value = Result.Ok(42)
    text = match value {
        Result.Ok(n) => str(n)
        Result.Err(message) => message
    }
    print(text)

    label = match 2 {
        1 => "one"
        _ => "other"
    }
    print(label)
}
"#;

    check_source("inline.ku", source).expect("match should check");
    run_source("inline.ku", source).expect("match should run");

    let err = check_err(r#"fn main() { value = switch 1 { _ => 1 } }"#);
    assert!(
        err.contains("switch is not supported; use match"),
        "unexpected switch error: {err}"
    );

    let err = check_err(
        r#"
enum Result { Ok(value:int) }
fn main() {
    value = Result.Ok(1)
    text = match value {
        Result.Ok() => "bad"
    }
}
"#,
    );
    assert!(
        err.contains("expects 1 fields"),
        "unexpected match error: {err}"
    );
}

#[test]
fn local_and_global_functions_are_checked_and_callable() {
    let local = r#"
fn main() {
    fn go(name: str, age: int) {
        print(`我是{name},我{age}岁了`)
    }
    go("Ku", 3)
}
"#;
    check_source("inline.ku", local).expect("local function should check");
    run_source("inline.ku", local).expect("local function should run");

    let global = r#"
fn go(name: str, age: int) {
    print(`我是{name},我{age}岁了`)
}

fn main() {
    go("Ku", 3)
}
"#;
    check_source("inline.ku", global).expect("global function should check");
    run_source("inline.ku", global).expect("global function should run");

    let err = check_err(r#"fn main() { fn go(name: str, age: int) { print(name) } go() }"#);
    assert!(
        err.contains("function value 'go' expects 2 arguments but got 0"),
        "unexpected local function arg error: {err}"
    );

    let err = check_err(r#"fn go(name: str) { print(name) } fn main() { go() }"#);
    assert!(
        err.contains("function 'go' expects 1 arguments but got 0"),
        "unexpected global function arg error: {err}"
    );
}

#[test]
fn check_reports_core_syntax_and_semantic_errors() {
    let cases = [
        ("fn main() { print(@) }", "unexpected character"),
        ("fn main() { print(\"unterminated) }", "unterminated string"),
        ("fn main() { print((1 + 2) }", "expected ')'"),
        ("return 1", "return outside function"),
        (
            "fn main() { print(missing) }",
            "undefined variable 'missing'",
        ),
        (
            "fn greet(name: str) { print(name) } fn main() { greet() }",
            "function 'greet' expects 1 arguments but got 0",
        ),
    ];

    for (source, expected) in cases {
        let err = check_err(source);
        assert!(
            err.contains(expected),
            "expected error containing {expected:?}, got: {err}"
        );
    }
}

#[test]
fn cli_rejects_oversized_source_files_before_lexing() {
    let path = unique_temp_path("oversized-source").with_extension("ku");
    fs::write(&path, " ".repeat(1_000_001)).expect("write oversized source");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        path.to_string_lossy().to_string(),
    ])
    .expect_err("oversized source should fail")
    .to_string();
    assert!(
        err.contains("source file too large"),
        "unexpected oversized source error: {err}"
    );

    let _ = fs::remove_file(path);
}

#[test]
fn imports_support_named_namespace_and_glob_forms() {
    let dir = unique_temp_path("imports");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let math = dir.join("math.ku");
    let named = dir.join("named.ku");
    let alias = dir.join("alias.ku");
    let absolute = dir.join("absolute.ku");
    let namespace = dir.join("namespace.ku");
    let glob = dir.join("glob.ku");
    let imported_return = dir.join("imported_return.ku");
    fs::write(
        &math,
        r#"
fn Add(a: int, b: int): int {
    return a + b
}

fn Twice(a: int): int {
    return a + a
}

fn Label(): str {
    return "Ku"
}

fn hidden(): int {
    return 99
}
"#,
    )
    .expect("write math module");
    fs::write(
        &named,
        r#"
import { Add } from "./math.ku"

fn main() {
    print(Add(1, 2))
}
"#,
    )
    .expect("write named import");
    fs::write(
        &alias,
        r#"
import { Add as Plus } from "./math"

fn main() {
    print(Plus(1, 2))
}
"#,
    )
    .expect("write alias import");
    fs::write(
        &absolute,
        format!(
            r#"
import {{ Add }} from "{}"

fn main() {{
    print(Add(7, 8))
}}
"#,
            ku_string(&math)
        ),
    )
    .expect("write absolute import");
    fs::write(
        &namespace,
        r#"
import math from "./math"

fn main() {
    print(math.Add(3, 4))
}
"#,
    )
    .expect("write namespace import");
    fs::write(
        &glob,
        r#"
import "./math.ku"

fn main() {
    print(Add(5, 6))
    print(Twice(4))
}
"#,
    )
    .expect("write glob import");
    fs::write(
        &imported_return,
        r#"
import "./math.ku"

fn main() {
    print(Label())
}
"#,
    )
    .expect("write imported return program");

    for source in [
        &named,
        &alias,
        &absolute,
        &namespace,
        &glob,
        &imported_return,
    ] {
        run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            source.to_string_lossy().to_string(),
        ])
        .expect("imported program should run");
    }

    let _ = fs::remove_file(math);
    let _ = fs::remove_file(named);
    let _ = fs::remove_file(alias);
    let _ = fs::remove_file(absolute);
    let _ = fs::remove_file(namespace);
    let _ = fs::remove_file(glob);
    let _ = fs::remove_file(imported_return);
    let _ = fs::remove_dir(dir);
}

#[test]
fn import_errors_use_imported_file_diagnostic_context() {
    let dir = unique_temp_path("bad-import-diagnostic");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let lib = dir.join("badlib.ku");
    let main = dir.join("main.ku");
    fs::write(
        &lib,
        r#"
fn Bad(): int {
    return "x"
}
"#,
    )
    .expect("write bad lib");
    fs::write(
        &main,
        r#"
import { Bad } from "./badlib.ku"

fn main() {
    print(Bad())
}
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("bad imported module should fail")
    .to_string();
    assert!(
        err.contains("badlib.ku"),
        "diagnostic should point to imported file: {err}"
    );
    assert!(
        err.contains("return \"x\""),
        "diagnostic should show imported source line: {err}"
    );
    assert!(
        !err.contains("print(Bad())"),
        "diagnostic should not render entry source line: {err}"
    );

    let _ = fs::remove_file(lib);
    let _ = fs::remove_file(main);
    let _ = fs::remove_dir(dir);
}

#[test]
fn imports_reject_bad_forms_private_names_and_cycles() {
    let dir = unique_temp_path("bad-imports");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let lib = dir.join("lib.ku");
    let bad_form = dir.join("bad_form.ku");
    let private_import = dir.join("private.ku");
    let a = dir.join("a.ku");
    let b = dir.join("b.ku");
    fs::write(
        &lib,
        r#"
fn Public(): int { return 1 }
fn private(): int { return 2 }
"#,
    )
    .expect("write lib");
    fs::write(
        &bad_form,
        r#"import from "./lib.ku" fn main() { print(1) }"#,
    )
    .expect("write bad form");
    fs::write(
        &private_import,
        r#"
import { private } from "./lib.ku"
fn main() { print(private()) }
"#,
    )
    .expect("write private import");
    fs::write(
        &a,
        r#"
import "./b.ku"
fn main() { print(1) }
"#,
    )
    .expect("write a");
    fs::write(
        &b,
        r#"
import "./a.ku"
fn B(): int { return 1 }
"#,
    )
    .expect("write b");

    for source in [&bad_form, &private_import, &a] {
        let err = run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            source.to_string_lossy().to_string(),
        ])
        .expect_err("bad import should fail")
        .to_string();
        assert!(
            err.contains("import") || err.contains("circular"),
            "unexpected error: {err}"
        );
    }

    let _ = fs::remove_file(lib);
    let _ = fs::remove_file(bad_form);
    let _ = fs::remove_file(private_import);
    let _ = fs::remove_file(a);
    let _ = fs::remove_file(b);
    let _ = fs::remove_dir(dir);
}

#[test]
fn lowercase_user_top_level_names_are_private_but_diagnostic_suggests_export_form() {
    let dir = unique_temp_path("private-import-help");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let lib = dir.join("lib.ku");
    let main = dir.join("main.ku");
    fs::write(&lib, "fn helper(): int { return 1 }\n").expect("write lib");
    fs::write(
        &main,
        r#"
import { helper } from "./lib.ku"
fn main() { print(helper()) }
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("lowercase user export should fail")
    .to_string();
    assert!(err.contains("not exported"), "unexpected error: {err}");
    assert!(
        err.contains("starts with an uppercase ASCII letter"),
        "missing export help: {err}"
    );
    assert!(err.contains("main.ku:2:"), "missing import location: {err}");

    let _ = fs::remove_file(lib);
    let _ = fs::remove_file(main);
    let _ = fs::remove_dir(dir);
}

#[test]
fn imports_keep_private_helpers_inside_imported_module() {
    let dir = unique_temp_path("import-helpers");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let lib = dir.join("lib.ku");
    let main = dir.join("main.ku");
    let namespace = dir.join("namespace.ku");
    fs::write(
        &lib,
        r#"
fn Public(value: int): int {
    return helper(value)
}

fn helper(value: int): int {
    return value + 1
}
"#,
    )
    .expect("write lib");
    fs::write(
        &main,
        r#"
import { Public } from "./lib.ku"

fn main() {
    print(Public(4))
}
"#,
    )
    .expect("write main");
    fs::write(
        &namespace,
        r#"
import lib from "./lib.ku"

fn main() {
    print(lib.Public(5))
}
"#,
    )
    .expect("write namespace");

    for source in [&main, &namespace] {
        run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            source.to_string_lossy().to_string(),
        ])
        .expect("imported public function should keep private helper");
    }

    let _ = fs::remove_file(lib);
    let _ = fs::remove_file(main);
    let _ = fs::remove_file(namespace);
    let _ = fs::remove_dir(dir);
}

#[test]
fn named_imports_only_expose_requested_types() {
    let dir = unique_temp_path("import-types");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let lib = dir.join("lib.ku");
    let good = dir.join("good.ku");
    let bad = dir.join("bad.ku");
    fs::write(
        &lib,
        r#"
struct User {
    name: str
}

fn Public(): int {
    return 1
}
"#,
    )
    .expect("write lib");
    fs::write(
        &good,
        r#"
import { User } from "./lib.ku"

fn main() {
    user = User { name: "Ku" }
    print(user.name)
}
"#,
    )
    .expect("write good");
    fs::write(
        &bad,
        r#"
import { Public } from "./lib.ku"

fn main() {
    user = User { name: "Ku" }
    print(Public())
}
"#,
    )
    .expect("write bad");

    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        good.to_string_lossy().to_string(),
    ])
    .expect("explicitly imported struct should run");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        bad.to_string_lossy().to_string(),
    ])
    .expect_err("unimported struct should not leak")
    .to_string();
    assert!(
        err.contains("undefined struct 'User'"),
        "unexpected error: {err}"
    );

    let _ = fs::remove_file(lib);
    let _ = fs::remove_file(good);
    let _ = fs::remove_file(bad);
    let _ = fs::remove_dir(dir);
}

#[test]
fn namespace_imports_expose_structs_and_enums_by_path() {
    let dir = unique_temp_path("namespace-types");
    fs::create_dir_all(&dir).expect("create temp import dir");
    let lib = dir.join("lib.ku");
    let main = dir.join("main.ku");
    fs::write(
        &lib,
        r#"
struct User {
    name: str
}

enum State {
    Ready
    Count(value: int)
}

fn Make(name: str): User {
    return User { name: name }
}
"#,
    )
    .expect("write lib");
    fs::write(
        &main,
        r##"
import lib from "./lib.ku"

fn show(user: lib.User): str {
    return user.name
}

fn main() {
    user = lib.User { name: "Ku" }
    user.name = show(lib.Make("Lang"))
    state = lib.State.Count(3)
    text = match state {
        lib.State.Count(value) => str(value)
        lib.State.Ready => "ready"
    }
    print(user.name)
    print(text)
}
"##,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("namespace struct and enum paths should run");

    let _ = fs::remove_file(lib);
    let _ = fs::remove_file(main);
    let _ = fs::remove_dir(dir);
}

#[test]
fn ku_build_creates_runnable_executable_wrapper() {
    let dir = unique_temp_path("build");
    fs::create_dir_all(&dir).expect("create temp build dir");
    let source = dir.join("main.ku");
    fs::write(&source, "fn main() { print(\"built\") }").expect("write ku source");

    run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        source.to_string_lossy().to_string(),
    ])
    .expect("ku build should succeed");

    let mut exe = dir.join(".ku").join("build").join("debug").join("main");
    if cfg!(windows) {
        exe.set_extension("exe");
    }
    assert!(exe.exists(), "expected built exe at {}", exe.display());
    let output = Command::new(&exe).output().expect("run built exe");
    assert!(output.status.success(), "built exe should run");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("built"),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ku_build_supports_output_path_alias_and_package_manifest_entry() {
    let dir = unique_temp_path("build-options");
    fs::create_dir_all(&dir).expect("create temp build dir");
    let source = dir.join("single.ku");
    fs::write(&source, "fn main() { print(\"single\") }").expect("write ku source");
    let mut explicit = dir.join("dist").join("single-bin");
    if cfg!(windows) {
        explicit.set_extension("exe");
    }
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        "build".to_string(),
        "-o".to_string(),
        explicit.to_string_lossy().to_string(),
        source.to_string_lossy().to_string(),
    ])
    .expect("ku run build alias should build");
    let output = Command::new(&explicit).output().expect("run explicit exe");
    assert!(output.status.success(), "explicit exe should run");
    assert!(String::from_utf8_lossy(&output.stdout).contains("single"));

    let package_dir = dir.join("pkg");
    let src = package_dir.join("src");
    fs::create_dir_all(&src).expect("create package src");
    fs::write(
        package_dir.join("ku.mod"),
        "name = \"build_pkg\"\nversion = \"0.1.0\"\nroot = \"src\"\nmain = \"app.ku\"\nout = \"dist\"\n",
    )
    .expect("write ku.mod");
    fs::write(src.join("app.ku"), "fn main() { print(\"package\") }").expect("write app");
    run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--release".to_string(),
        "--emit-ir".to_string(),
        package_dir.to_string_lossy().to_string(),
    ])
    .expect("ku build package should succeed");
    let mut package_exe = package_dir.join("dist").join("release").join("build_pkg");
    if cfg!(windows) {
        package_exe.set_extension("exe");
    }
    assert!(
        package_exe.exists(),
        "expected package exe at {}",
        package_exe.display()
    );
    assert!(
        package_dir
            .join("dist")
            .join("release")
            .join("ir")
            .join("main.ir")
            .exists(),
        "expected emitted Ku IR artifact"
    );
    let output = Command::new(&package_exe)
        .output()
        .expect("run package exe");
    assert!(output.status.success(), "package exe should run");
    assert!(String::from_utf8_lossy(&output.stdout).contains("package"));

    let _ = fs::remove_dir_all(dir);
}
