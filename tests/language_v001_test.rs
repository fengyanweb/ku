use ku::cli::{check_source, run_source};

#[test]
fn check_accepts_valid_program() {
    let source = r#"
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    print(add(10, 20))
}
"#;

    check_source("inline.ku", source).expect("valid program should pass check");
}

#[test]
fn check_rejects_string_number_addition() {
    let source = r#"
fn main() {
    print(1 + "hello")
}
"#;

    let err = check_source("inline.ku", source).expect_err("type error should fail check");
    assert!(
        err.to_string().to_lowercase().contains("type error"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_rejects_assignment_to_immutable_variable() {
    let source = r#"
fn main() {
    APP_COUNT = 0
    APP_COUNT = 1
}
"#;

    let err = check_source("inline.ku", source).expect_err("constant assignment should fail");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_rejects_let_syntax() {
    let source = r#"
fn main() {
    let name = "Ku"
}
"#;

    let err = check_source("inline.ku", source).expect_err("let syntax should fail");
    assert!(
        err.to_string().contains("'let' is not supported"),
        "unexpected error: {err}"
    );
}

#[test]
fn run_stops_unbounded_recursion() {
    let source = r#"
fn spin(): int {
    return spin()
}

fn main() {
    print(spin())
}
"#;

    let err = run_source("inline.ku", source).expect_err("recursion should be bounded");
    assert!(
        err.to_string().contains("call depth"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_accepts_v002_declaration_inference_and_arrow_functions() {
    let source = r#"
fn main() {
    name = 'Ku'
    count:int
    title:str
    add = (a, b) => {
        return a + b
    }

    count = add(10, 20)
    print(`Hello {name} {count} {title}`)
}
"#;

    check_source("inline.ku", source).expect("v0.0.2 syntax should pass check");
}

#[test]
fn check_rejects_constant_reassignment_by_name_rule() {
    let source = r#"
fn main() {
    APP_NAME = "ku"
    APP_NAME = "max"
}
"#;

    let err = check_source("inline.ku", source).expect_err("constant reassignment should fail");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_rejects_missing_return_for_typed_function() {
    let source = r#"
fn broken(): int {
    value = 10
}

fn main() {
    print(broken())
}
"#;

    let err = check_source("inline.ku", source).expect_err("missing return should fail");
    assert!(
        err.to_string().contains("must return"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_rejects_function_value_return_type_mismatch() {
    let source = r##"
fn main() {
    add = (a, b) => {
        return a + b
    }

    result:int = add("x", "y")
    print(result)
}
"##;

    let err = check_source("inline.ku", source).expect_err("function value type should fail");
    assert!(
        err.to_string().contains("type error"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_accepts_template_string_number_concat() {
    let source = r##"
fn main() {
    print(`value {1 + "x"} {"x" + 2.5}`)
}
"##;

    check_source("inline.ku", source).expect("template concat should pass check");
    run_source("inline.ku", source).expect("template concat should run");
}

#[test]
fn check_rejects_template_string_invalid_cross_type_operator() {
    let source = r##"
fn main() {
    print(`value {1 - "x"}`)
}
"##;

    let err = check_source("inline.ku", source).expect_err("template invalid op should fail");
    assert!(
        err.to_string().contains("type error"),
        "unexpected error: {err}"
    );
}

#[test]
fn run_stops_unbounded_while_loop() {
    let source = r#"
fn main() {
    while (true) {
    }
}
"#;

    let err = run_source("inline.ku", source).expect_err("while loop should be bounded");
    assert!(
        err.to_string().contains("execution step limit"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_accepts_null_type_and_literal() {
    let source = r#"
fn main() {
    empty:null = null
    print(empty)
}
"#;

    check_source("inline.ku", source).expect("null should pass check");
}

#[test]
fn check_rejects_string_type_alias() {
    let source = r#"
fn main() {
    name:string = "Ku"
    print(name)
}
"#;

    let err = check_source("inline.ku", source).expect_err("string type alias should fail");
    assert!(
        err.to_string().contains("expected type name"),
        "unexpected error: {err}"
    );
}
