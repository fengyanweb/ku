use ku::cli::{check_source, run_source};

fn accepts(source: &str) {
    check_source("generic.ku", source).unwrap_or_else(|error| panic!("{error}\n{source}"));
}

fn rejects_move(source: &str) {
    let error = check_source("generic.ku", source).expect_err(source);
    assert_eq!(error.diagnostic_data("generic.ku", source).code, "E0901");
    assert!(error.message.contains("moved"), "{error}");
    assert!(run_source("generic.ku", source).is_err());
}

#[test]
fn generic_ownership_rechecks_concrete_owned_body_without_rejecting_copy() {
    let prefix = "fn Twice<T>(value: T): T { copy = value return value }";
    rejects_move(&format!(
        "{prefix} fn main() {{ println(Twice(\"owned\")) }}"
    ));
    rejects_move(&format!("{prefix} fn main() {{ println(Twice([1, 2])) }}"));
    let source =
        format!("{prefix} fn main() {{ if (Twice(7) != 7) {{ panic(\"Copy generic\") }} }}");
    accepts(&source);
    run_source("generic.ku", &source).unwrap();
}

#[test]
fn generic_ownership_transitive_instantiation_rechecks_moves() {
    rejects_move("fn Twice<T>(value: T): T { copy = value return value } fn Forward<T>(value: T): T { return Twice(value) } fn main() { Forward(\"owned\") }");
}

#[test]
fn generic_ownership_borrow_clone_and_owning_call_keep_original_modes() {
    accepts("fn Copy<T>(&value: T): T { return value.clone() } fn Identity<T>(value: T): T { return value } fn main() { text = \"Ku\" first = Copy(text) second = Copy(text) println(text) println(Identity(first)) println(second) }");
    rejects_move("fn Identity<T>(value: T): T { return value } fn main() { text = \"Ku\" moved = Identity(text) println(text) }");
    let error = check_source(
        "generic.ku",
        "fn Escape<T>(&value: T): T { return value } fn main() {}",
    )
    .unwrap_err();
    assert_eq!(error.diagnostic_data("generic.ku", "").code, "E0911");
}

#[test]
fn generic_ownership_concrete_callback_modes_are_checked() {
    accepts("fn Apply<T>(&op: fn(&T): T, &value: T): T { return op(value) } fn Copy(&text: str): str { return text.clone() } fn main() { text = \"Ku\" result = Apply(Copy, text) println(text) println(result) }");
    let source = "fn Apply<T>(&op: fn(&T): T, &value: T): T { return op(value) } fn Read(&value: int): int { return value } fn main() { Apply(Read, \"wrong\") }";
    assert!(check_source("generic.ku", source).is_err());
}

#[test]
fn generic_ownership_function_values_remain_an_explicit_boundary() {
    let error = check_source(
        "generic.ku",
        "fn Identity<T>(value: T): T { return value } fn main() { op = Identity }",
    )
    .unwrap_err();
    assert!(error.message.contains("generic function"), "{error}");
    assert!(error.message.contains("function value"), "{error}");
}
