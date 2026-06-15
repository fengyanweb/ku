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
fn run_accepts_bool_conditions_expr_arrows_and_println() {
    let source = r#"
fn main() {
    double = (x) => x * 2
    if (double(3) != 6) {
        panic("bad arrow")
    }
    marker = 0
    if (true) {
        marker = 1
    }
    if (marker != 1) {
        panic("bool condition should run")
    }
    value = println("Hello Ku")
    if (value != null) {
        panic("println should return null")
    }
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");

    for source in [
        r#"fn main() { if (0) { print("bad") } }"#,
        r#"fn main() { while ("Ku") { break } }"#,
    ] {
        let err = check_source("inline.ku", source).expect_err("condition must be bool");
        assert!(
            err.to_string().contains("condition must be bool"),
            "unexpected error: {err}"
        );
    }
}

#[test]
fn run_supports_statement_increment_and_decrement() {
    let source = r#"
fn main() {
    i = 1
    i++
    i--
    nums:[int] = [1]
    nums[0]++
    if (i != 1) {
        panic("bad variable increment")
    }
    if (nums[0] != 2) {
        panic("bad index increment")
    }
}
"#;

    check_source("inline.ku", source).expect("increment program should check");
    run_source("inline.ku", source).expect("increment program should run");

    let err = check_source("inline.ku", r#"fn main() { name = "Ku" name++ }"#)
        .expect_err("string increment should fail");
    assert!(
        err.to_string().contains("expected numbers"),
        "unexpected error: {err}"
    );
}

#[test]
fn run_allows_omitted_function_parameter_types() {
    let source = r#"
fn join(name, age) {
    print(`我是{name},我{age}岁了`)
}

fn add(a, b) {
    return a + b
}

fn main() {
    join("Ku", 3)
    if (add(1, 2) != 3) {
        panic("bad inferred params")
    }
}
"#;

    check_source("inline.ku", source).expect("omitted function parameter types should check");
    run_source("inline.ku", source).expect("omitted function parameter types should run");
}

#[test]
fn run_supports_break_and_continue_in_loops() {
    let source = r#"
fn main() {
    i = 0
    total = 0
    while (true) {
        i++
        if (i == 2) {
            continue
        }
        if (i > 4) {
            break
        }
        total = total + i
    }
    nums:[int] = [1, 2, 3, 4]
    for n in nums {
        if (n == 2) {
            continue
        }
        if (n == 4) {
            break
        }
        total = total + n
    }
    if (total != 12) {
        panic("bad loop control")
    }
}
"#;

    check_source("inline.ku", source).expect("loop control should check");
    run_source("inline.ku", source).expect("loop control should run");

    let err = check_source("inline.ku", "fn main() { break }")
        .expect_err("break outside loop should fail");
    assert!(
        err.to_string().contains("break outside loop"),
        "unexpected error: {err}"
    );

    let err = check_source("inline.ku", "fn main() { continue }")
        .expect_err("continue outside loop should fail");
    assert!(
        err.to_string().contains("continue outside loop"),
        "unexpected error: {err}"
    );
}

#[test]
fn run_supports_optional_field_access() {
    let source = r#"
struct User {
    name: str
}

fn main() {
    none = null
    if (none?.name != null) {
        panic("null optional field should be null")
    }
    object = { name: "Ku" }
    if (object?.name != "Ku") {
        panic("object optional field failed")
    }
    if (object?.missing != null) {
        panic("missing optional field should be null")
    }
    user = User { name: "Ku" }
    if (user?.name != "Ku") {
        panic("struct optional field failed")
    }
}
"#;

    check_source("inline.ku", source).expect("optional field should check");
    run_source("inline.ku", source).expect("optional field should run");

    let err = run_source("inline.ku", "fn main() { none = null print(none.name) }")
        .expect_err("plain null field should fail");
    assert!(
        err.to_string().contains("has no fields"),
        "unexpected error: {err}"
    );
}

#[test]
fn run_supports_destructuring_assignment() {
    let source = r#"
fn pair() {
    return 2
}

fn main() {
    a, b = 1, pair()
    a, _ = 3, 4
    if (a != 3) {
        panic("bad first destructured value")
    }
    if (b != 2) {
        panic("bad second destructured value")
    }
}
"#;

    check_source("inline.ku", source).expect("destructuring should check");
    run_source("inline.ku", source).expect("destructuring should run");

    let err = check_source("inline.ku", "fn main() { a, b = 1 }")
        .expect_err("destructuring arity should fail");
    assert!(
        err.to_string()
            .contains("destructuring assignment expects 2 values but got 1"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_supports_union_parameter_types() {
    let source = r#"
fn show(value: str | int): str {
    return str(value)
}

fn main() {
    if (show("Ku") != "Ku") {
        panic("bad string union")
    }
    if (show(7) != "7") {
        panic("bad int union")
    }
}
"#;

    check_source("inline.ku", source).expect("union parameter types should check");
    run_source("inline.ku", source).expect("union parameter types should run");

    let err = check_source(
        "inline.ku",
        r#"
fn show(value: str | int): str {
    return str(value)
}

fn main() {
    show(true)
}
"#,
    )
    .expect_err("bool should not match str | int");
    assert!(
        err.to_string()
            .contains("type error: expected str | int but got bool"),
        "unexpected error: {err}"
    );
}

#[test]
fn check_and_run_support_generic_functions() {
    let source = r#"
fn id<T>(value:T): T {
    return value
}

fn pair_first<T>(left:T, right:T): T {
    return left
}

fn main() {
    name:str = id("Ku")
    age:int = id(12)
    value:int = pair_first(1, 2)
    if (name != "Ku" || age != 12 || value != 1) {
        panic("bad generic function")
    }
}
"#;

    check_source("inline.ku", source).expect("generic functions should check");
    run_source("inline.ku", source).expect("generic functions should run");

    let err = check_source(
        "inline.ku",
        r#"
fn same<T>(left:T, right:T): T { return left }
fn main() { value = same(1, "bad") }
"#,
    )
    .expect_err("generic mismatch should fail");
    assert!(
        err.to_string().contains("type error:"),
        "unexpected generic error: {err}"
    );
}

#[test]
fn run_supports_array_map_chain() {
    let source = r#"
fn main() {
    nums:[int] = [1, 2, 3]
    doubled = nums.map(x => x * 2)
    if (doubled[0] != 2 || doubled[1] != 4 || doubled[2] != 6) {
        panic("bad array map")
    }
}
"#;

    check_source("inline.ku", source).expect("array map should check");
    run_source("inline.ku", source).expect("array map should run");

    let err = check_source("inline.ku", "fn main() { nums:[int] = [1] nums.map(1) }")
        .expect_err("array map should require function");
    assert!(
        err.to_string()
            .contains("array.map expects function but got int"),
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
