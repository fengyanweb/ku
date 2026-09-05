use ku::{interpreter::Interpreter, lexer::Lexer, parser::Parser};

fn run_unchecked(source: &str) -> ku::error::KuResult<()> {
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    Interpreter::new().run(program)
}

#[test]
fn borrow_interpreter_repeated_nested_projection_reborrow_and_temporary() {
    run_unchecked(r#"
        struct User { name: str, names: [str] }
        fn size(&name: str): int { return name.len() }
        fn inspect(&user: User): str {
            if (size(user.name) != 2) { panic "name" }
            if (size(user.names[0]) != 2) { panic "index" }
            if (user.names.len() != 2) { panic "array len" }
            if (!(user.name == user.names[0])) { panic "equal" }
            if (!user.name.contains("K")) { panic "contains" }
            return "Hello " + user.name
        }
        fn compare(&left: User, &right: User): bool { return left == right }
        fn main() {
            user = User { name: "Ku", names: ["Ku", "世界"] }
            if (inspect(user) != "Hello Ku") { panic "first" }
            if (inspect(user) != "Hello Ku") { panic "second" }
            if (!compare(user, user)) { panic "same root" }
            if (user.name != "Ku") { panic "source" }
            if (inspect(User { name: "Ku", names: ["Ku", "x"] }) != "Hello Ku") { panic "temporary" }
        }
    "#).unwrap();
}

#[test]
fn borrow_interpreter_function_values_local_functions_clone_and_finally() {
    run_unchecked(
        r#"
        fn copy(&text: str): str { return text.clone() }
        fn invoke(&reader: fn(&str): str, &text: str): str { return reader(text) }
        fn invoke_field(&holder, &text: str): str { return holder.reader(text) }
        fn failing(&text: str): null! { try { fail "expected" } finally { println(text) } }
        fn main() {
            fn local(&text: str): str { return `value: {text}` }
            reader: fn(&str): str = copy
            arrow = (&text: str): str => text.clone()
            text = "Ku"
            copied = reader(text)
            if (invoke(reader, text) != copied) { panic "borrowed callable" }
            holder = { reader: copy }
            if (invoke_field(holder, text) != copied) { panic "borrowed callable field" }
            if (arrow(text) != copied) { panic "fn value" }
            if (local(text) != "value: Ku") { panic "local" }
            try { failing(text)? } catch (error) { println(error.message) }
            text += "!"
            if (text != "Ku!" || copied != "Ku") { panic "clone lifetime" }
        }
    "#,
    )
    .unwrap();
}

#[test]
fn borrow_interpreter_json_stringify_and_sync_borrow_inside_async() {
    ku::cli::run_source(
        "borrow_json.ku",
        r#"
        import { json } from "std"
        fn encode(&value): str! { return json.stringify(value) }
        fn copy(&text: str): str { return text.clone() }
        async fn worker(text: str): str! { return ok(copy(text)) }
        async fn main(): null! {
            value = { name: "Ku", items: [1, 2] }
            text = encode(value)?
            if (!text.contains("Ku")) { panic "json" }
            println(value)
            result = (await worker("Ku"))?
            if (result != "Ku") { panic "async sync borrow" }
            return ok(null)
        }
    "#,
    )
    .unwrap();
}

#[test]
fn borrow_interpreter_readonly_stdlib_and_copy_enum_payload() {
    run_unchecked(
        r#"
        enum Message { Text(value: str), Code(value: int) }
        fn code(&message: Message): int {
            return match message {
                Message.Code(value) => value
                Message.Text(_) => 0
            }
        }
        fn read(&text: str, &items: [int]): int {
            if (!string.starts_with(text, "K") || !text.ends_with("u")) { panic "string read" }
            if (string.byte_len(text) != 2 || string.len(text) != 2) { panic "string length" }
            if (array.is_empty(items) || array.len(items) != 3) { panic "array read" }
            return items[1]
        }
        fn main() {
            text = "Ku"
            items = [1, 2, 3]
            if (read(text, items) != 2 || len(items) != 3) { panic "read" }
            message = Message.Code(7)
            if (code(message) != 7) { panic "copy payload" }
            println(text)
            println(items)
            if (str(text) != "Ku") { panic "str" }
        }
    "#,
    )
    .unwrap();
}

#[test]
fn borrow_interpreter_defensively_rejects_mutation_escape_capture_and_async() {
    for (source, expected) in [
        (
            r#"fn bad(&s: str) { s = "bad" } fn main() { s = "Ku" bad(s) }"#,
            "borrowed",
        ),
        (
            r#"fn bad(&s) { s.name = "bad" } fn main() { s = { name: "Ku" } bad(s) }"#,
            "borrowed",
        ),
        (
            r#"fn bad(&s) { s[0] = "bad" } fn main() { s = ["Ku"] bad(s) }"#,
            "borrowed",
        ),
        (
            r#"fn bad(&s: str): str { return s } fn main() { s = "Ku" bad(s) }"#,
            "borrowed",
        ),
        (
            r#"fn bad(&s: str) { saved = s } fn main() { s = "Ku" bad(s) }"#,
            "borrowed",
        ),
        (
            r#"fn bad(&s: str) { saved = [s] } fn main() { s = "Ku" bad(s) }"#,
            "borrowed",
        ),
        (
            r#"fn bad(&s: str) { callback = () => s.clone() } fn main() { s = "Ku" bad(s) }"#,
            "capture borrowed",
        ),
        (
            r#"async fn bad(&s: str) {} fn main() { s = "Ku" bad(s) }"#,
            "async functions cannot declare borrowed",
        ),
        (
            r#"fn consume(s: str) {} fn bad(&s: str) { consume(s) } fn main() { s = "Ku" bad(s) }"#,
            "owning parameter",
        ),
        (
            r#"fn mix(left: str, &right: str) {} fn main() { s = "Ku" mix(s, s) }"#,
            "borrow conflicts with move",
        ),
        (
            r#"fn mix(&left: str, right: str) {} fn main() { s = "Ku" mix(s, s) }"#,
            "borrow conflicts with move",
        ),
        (
            r#"fn bad(&s: [str]) { for item in s { println(item) } } fn main() { s = ["Ku"] bad(s) }"#,
            "for over a borrowed",
        ),
    ] {
        let error = run_unchecked(source).expect_err(source);
        assert!(error.message.contains(expected), "{source}: {error:?}");
    }
}
