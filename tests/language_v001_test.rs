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
    double = (x: int) => x * 2
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
    ++i
    --i
    i++
    i--
    i += i + 1
    i *= 2
    i /= 3
    i %= 2
    nums:[int] = [1]
    nums[0]++
    nums[0] += 3
    user = { age: 1 }
    user.age += 2
    nested = { inner: { age: 1 } }
    nested.inner.age += 4
    rows:[[int]] = [[1]]
    rows[0][0] += 6
    if (i != 0) {
        panic("bad variable increment")
    }
    if (nums[0] != 5) {
        panic("bad index increment")
    }
    if (user.age != 3) {
        panic("bad field compound assignment")
    }
    if (nested.inner.age != 5) {
        panic("bad nested field compound assignment")
    }
    if (rows[0][0] != 7) {
        panic("bad nested index compound assignment")
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
fn run_supports_int_for_iterator_and_single_statement_control_bodies() {
    let source = r#"
fn main() {
    total = 0
    for i in 4 total += i
    if (total != 6) panic("bad int iterator")

    i = 0
    while (true)
        if (i >= 3) break
        else i++
    if (i != 3) panic("bad single statement while/if")
}
"#;

    check_source("inline.ku", source).expect("int iterator program should check");
    run_source("inline.ku", source).expect("int iterator program should run");
    run_source(
        "inline.ku",
        r#"
fn main() {
    for i in 9223372036854775807 break
}
"#,
    )
    .expect("huge int iterator should not preallocate before break");

    let err = run_source("inline.ku", "fn main() { for i in -1 print(i) }")
        .expect_err("negative int iterator should fail");
    assert!(
        err.to_string().contains("non-negative"),
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
fn run_supports_object_destructuring_assignment() {
    let source = r#"
import { http } from "std"

fn main() {
    user = { code: 7, name: "Ku", city: "Hangzhou" }
    { code, name: userName, missing = 42, ...rest } = user
    { code: _ } = { code: 9 }
    { code: httpCode } = http

    if (code != 7) {
        panic("bad shorthand field")
    }
    if (userName != "Ku") {
        panic("bad renamed field")
    }
    if (missing != 42) {
        panic("bad default field")
    }
    if (rest.city != "Hangzhou") {
        panic("bad rest object")
    }
    if (httpCode.SUCCESS != 200) {
        panic("bad std module object destructuring")
    }
}
"#;

    check_source("inline.ku", source).expect("object destructuring should check");
    run_source("inline.ku", source).expect("object destructuring should run");

    let err = check_source("inline.ku", "fn main() { { missing } = { name: \"Ku\" } }")
        .expect_err("missing static field should fail");
    assert!(
        err.to_string().contains("object has no field 'missing'"),
        "unexpected error: {err}"
    );

    let err = check_source(
        "inline.ku",
        "import { http } from \"std\" fn main() { { service } = http }",
    )
    .expect_err("http service must not be exposed as an object field");
    assert!(
        err.to_string().contains("object has no field 'service'"),
        "unexpected error: {err}"
    );

    // json.parse yields a KuValue (a first-class tagged dynamic value), not a
    // static object, so destructuring it is a compile-time error — dynamic reads
    // must go through obj["key"]? instead.
    let err = check_source(
        "inline.ku",
        "fn main() { obj = json.parse(\"{}\") { missing } = obj }",
    )
    .expect_err("a KuValue cannot be destructured");
    assert!(
        err.to_string()
            .contains("object destructuring expects object but got KuValue"),
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
    add = (a: int, b: int) => {
        return a + b
    }

    count = add(10, 20)
    print(`Hello {name} {count} {title}`)
}
"#;

    check_source("inline.ku", source).expect("v0.0.2 syntax should pass check");
}

#[test]
fn typed_arrow_functions_are_first_class_function_values() {
    let source = r#"
fn main() {
    add = (a: int, b: int): int => a + b
    double = value: int => value * 2
    selected = double
    print(add(2, 3))
    print(selected(4))
}
"#;

    check_source("inline.ku", source).expect("typed arrow functions should check");
    run_source("inline.ku", source).expect("typed arrow functions should remain first class");
}

#[test]
fn function_type_annotations_accept_sync_and_async_function_values() {
    let source = r#"
fn Add(a: int, b: int): int {
    return a + b
}

async fn Load(id: int): str! {
    return ok("user")
}

fn main() {
    op: fn(int, int): int = Add
    loader: async fn(int): str! = Load
    print(op(2, 3))
}
"#;

    check_source("inline.ku", source).expect("function type annotations should check");
    run_source("inline.ku", source).expect("sync function type value should run");
}

#[test]
fn function_type_annotation_rejects_mismatched_signatures() {
    let source = r#"
fn Add(a: int, b: int): int {
    return a + b
}

fn main() {
    op: fn(int): int = Add
    print(op(2))
}
"#;

    let err = check_source("inline.ku", source)
        .expect_err("mismatched function type annotation should fail");
    assert!(
        err.to_string()
            .contains("expected fn(int): int but got fn(int, int): int"),
        "unexpected error: {err}"
    );
}

#[test]
fn function_type_annotation_requires_precise_function_value_signature() {
    let source = r#"
fn main() {
    f: fn(int): int = (x) => "bad"
    print(f(1))
}
"#;

    let err = check_source("inline.ku", source)
        .expect_err("untyped function value should not satisfy a precise function type");
    assert!(
        err.to_string()
            .contains("expected fn(int): int but got fn(int): str"),
        "unexpected error: {err}"
    );
}

#[test]
fn arrow_function_type_syntax_is_not_a_type_annotation() {
    let source = r#"
fn Add(a: int, b: int): int {
    return a + b
}

fn main() {
    op: (int, int) => int = Add
    print(op(2, 3))
}
"#;

    let err = check_source("inline.ku", source).expect_err("arrow syntax is not a function type");
    assert!(
        err.to_string().contains("expected type name"),
        "unexpected error: {err}"
    );
}

#[test]
fn typed_arrow_function_return_type_is_checked() {
    let source = r#"
fn main() {
    broken = (value: int): str => value
    print(broken(1))
}
"#;

    let err =
        check_source("inline.ku", source).expect_err("typed arrow return mismatch should fail");
    assert!(
        err.to_string().contains("type error"),
        "unexpected error: {err}"
    );
}

#[test]
fn object_index_is_strict_unless_question_is_explicit() {
    let strict = r#"
fn main() {
    user = { name: "Ku" }
    print(user["missing"])
}
"#;
    let err = run_source("inline.ku", strict).expect_err("missing object key should fail");
    assert!(
        err.to_string().contains("object has no key 'missing'"),
        "unexpected error: {err}"
    );

    let present = r#"
fn main(): null! {
    user = { name: "Ku" }
    print(user["name"]?)
    return ok(null)
}
"#;
    check_source("inline.ku", present).expect("present key with ? should check");
    run_source("inline.ku", present).expect("present key with ? should unwrap");

    // `obj[key]?` is strict: a missing key propagates a recoverable Err
    // (domain "object", code "missing_key"), it does not return null.
    let missing = r#"
fn main(): null! {
    user = { name: "Ku" }
    print(user["missing"]?)
    return ok(null)
}
"#;
    let missing_err = run_source("inline.ku", missing)
        .expect_err("missing key with ? should propagate a recoverable error");
    assert!(
        missing_err.to_string().contains("missing_key"),
        "expected missing_key error: {missing_err}"
    );

    // Lenient reads use get_or, which returns the default for a missing key.
    let lenient = r#"
fn main() {
    user = { name: "Ku" }
    print(str(user.get_or("missing", null)))
}
"#;
    run_source("inline.ku", lenient).expect("get_or returns the default for a missing key");
}

#[test]
fn object_get_or_supports_function_and_method_forms() {
    let source = r#"
fn main() {
    user = { name: "Ku" }
    print(object.get_or(user, "name", "fallback"))
    print(object.get_or(user, "missing", "fallback"))
    print(user.get_or("name", "fallback"))
    print(user.get_or("missing", "fallback"))
}
"#;

    check_source("inline.ku", source).expect("object.get_or forms should check");
    run_source("inline.ku", source).expect("object.get_or forms should run");
}

#[test]
fn object_get_or_default_argument_is_evaluated_immediately() {
    let source = r#"
fn Boom(): str {
    panic("default evaluated")
    return "fallback"
}

fn main() {
    user = { name: "Ku" }
    print(user.get_or("name", Boom()))
}
"#;

    let err = run_source("inline.ku", source)
        .expect_err("object.get_or default argument should be evaluated immediately");
    assert!(
        err.to_string().contains("default evaluated"),
        "unexpected error: {err}"
    );
}

#[test]
fn object_get_or_returns_kuvalue_not_static_field_type() {
    let source = r#"
fn main() {
    obj = { name: "Ku" }
    value: int = obj.get_or("name", 0)
    print(value)
}
"#;

    // get_or yields a first-class KuValue (dynamic), not the static field type,
    // so binding it to `int` is a type error mentioning KuValue.
    let err = check_source("inline.ku", source)
        .expect_err("get_or returns a KuValue, not the static field type");
    assert!(
        err.to_string().contains("expected int but got KuValue"),
        "unexpected error: {err}"
    );
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
    add = (a: int, b: int) => {
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
fn check_accepts_unbounded_loop_without_fixed_step_limit() {
    let source = r#"
fn main() {
    while (true) {
    }
}
"#;

    check_source("inline.ku", source).expect("unbounded loop syntax should check");
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
