#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::ast::{ExprKind, FunctionParam, Item, ParamMode, Program, Stmt, TypeName};
use ku::lexer::Lexer;
use ku::parser::Parser;

fn parse(source: &str) -> Program {
    Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap_or_else(|error| panic!("{source}: {error:?}"))
}

fn function(source: &str) -> ku::ast::FnDecl {
    let mut program = parse(source);
    let Item::Function(function) = program.items.remove(0) else {
        panic!("expected function")
    };
    function
}

fn expression_params(source: &str) -> Vec<FunctionParam> {
    let expr = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_expression_only()
        .unwrap_or_else(|error| panic!("{source}: {error:?}"));
    let ExprKind::Function { params, .. } = expr.kind else {
        panic!("expected function expression")
    };
    params
}

#[test]
fn borrow_parser_named_generic_and_mixed_parameters_preserve_mode_and_span() {
    let source = "fn inspect<T>(&a: T, b: int, &c) {}";
    let function = function(source);
    assert_eq!(function.type_params, ["T"]);
    assert_eq!(
        function.params.iter().map(|p| p.mode).collect::<Vec<_>>(),
        [ParamMode::View, ParamMode::Owned, ParamMode::View]
    );
    assert_eq!(function.params[0].ty, Some(TypeName::Custom("T".into())));
    assert_eq!(function.params[1].ty, Some(TypeName::Int));
    assert_eq!(function.params[2].ty, None);
    for (param, expected) in function.params.iter().zip(["&a", "b", "&c"]) {
        assert_eq!(
            &source[param.span.start.offset..param.span.end.offset],
            expected
        );
    }
}

#[test]
fn borrow_parser_local_anonymous_and_parenthesized_arrow_parameters() {
    let function = function("fn main() { fn local(&value: str) {} }");
    let Stmt::Function(local) = &function.body[0] else {
        panic!("expected local function")
    };
    assert_eq!(local.params[0].mode, ParamMode::View);
    for source in [
        "fn(&value: str) {}",
        "fn(&value) {}",
        "(&value: str) => value.clone()",
        "(&value: str): str => value.clone()",
        "(&value) => value",
    ] {
        let params = expression_params(source);
        assert_eq!(params.len(), 1, "{source}");
        assert_eq!(params[0].mode, ParamMode::View, "{source}");
        assert_eq!(params[0].name, "value");
        assert_eq!(
            &source[params[0].span.start.offset..params[0].span.end.offset],
            "&value"
        );
    }
}

#[test]
fn borrow_parser_function_type_modes_survive_nested_callback_slots() {
    let function = function(
        "fn inspect(reader: fn(&ns.User, int): str, task: async fn(fn(&ns.User): str): null) {}",
    );
    let Some(TypeName::Function {
        params,
        param_modes,
        return_type,
        is_async,
    }) = &function.params[0].ty
    else {
        panic!("expected reader function type")
    };
    assert_eq!(params, &[TypeName::Custom("ns.User".into()), TypeName::Int]);
    assert_eq!(param_modes, &[ParamMode::View, ParamMode::Owned]);
    assert_eq!(return_type.as_ref(), &TypeName::String);
    assert!(!is_async);
    let Some(TypeName::Function {
        params,
        param_modes,
        is_async,
        ..
    }) = &function.params[1].ty
    else {
        panic!("expected async callback type")
    };
    assert!(*is_async);
    assert_eq!(param_modes, &[ParamMode::Owned]);
    let TypeName::Function { param_modes, .. } = &params[0] else {
        panic!("expected nested synchronous callback")
    };
    assert_eq!(param_modes, &[ParamMode::View]);

    // Arrow lookahead must scan the same function type grammar as the parser.
    let params = expression_params("(reader: fn(&ns.User): str, &value: ns.User) => reader(value)");
    assert_eq!(params[0].mode, ParamMode::Owned);
    assert_eq!(params[1].mode, ParamMode::View);
    assert!(matches!(
        &params[0].ty,
        Some(TypeName::Function { param_modes, .. }) if param_modes == &[ParamMode::View]
    ));
}

#[test]
fn borrow_parser_function_type_owned_and_view_are_distinct() {
    let function = function("fn inspect(a: fn(str): int, b: fn(&str): int) {}");
    assert_ne!(function.params[0].ty, function.params[1].ty);
    assert_eq!(ParamMode::default(), ParamMode::Owned);
}

#[test]
fn borrow_parser_function_type_and_arrow_lookahead_share_depth_limit() {
    let nested = |depth: usize| format!("{}int{}", "fn(&".repeat(depth), "): int".repeat(depth));
    let accepted = nested(32);
    let function = function(&format!("fn Inspect(reader: {accepted}) {{}}"));
    let mut current = function.params[0].ty.as_ref().unwrap();
    for _ in 0..32 {
        let TypeName::Function {
            params,
            param_modes,
            ..
        } = current
        else {
            panic!("nested callback lost its type")
        };
        assert_eq!(param_modes, &[ParamMode::View]);
        assert_eq!(params.len(), param_modes.len());
        current = &params[0];
    }
    expression_params(&format!("(&reader: {accepted}) => 1"));
    let rejected = nested(33);
    for source in [
        format!("fn Inspect(reader: {rejected}) {{}}"),
        format!("fn main() {{ reader = (&value: {rejected}) => 1 }}"),
    ] {
        let error = Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_program()
            .unwrap_err();
        assert!(
            error.message.contains("maximum type depth exceeded"),
            "{error:?}"
        );
    }
}

#[test]
fn borrow_parser_async_mode_is_preserved_for_checker_diagnostic() {
    let function = function("async fn inspect(&value: str): null {}");
    assert!(function.is_async);
    assert_eq!(function.params[0].mode, ParamMode::View);
}

#[test]
fn borrow_parser_rejects_reference_types_calls_and_public_aliases() {
    for source in [
        "fn f(value: &User) {}",
        "fn f(view value: User) {}",
        "fn f(ref value: User) {}",
        "fn f(&&value: User) {}",
        "fn f(&mut value: User) {}",
        "fn f() { value: &User = other }",
        "struct Holder { value: &User }",
        "struct Holder { &value: User }",
        "enum Holder { Value(&value: User) }",
        "fn f(values: [&User]) {}",
        "fn f(): &User {}",
        "fn f() { value = &other }",
        "fn f() { return &other }",
        "fn f() { call(&other) }",
        "fn f() { mapper = &item: User => item }",
    ] {
        let result = Parser::new(Lexer::new(source).lex().unwrap()).parse_program();
        assert!(result.is_err(), "must reject {source}");
    }
}

#[test]
fn borrow_parser_invalid_ampersand_diagnostics_point_at_operator() {
    for (source, message, code) in [
        (
            "fn f() { inspect(&data) }",
            "'&' is not written at the call site",
            "E0918",
        ),
        (
            "fn f() { print(&data) }",
            "'&' is not written at the call site",
            "E0918",
        ),
        (
            "fn f() { inspect(data, &other) }",
            "'&' is not written at the call site",
            "E0918",
        ),
        (
            "fn f() { value = &other }",
            "single '&' is only allowed before a function parameter",
            "E0919",
        ),
        (
            "fn f(value: &User) {}",
            "single '&' is only allowed before a function parameter",
            "E0919",
        ),
    ] {
        let error = Parser::new(Lexer::new(source).lex().unwrap())
            .parse_program()
            .unwrap_err();
        assert!(error.message.starts_with(message), "{error:?}");
        assert_eq!(error.span.start.offset, source.find('&').unwrap());
        assert_eq!(error.span.end.offset, error.span.start.offset + 1);
        let diagnostic = error.diagnostic_data("borrow.ku", source);
        assert_eq!(diagnostic.code, code);
        assert_eq!(diagnostic.file, "borrow.ku");
        assert_eq!(diagnostic.column, error.span.start.column);
        assert!(!diagnostic.helps.is_empty());
    }
}

#[test]
fn borrow_parser_unparenthesized_arrow_has_explicit_parentheses_guidance() {
    for source in ["&item: User => item", "&item => item"] {
        let error = Parser::new(Lexer::new(source).lex().unwrap())
            .parse_expression_only()
            .unwrap_err();
        assert!(error.message.contains("use parentheses"), "{error:?}");
        assert_eq!(error.diagnostic_data("borrow.ku", source).code, "E0919");
    }
    let error = Parser::new(Lexer::new("&item").lex().unwrap())
        .parse_expression_only()
        .unwrap_err();
    assert!(error
        .message
        .contains("does not provide an address-of expression"));
}

#[test]
fn borrow_parser_namespace_and_named_import_rewrites_preserve_parameter_modes() {
    let temp = native_harness::TempDir::new("borrow-parameter-import");
    std::fs::write(
        temp.path().join("model.ku"),
        "struct User { name: str } fn Inspect(&user: User): int { return 1 } fn Consume(user: User): int { return 1 } fn Apply(reader: fn(&User): int, &user: User): int { return reader(user) }",
    )
    .unwrap();
    for (import, user, inspect, consume, apply) in [
        ("import model from \"./model.ku\"", "model.User", "model.Inspect", "model.Consume", "model.Apply"),
        ("import { User as Person, Inspect as Read, Consume as Take, Apply as Run } from \"./model.ku\"", "Person", "Read", "Take", "Run"),
    ] {
        let entry = temp.path().join("main.ku");
        let source = format!(
            "{import}\nfn main() {{ user = {user} {{ name: \"Ku\" }} reader: fn(&{user}): int = {inspect} consumer: fn({user}): int = {consume} println({inspect}(user)) println({apply}(reader, user)) println(consumer(user)) }}"
        );
        std::fs::write(&entry, &source).unwrap();
        ku::cli::check_source(entry.to_str().unwrap(), &source)
            .unwrap_or_else(|error| panic!("import mode preservation: {error:?}"));
        let mismatch = source.replace(
            &format!("reader: fn(&{user}): int = {inspect}"),
            &format!("reader: fn(&{user}): int = {consume}"),
        );
        std::fs::write(&entry, &mismatch).unwrap();
        let error = ku::cli::check_source(entry.to_str().unwrap(), &mismatch)
            .expect_err("an import rewrite must not erase the borrowed mode");
        assert_eq!(error.diagnostic_data("main.ku", &mismatch).code, "E0914");
    }
}

#[test]
fn borrow_parser_view_remains_an_identifier_and_storage_fields_remain_owned() {
    let program = parse(
        "struct Holder { view: str } enum Choice { View(view: str) } fn view(view: str) { page.view ui.view() }",
    );
    let Item::Struct(structure) = &program.items[0] else {
        panic!("expected struct")
    };
    assert_eq!(structure.fields[0].mode, ParamMode::Owned);
    let Item::Enum(enumeration) = &program.items[1] else {
        panic!("expected enum")
    };
    assert_eq!(enumeration.variants[0].fields[0].mode, ParamMode::Owned);
    let Item::Function(function) = &program.items[2] else {
        panic!("expected function")
    };
    assert_eq!(function.name, "view");
    assert_eq!(function.params[0].mode, ParamMode::Owned);
}
