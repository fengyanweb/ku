use ku::cli::{check_source, run_source};

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
fn std_string_array_json_and_time_check_and_run() {
    let source = r#"
fn main() {
    text = string.trim("  Ku Lang  ")
    print(string.upper(text))
    print(string.contains(text, "Lang"))
    print(string.starts_with(text, "Ku"))
    print(string.ends_with(text, "Lang"))
    print(string.replace(text, "Lang", "0.0.6"))

    nums:[int] = [1, 2]
    nums = array.push(nums, 3)
    nums = array.concat(nums, [4])
    print(array.len(nums))
    print(array.first(nums))
    print(array.last(nums))
    print(array.is_empty(nums))

    data = { name: "Ku", version: 6, ok: true }
    json_text = json.stringify(data)
    parsed = json.parse(json_text)
    print(json.stringify(parsed))

    now:int = time.now()
    millis:int = time.millis()
    print(now <= millis)
}
"#;

    check_source("inline.ku", source).expect("stdlib program should check");
    run_source("inline.ku", source).expect("stdlib program should run");
}

#[test]
fn std_json_try_parse_integrates_with_question_and_try_catch() {
    let source = r#"
fn parse_value(): str! {
    value = json.try_parse("{bad}")?
    return ok(json.stringify(value))
}

fn main() {
    message = "none"
    try {
        message = parse_value()?
    } catch (err) {
        message = "json failed"
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("json.try_parse program should check");
    run_source("inline.ku", source).expect("json.try_parse program should run");
}

#[test]
fn stdlib_type_errors_are_checked_before_run() {
    for source in [
        r#"fn main() { print(string.len(123)) }"#,
        r#"fn main() { print(string.contains("Ku", 1)) }"#,
        r#"fn main() { xs:[int] = [1]; xs = array.push(xs, "bad") }"#,
        r#"fn main() { xs:[int] = [1]; xs = array.concat(xs, ["bad"]) }"#,
        r#"fn main() { print(json.parse(123)) }"#,
        r#"fn main() { print(time.now(1)) }"#,
        r#"fn main() { print(time.unix(1)) }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("error:"), "unexpected error: {err}");
    }
}

#[test]
fn stdlib_modules_can_be_shadowed_by_local_values() {
    let source = r#"
fn main() {
    string = { len: "local string" }
    array = { len: "local array" }
    json = { parse: "local json" }
    time = { now: "local time" }

    print(string.len)
    print(array.len)
    print(json.parse)
    print(time.now)
}
"#;

    check_source("inline.ku", source).expect("shadowing should check");
    run_source("inline.ku", source).expect("shadowing should run");
}

#[test]
fn json_runtime_errors_are_clear_and_bounded() {
    let parse_error = run_err(r#"fn main() { print(json.parse("{bad}")) }"#);
    assert!(
        parse_error.contains("expected") || parse_error.contains("json"),
        "unexpected error: {parse_error}"
    );

    let stringify_error = run_err(
        r#"
fn main() {
    value = ok(1)
    print(json.stringify(value))
}
"#,
    );
    assert!(
        stringify_error.contains("json.stringify does not support result"),
        "unexpected error: {stringify_error}"
    );

    for bad_json in ["1.", "1e", "01"] {
        let source = format!(r#"fn main() {{ print(json.parse("{bad_json}")) }}"#);
        let err = run_err(&source);
        assert!(
            err.contains("digit") || err.contains("leading zero"),
            "unexpected error for {bad_json}: {err}"
        );
    }
}
