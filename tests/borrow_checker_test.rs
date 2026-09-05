use ku::cli::check_source;

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

fn accepts(source: &str) {
    check_source("borrow.ku", source).unwrap_or_else(|error| {
        panic!(
            "{}\nsource:\n{source}",
            error.diagnostic("borrow.ku", source)
        )
    });
}

fn rejects(source: &str, code: &str, message: &str) {
    let error = check_source("borrow.ku", source).expect_err(source);
    let diagnostic = error.diagnostic_data("borrow.ku", source);
    assert_eq!(diagnostic.code, code, "{error}\n{source}");
    assert!(error.message.contains(message), "{error}\n{source}");
    assert!(!diagnostic.helps.is_empty(), "actionable help is required");
}

#[test]
fn borrow_checker_repeated_reborrow_projection_clone_copy_and_temporary() {
    accepts(
        r#"
struct User { name: str, age: int, items: [str] }
fn Read(&name: str): int { return name.len() }
fn CopyName(&user: User): str { return user.name.clone() }
fn Inspect(&user: User): int {
    println(user.items.len())
    println(user.items[0])
    Read(user.name)
    return user.age
}
fn Both(&left: User, &right: User): bool { return left.name == right.name }
fn main() {
    user = User { name: "Ku", age: 1, items: ["x"] }
    Inspect(user)
    Inspect(user)
    println(Both(user, user))
    copy = CopyName(user)
    println(copy)
    println(user.name)
    Inspect(User { name: "temporary", age: 2, items: [] })
}
"#,
    );
}

#[test]
fn borrow_checker_owning_arguments_still_move_including_function_values() {
    for source in [
        "fn Eat(x: str) {} fn main() { x = \"Ku\" Eat(x) println(x) }",
        "fn Add(): int { return 1 } fn Apply(op: fn(): int): int { return op() } fn main() { op = Add Apply(op) op() }",
    ] {
        rejects(source, "E0901", "use of moved value");
    }
    accepts("fn Add(): int { return 1 } fn Apply(&op: fn(): int): int { return op() + op() } fn main() { op = Add Apply(op) op() }");
}

#[test]
fn borrow_checker_readonly_assignments_cover_roots_and_nested_places() {
    for statement in [
        "user = User { name: \"new\", age: 2, items: [1] }",
        "user.name = \"new\"",
        "user.items[0] = 2",
        "user.age += 1",
        "user.age++",
        "user.age--",
    ] {
        rejects(&format!("struct User {{ name: str, age: int, items: [int] }} fn Change(&user: User) {{ {statement} }} fn main() {{}}"), "E0910", "cannot modify through borrowed parameter");
    }
}

#[test]
fn borrow_checker_owned_values_cannot_return_or_enter_aggregates() {
    for statement in [
        "return user.name",
        "copy = user.name",
        "copy = [user.name]",
        "copy = { name: user.name }",
        "copy = Holder { name: user.name }",
        "copy = Message.Text(user.name)",
        "copy = ok(user.name)",
    ] {
        let code = if statement.contains("ok(") {
            "E0915"
        } else {
            "E0911"
        };
        rejects(&format!("struct User {{ name: str }} struct Holder {{ name: str }} enum Message {{ Text(value: str) }} fn Inspect(&user: User) {{ {statement} }} fn main() {{}}"), code, "borrowed value");
    }
}

#[test]
fn borrow_checker_rejects_owning_argument_and_nested_owned_projection() {
    rejects("struct Profile { name: str } struct User { profile: Profile } fn Eat(x: str) {} fn Inspect(&user: User) { Eat(user.profile.name) } fn main() {}", "E0915", "cannot pass borrowed value");
    rejects(
        "fn Inspect(&items: [str]): str { return items[0] } fn main() {}",
        "E0911",
        "cannot move out of borrowed value",
    );
    accepts("fn Eat(x: str) {} fn Inspect(&x: str) { Eat(x.clone()) } fn main() {}");
}

#[test]
fn borrow_checker_rejects_captures_in_all_closure_spellings_even_copy() {
    for statement in [
        "f = () => value",
        "f = fn(): int { return value }",
        "fn Local(): int { return value }",
    ] {
        rejects(
            &format!("fn Inspect(&value: int) {{ {statement} }} fn main() {{}}"),
            "E0912",
            "cannot capture borrowed parameter",
        );
    }
    accepts("fn Inspect(&value: str) { copy = value.clone() f = () => copy.clone() println(f()) } fn main() {}");
}

#[test]
fn borrow_checker_async_boundary_and_sync_callback_inside_async() {
    rejects(
        "async fn Load(&value: str): null! { return ok(null) } fn main() {}",
        "E0913",
        "async functions cannot declare borrowed parameters",
    );
    rejects(
        "fn main() { async fn Load(&value: str): null! { return ok(null) } }",
        "E0913",
        "async functions cannot declare borrowed parameters",
    );
    accepts("fn Inspect(&value: str) { println(value) } async fn Load(value: str): null! { Inspect(value) Inspect(value) return ok(null) } fn main() {}");
    accepts("async fn Load(callback: fn(&str): int): null! { text = \"Ku\" println(callback(text)) println(text) return ok(null) } fn main() {}");
}

#[test]
fn borrow_checker_callable_parameter_modes_match_exactly() {
    for source in [
        "fn Read(&value: str): int { return value.len() } fn main() { op: fn(str): int = Read }",
        "fn Eat(value: str): int { return value.len() } fn main() { op: fn(&str): int = Eat }",
        "fn main() { op: fn(&str): int = (value: str): int => value.len() }",
        "fn Apply(op: fn(&str): int) {} fn Eat(value: str): int { return value.len() } fn main() { Apply(Eat) }",
    ] {
        rejects(source, "E0914", "callable parameter mode mismatch");
    }
    accepts("fn Read(&value: str): int { return value.len() } fn main() { op: fn(&str): int = Read value = \"Ku\" println(op(value)) println(op(value)) println(value) }");
}

#[test]
fn borrow_checker_same_call_borrow_move_conflicts_are_order_independent() {
    for source in [
        "fn Mixed(&left: str, right: str) {} fn main() { x = \"Ku\" Mixed(x, x) }",
        "fn Mixed(left: str, &right: str) {} fn main() { x = \"Ku\" Mixed(x, x) }",
        "struct User { name: str } fn Mixed(&left: str, right: User) {} fn main() { x = User { name: \"Ku\" } Mixed(x.name, x) }",
        "struct User { name: str } fn Mixed(&left: User, right: str) {} fn main() { x = User { name: \"Ku\" } Mixed(x, x.name) }",
        "fn Eat(value: str): int { return value.len() } fn Mixed(&left: str, right: int) {} fn main() { x = \"Ku\" Mixed(x, Eat(x)) }",
    ] {
        rejects(source, "E0916", "borrow conflicts with move or mutation in the same call");
    }
    accepts("fn Mixed(&left: str, right: str) {} fn main() { x = \"Ku\" Mixed(x, x.clone()) println(x) }");
}

#[test]
fn borrow_checker_same_call_capture_mutation_and_copy_field() {
    rejects("fn main() { text = \"old\" change = () => { text = \"new\" return \"old\" } println(text.contains(change())) }", "E0916", "borrow conflicts with move or mutation");
    rejects("fn Both(&value: str, n: int) {} fn main() { text = \"Ku\" change = () => { text = \"new\" return 1 } Both(text, change()) }", "E0916", "borrow conflicts with move or mutation");
    accepts("struct User { name: str, age: int } fn Both(&value: User, age: int) {} fn main() { user = User { name: \"Ku\", age: 1 } Both(user, user.age) }");
}

#[test]
fn borrow_checker_completed_read_temporaries_do_not_extend_argument_loans() {
    accepts("fn Read(&owner: str, values: [str]) {} fn main() { owner = \"owner\" payload = \"payload\" Read(owner, [payload.clone()]) println(owner) }");
    accepts("struct Error { domain: str, code: str, message: str } fn Escape(value: str): str { return value } fn main() { err = Error { domain: \"app\", code: \"bad\", message: \"message\" } println(\"ERR|\" + err.domain + \"|\" + err.code + \"|\" + Escape(err.message)) }");
    accepts("fn Read(&value: str): int { return value.len() } fn Consume(n: int, value: str) {} fn main() { value = \"Ku\" Consume(Read(value), value) }");
    accepts("fn Mixed(&left: str, right: str) {} fn main() { value = \"Ku\" Mixed(value.clone(), value) }");
}

#[test]
fn borrow_checker_same_call_callbacks_cannot_invalidate_borrowed_storage() {
    for source in [
        "fn main() { text = \"Ku\" change = (&value: str): int => { text = \"new\" return value.len() } change(text) }",
        "fn Both(&value: str, op: fn(): int) {} fn main() { text = \"Ku\" change = () => { text = \"new\" return 1 } Both(text, change) }",
        "fn Both(&value: str, op: fn(): int) {} fn main() { text = \"Ku\" Both(text, () => { text = \"new\" return 1 }) }",
        "fn Run(op: fn(): int): int { return op() } fn Both(&value: str, n: int) {} fn main() { text = \"Ku\" change = () => { text = \"new\" return 1 } Both(text, Run(change)) }",
        "fn Both(&value: str, n: int) {} fn Caller(text: str, callback: fn(): int) { Both(text, callback()) } fn main() {}",
    ] {
        rejects(source, "E0916", "borrow conflicts with move or mutation");
    }
    accepts("fn Read(&text: str, n: int) {} fn Number(): int { return 1 } fn main() { text = \"Ku\" Read(text, Number()) println(text) }");
    accepts("fn Apply(&op: fn(): int): int { return op() } fn Forward(&op: fn(): int): int { return Apply(op) } fn main() {}");
    accepts("fn Apply(&op: fn(&str): str, &text: str): str { return op(text) } fn Read(&text: str): str { return text.clone() } fn main() { text = \"Ku\" println(Apply(Read, text)) println(text) }");
    rejects("fn Apply(&op: fn(&str): str, &text: str): str { return op(text) } fn main() { text = \"Ku\" change = (&value: str): str => { text = \"new\" return value.clone() } Apply(change, text) }", "E0916", "borrow conflicts with move or mutation");
}

#[test]
fn borrow_checker_unmigrated_stdlib_borrow_boundaries_are_explicit() {
    for statement in [
        "copy = values.push(1)",
        "copy = values.first()",
        "copy = values.last()",
        "copy = values.map((x: int): int => x + 1)",
    ] {
        rejects(
            &format!("fn Read(&values: [int]) {{ {statement} }} fn main() {{}}"),
            "E0917",
            "borrowed operation is not supported",
        );
    }
    rejects(
        "fn Read(&value: str) { values = [\"Ku\"] next = values.push(value) } fn main() {}",
        "E0917",
        "borrowed operation is not supported",
    );
}

#[test]
fn borrow_checker_factory_and_container_callbacks_keep_owner_aliases() {
    let prefix = "fn Apply(&op: fn(&str): str, &text: str): str { return op(text) } fn Forward(op: fn(&str): str): fn(&str): str { return op } struct Holder { op: fn(&str): str }";
    for body in [
        "text = \"Ku\" change = (&value: str): str => { text = \"new\" return value.clone() } Apply(Forward(change), text)",
        "text = \"Ku\" change = (&value: str): str => { text = \"new\" return value.clone() } holder = Holder { op: change } Apply(holder.op, text)",
        "text = \"Ku\" change = (&value: str): str => { text = \"new\" return value.clone() } values = [change] Apply(values[0], text)",
    ] {
        rejects(&format!("{prefix} fn main() {{ {body} }}"), "E0916", "borrow conflicts with move or mutation");
    }
}

#[test]
fn borrow_checker_read_operations_and_required_stdlib_preserve_values() {
    accepts(
        r#"
import { json } from "std"
fn Greet(&name: str): str { return "Hello " + name }
fn Render(&name: str): str { return `name: {name}` }
fn Read(&name: str, &values: [int]): str! {
    println(name)
    println(str(name))
    println(len(name))
    println(name.len())
    println(name.byte_len())
    println(name.contains(name))
    println(name.starts_with(name))
    println(name.ends_with(name))
    println(values.len())
    println(values.is_empty())
    return json.stringify(values)
}
fn main(): null! {
    value = { name: "Ku" }
    text = json.stringify(value)?
    println(value)
    println(text)
    return ok(null)
}
"#,
    );
    rejects(
        "fn main() { value = \"Ku\" result = ok(value) println(value) }",
        "E0901",
        "use of moved value",
    );
}

#[test]
fn borrow_checker_consuming_iteration_match_destructure_result_boundaries() {
    rejects(
        "fn Read(&values: [str]) { for value in values { println(value) } } fn main() {}",
        "E0917",
        "for iteration",
    );
    rejects("enum Message { Text(value: str) } fn Read(&message: Message): str { return match message { Message.Text(value) => value.clone() } } fn main() {}", "E0917", "match with owned payload");
    accepts("enum Code { Number(value: int) } fn Read(&code: Code): int { return match code { Code.Number(value) => value } } fn main() {}");
    rejects(
        "fn Read(&value: str!): null! { text = value? return ok(null) } fn main() {}",
        "E0911",
        "cannot move out of borrowed value",
    );
    rejects(
        "fn Read(&value) { { name } = value } fn main() {}",
        "E0911",
        "cannot move out of borrowed value",
    );
}

#[test]
fn borrow_checker_branch_loop_finally_keep_borrow_source_live() {
    accepts("fn Read(&value: str) { println(value) } fn main() { value = \"Ku\" if (true) { Read(value) } else { Read(value) } i = 0 while (i < 2) { Read(value) i++ } try { Read(value) } catch(err) { println(err.message) } finally { Read(value) } println(value) }");
    rejects(
        "fn Eat(value: str) {} fn main() { value = \"Ku\" if (true) { Eat(value) } println(value) }",
        "E0901",
        "use of moved value",
    );
}

#[test]
fn borrow_checker_generic_read_and_explicit_borrowed_higher_order() {
    accepts("fn Read<T>(&value: T) { println(value) } fn main() { text = \"Ku\" Read(text) Read(text) Read(1) println(text) }");
    rejects(
        "fn Take<T>(&value: T): T { return value } fn main() {}",
        "E0911",
        "cannot move out of borrowed value",
    );
    accepts("fn main() { fn Read(&text: str): int { return text.len() } x = \"Ku\" println(Read(x)) op: fn(&str): int = (&text: str): int => text.len() println(op(x)) println(x) }");
}

#[test]
fn borrow_checker_alias_scan_rejects_deep_expressions_without_recursing_unboundedly() {
    let sum = std::iter::repeat_n("1", 80).collect::<Vec<_>>().join(" + ");
    let source =
        format!("fn Read(&text: str, n: int) {{}} fn main() {{ text = \"Ku\" Read(text, {sum}) }}");
    let error = check_source("borrow.ku", &source).expect_err("depth must be bounded");
    assert!(
        error.message.contains("maximum check depth exceeded"),
        "{error}"
    );
}

#[test]
fn borrow_checker_json_lines_golden_and_success_silence() {
    use bounded_process::{run_bounded, OutputLimits};
    use std::{
        fs,
        process::Command,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    let directory = std::env::temp_dir().join(format!(
        "ku-borrow-diagnostics-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("borrow.ku");
    let path_text = path.to_string_lossy().replace('\\', "/");
    let execute = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ku"));
        command.args(["check", "--json", &path_text]);
        run_bounded(
            &mut command,
            Duration::from_secs(20),
            OutputLimits::new(64 * 1024, 64 * 1024),
        )
        .expect("bounded check")
    };
    fs::write(
        &path,
        "fn Inspect(&x: str): str {\n    return x\n}\nfn main() {}\n",
    )
    .unwrap();
    let failed = execute();
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    let diagnostic = String::from_utf8(failed.stderr).unwrap();
    assert_eq!(diagnostic.lines().count(), 1);
    assert_eq!(diagnostic.trim_end(), format!("{{\"level\":\"error\",\"code\":\"E0911\",\"message\":\"cannot move out of borrowed value rooted at 'x'; use '.clone()' to create an owned value\",\"file\":\"{path_text}\",\"line\":2,\"column\":12,\"endLine\":2,\"endColumn\":13,\"notes\":[\"a borrowed parameter does not own the value\"],\"helps\":[\"use '.clone()' to create an owned value before storing or returning it\"]}}"));
    fs::write(&path, "fn Inspect(&x: str): int { return x.len() } fn main() { x = \"Ku\" Inspect(x) println(x) }").unwrap();
    let passed = execute();
    fs::remove_dir_all(&directory).unwrap();
    assert!(
        passed.status.success(),
        "{}",
        String::from_utf8_lossy(&passed.stderr)
    );
    assert!(passed.stdout.is_empty());
    assert!(passed.stderr.is_empty());
}
