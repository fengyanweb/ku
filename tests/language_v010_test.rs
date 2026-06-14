use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::{
    backend, checker::Checker, cli::run_cli, cli::run_source, ir, lexer::Lexer, package,
    parser::Parser,
};

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-v010-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should work")
            .as_nanos()
    ))
}

fn lower_ir(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    ir::lower_program(&program).expect("lower ir").to_string()
}

fn check_err(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new()
        .check(&program)
        .expect_err("program should fail")
        .to_string()
}

#[test]
fn runtime_closure_captures_outer_bindings_without_whole_env() {
    let source = r#"
fn main() {
    base = 1
    fn calc(n:int): int {
        if (n <= 1) {
            return base
        } else {
            return n * calc(n - 1)
        }
    }
    base = 2
    value = calc(4)
    if (value != 48) {
        panic("bad closure capture")
    }
}
"#;

    run_source("inline.ku", source).expect("recursive local closure should run");
}

#[test]
fn ir_lowers_question_to_explicit_result_cfg() {
    let text = lower_ir(
        r#"
fn value(): int! {
    return ok(7)
}

fn main(): int! {
    item = value()?
    return ok(item)
}
"#,
    );

    assert!(text.contains("result_branch"), "unexpected IR:\n{text}");
    assert!(text.contains("ok_value"), "unexpected IR:\n{text}");
    assert!(text.contains("propagate_err"), "unexpected IR:\n{text}");
    assert!(!text.contains(" = value()?"), "unexpected IR:\n{text}");
}

#[test]
fn ir_lowers_fail_inside_try_to_error_handler() {
    let text = lower_ir(
        r#"
fn main(): int! {
    try {
        fail "bad"
    } catch (err) {
        return ok(1)
    } finally {
        print("cleanup")
    }
    return ok(2)
}
"#,
    );

    assert!(text.contains("jump_err"), "unexpected IR:\n{text}");
    assert!(
        text.contains("bind_error err from"),
        "unexpected IR:\n{text}"
    );
    assert!(!text.contains("fail \"bad\""), "unexpected IR:\n{text}");
}

#[test]
fn checker_requires_enum_match_to_be_exhaustive() {
    let err = check_err(
        r#"
enum Maybe {
    Some(value:int)
    None
}

fn main() {
    value = Maybe.Some(1)
    text = match value {
        Maybe.Some(v) => "some"
    }
    print(text)
}
"#,
    );
    assert!(err.contains("not exhaustive"), "unexpected error: {err}");

    let guarded = check_err(
        r#"
enum Maybe {
    Some(value:int)
    None
}

fn main() {
    value = Maybe.Some(1)
    text = match value {
        Maybe.Some(v) if (v > 0) => "some"
        Maybe.None => "none"
    }
    print(text)
}
"#,
    );
    assert!(
        guarded.contains("not exhaustive"),
        "unexpected error: {guarded}"
    );
}

#[test]
fn native_c_backend_accepts_if_while_int_subset() {
    let tokens = Lexer::new(
        r#"
fn sum(n:int): int {
    total = 0
    i = 0
    while (i < n) {
        total = total + i
        i = i + 1
    }
    if (total > 2) {
        return total
    } else {
        return 0
    }
}

fn main() {
    print(sum(4))
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("if ("));
    assert!(c.contains("goto block"));
    assert!(c.contains("block"));
    assert!(c.contains("return total;"));
}

#[test]
fn native_c_backend_lowers_result_int_question_and_propagation() {
    let tokens = Lexer::new(
        r#"
fn value(): int! {
    return ok(7)
}

fn main(): int! {
    item = value()?
    return ok(item + 1)
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("typedef struct { bool ok; int64_t value; const char* error; } KuResultInt"));
    assert!(c.contains("if (t0.ok) goto block"));
    assert!(c.contains(" = t0.value;"));
    assert!(c.contains("int64_t item = "));
    assert!(c.contains("return t"));
}

#[test]
fn native_c_backend_still_rejects_complex_result_payloads() {
    let tokens = Lexer::new(
        r#"
fn values(): [int]! {
    return ok([1, 2])
}

fn main() {
    print("ok")
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let err = backend::c::generate_c_source(&ir)
        .expect_err("array Result payload is outside native C prototype")
        .to_string();
    assert!(
        err.contains("native C prototype"),
        "unexpected error: {err}"
    );
}

#[test]
fn guarded_wildcard_does_not_make_later_match_arms_unreachable() {
    let source = r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Done
    label = match state {
        _ if (false) => "guarded"
        State.Ready => "ready"
        State.Done => "done"
    }
    print(label)
}
"#;

    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
}

#[test]
fn guarded_wildcard_alone_is_not_exhaustive_for_enum_match() {
    let err = check_err(
        r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Done
    label = match state {
        _ if (true) => "guarded"
    }
    print(label)
}
"#,
    );

    assert!(err.contains("not exhaustive"), "unexpected error: {err}");
}

#[test]
fn duplicate_unguarded_literal_match_arm_is_unreachable() {
    let err = check_err(
        r#"
fn main() {
    value = 1
    text = match value {
        1 => "one"
        1 => "again"
        _ => "other"
    }
    print(text)
}
"#,
    );

    assert!(err.contains("unreachable"), "unexpected error: {err}");
}

#[test]
fn match_guarded_variant_then_unguarded_variant_is_allowed() {
    let source = r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Ready
    label = match state {
        State.Ready if (false) => "guarded"
        State.Ready => "ready"
        State.Done => "done"
    }
    print(label)
}
"#;

    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
}

#[test]
fn match_unguarded_variant_then_guarded_variant_is_unreachable() {
    let err = check_err(
        r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Ready
    label = match state {
        State.Ready => "ready"
        State.Ready if (true) => "again"
        State.Done => "done"
    }
    print(label)
}
"#,
    );

    assert!(err.contains("unreachable"), "unexpected error: {err}");
}

#[test]
fn package_lock_records_import_dependencies_and_cache_keys() {
    let dir = unique_temp_path("package-lock-deps");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create package src");
    fs::write(
        dir.join("ku.mod"),
        r#"
name = "demo_pkg"
version = "0.1.3"
"#,
    )
    .expect("write ku.mod");
    fs::write(src.join("util.ku"), "fn Value(): int { return 1 }").expect("write util");
    let main = src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("package check");
    let lock = fs::read_to_string(dir.join("ku.lock")).expect("read lock");
    assert!(lock.contains("[[dependency]]"), "unexpected lock:\n{lock}");
    assert!(
        lock.contains("path = \"src/util.ku\""),
        "unexpected lock:\n{lock}"
    );
    assert!(
        lock.contains("cache_key = \"ku-fnv64-"),
        "unexpected lock:\n{lock}"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_dependency_is_cached_and_importable() {
    let dir = unique_temp_path("package-remote-dep");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 42 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    let checksum = package::package_source_checksum(&dep_root).expect("checksum");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
version = "0.1.4"
dep.util = "1.0.0"
dep.util.source = "file://{}"
dep.util.checksum = "{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/"),
            checksum
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("package check");
    let lock = fs::read_to_string(dir.join("app").join("ku.lock")).expect("read lock");
    assert!(
        lock.contains("[[package_dependency]]"),
        "unexpected lock:\n{lock}"
    );
    assert!(lock.contains("name = \"util\""), "unexpected lock:\n{lock}");
    assert!(lock.contains(&checksum), "unexpected lock:\n{lock}");
    assert!(
        dir.join("app")
            .join(".ku")
            .join("cache")
            .join("packages")
            .join("util")
            .join("1.0.0")
            .join("src")
            .join("util.ku")
            .exists(),
        "dependency should be cached"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_dependency_import_rejects_parent_escape() {
    let dir = unique_temp_path("package-dep-escape");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 42 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/../secret"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("dependency escape should fail")
    .to_string();
    assert!(err.contains("dependency root"), "unexpected error: {err}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_url_accepts_triple_slash_windows_path() {
    let dir = unique_temp_path("package-file-url-triple");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 7 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file:///{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("package check");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_dependency_without_checksum_refreshes_changed_cache() {
    let dir = unique_temp_path("package-dep-refresh");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    let dep_file = dep_src.join("util.ku");
    fs::write(&dep_file, "fn Value(): int { return 1 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("first package check");
    fs::write(&dep_file, "fn Value(): int { return 2 }").expect("update dep util");
    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("second package check");
    let cached = fs::read_to_string(
        dir.join("app")
            .join(".ku")
            .join("cache")
            .join("packages")
            .join("util")
            .join("1.0.0")
            .join("src")
            .join("util.ku"),
    )
    .expect("read cached util");
    assert!(cached.contains("return 2"), "unexpected cache:\n{cached}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_manifest_rejects_bad_checksum_format() {
    let dir = unique_temp_path("package-bad-checksum-format");
    let app_src = dir.join("app").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::write(app_src.join("main.ku"), "fn main() { print(\"ok\") }").expect("write main");
    fs::write(
        dir.join("app").join("ku.mod"),
        r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.checksum = "bad"
"#,
    )
    .expect("write ku.mod");

    let err = package::discover_for_file(&app_src.join("main.ku"))
        .expect_err("bad checksum should fail")
        .to_string();
    assert!(err.contains("checksum"), "unexpected error: {err}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_dependency_checksum_mismatch_is_rejected() {
    let dir = unique_temp_path("package-remote-dep-bad-checksum");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 1 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://{}"
dep.util.checksum = "ku-fnv64-00000000deadbeef"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("checksum mismatch should fail")
    .to_string();
    assert!(err.contains("checksum mismatch"), "unexpected error: {err}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_gc_removes_stale_dependency_versions_only() {
    let dir = unique_temp_path("package-gc");
    let app_src = dir.join("app").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::write(app_src.join("main.ku"), "fn main() { print(\"ok\") }").expect("write main");
    fs::write(
        dir.join("app").join("ku.mod"),
        r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://C:/tmp/util"
"#,
    )
    .expect("write ku.mod");
    let cache = dir.join("app").join(".ku").join("cache").join("packages");
    fs::create_dir_all(cache.join("util").join("1.0.0")).expect("create current cache");
    fs::create_dir_all(cache.join("util").join("0.9.0")).expect("create stale version cache");
    fs::create_dir_all(cache.join("old").join("1.0.0")).expect("create stale package cache");
    let package = package::discover_for_file(&app_src.join("main.ku"))
        .expect("discover")
        .expect("package");

    let removed = package::gc_cache(&package, 64).expect("gc cache");

    assert_eq!(removed, 2);
    assert!(cache.join("util").join("1.0.0").exists());
    assert!(!cache.join("util").join("0.9.0").exists());
    assert!(!cache.join("old").exists());

    let _ = fs::remove_dir_all(dir);
}
