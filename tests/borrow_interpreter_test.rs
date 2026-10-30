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
        fn encode_alias(&value): str! { return json.encode(value) }
        fn copy(&text: str): str { return text.clone() }
        async fn worker(text: str): str! { return ok(copy(text)) }
        async fn main(): null! {
            value = { name: "Ku", items: [1, 2] }
            text = encode(value)?
            if (text != encode_alias(value)? || text != json.encode(value)?) { panic "JSON alias borrow" }
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
fn borrow_interpreter_json_parser_aliases_return_owned_values_and_release_error_loans() {
    ku::cli::run_source(
        "borrow_json_parse.ku",
        r#"
        struct Input { text: str }
        fn decode(&text: str): str! { value = json.decode(text)? return value.as_str() }
        fn parse(&text: str): str! { value = json.parse(text)? return value.as_str() }
        fn try_parse(&text: str): str! { value = json.try_parse(text)? return value.as_str() }
        fn main(): null! {
            input = Input { text: "\"" + "中文😀\\u0000tail\"" }
            decoded = decode(input.text)?
            if (parse(input.text)? != decoded || try_parse(input.text)? != decoded) { panic("parser aliases") }
            input.text = "changed"
            if (decoded.byte_len() != 15 || !decoded.starts_with("中文😀")) { panic("decoded storage") }
            bad = "{\"a\":[\"owned\",]}"
            caught = 0
            finalized = 0
            try { decode(bad)? } catch(error) { if (error.domain == "json" && error.code == "parse_error") { caught++ } } finally { finalized++ }
            try { parse(bad)? } catch(error) { if (error.domain == "json" && error.code == "parse_error") { caught++ } } finally { finalized++ }
            try { try_parse(bad)? } catch(error) { if (error.domain == "json" && error.code == "parse_error") { caught++ } } finally { finalized++ }
            bad = "\"recovered\""
            if (caught != 3 || finalized != 3 || decode(bad)? != "recovered") { panic("parser loan cleanup") }
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

#[test]
fn borrow_interpreter_json_take_owns_selected_subtree_and_crosses_await() {
    ku::cli::run_source("json-take.ku", r#"
import json from "std.json"
async fn pause(): int! { return ok(1) }
fn key(): str! { return ok("data") }
fn main_value(): str! { return json.encode((json.decode("{\"data\":\"temporary\"}")?).take(key()?)?) }
async fn main(): null! {
    text = "{\"data\":{\"text\":\"中文😀\\u0000\\n\\\"\\\\tail\",\"items\":[1,true,null]},\"unused\":\"drop me\"}"
    root = json.decode(text)?
    copied = root["data"]?
    selected = root.take(key()?)?
    text = "input released"
    awaited = (await pause())?
    if (awaited != 1 || json.encode(selected)? != json.encode(copied)?) { panic("selected subtree lifetime") }
    if (main_value()? != "\"temporary\"") { panic("temporary receiver") }
    named = json.decode("{\"take\":7,\"data\":null}")?
    if ((named.take("take")?).as_int()? != 7) { panic("ordinary JSON take key") }
    failures = 0
    finalized = 0
    try { (json.decode("{\"other\":1}")?).take("missing")? }
    catch(error) { if (error.domain == "object" && error.code == "missing_key" && error.message == "missing object key: missing") { failures++ } }
    finally { finalized++ }
    try { (json.decode("[1,2]")?).take("missing")? }
    catch(error) { if (error.domain == "object" && error.code == "type_unsupported" && error.message == "expected object value") { failures++ } }
    finally { finalized++ }
    if (failures != 2 || finalized != 2) { panic("take errors") }
    return ok(null)
}
"#).unwrap();
    let error = run_unchecked(
        r#"
fn read(&value) { value.take("data") }
fn main(): null! { root = json.decode("{\"data\":1}")? read(root) return ok(null) }
"#,
    )
    .unwrap_err();
    assert!(error.message.contains("borrowed"), "{error}");
}

#[test]
fn borrow_interpreter_callable_take_fields_keep_prior_dispatch_and_source_order() {
    ku::cli::run_source("callable-take.ku", r#"
struct ZeroReader { take: fn(): int, label: str }
struct KeyReader { take: fn(&str): int, label: str }
fn zero(): int { return 7 }
fn count(&text: str): int { return text.byte_len() }
fn inspect(&reader: KeyReader, &text: str): int { return reader.take(text) }
fn main() {
    x = ZeroReader { take: zero, label: "still owned" }
    if (x.take() != 7 || x.label != "still owned") { panic("zero callable field") }
    reader = KeyReader { take: count, label: "still owned" }
    text = "Ku"
    if (inspect(reader, text) != 2 || reader.label != "still owned") { panic("borrowed callable field") }
    object = { take: count, label: "object owned" }
    if (object.take(text) != 2 || object.label != "object owned" || text != "Ku") { panic("object callable field") }
    calls = 0
    arguments = 0
    fn make(): KeyReader { calls += 1 return KeyReader { take: count, label: "temporary" } }
    fn key(): str { arguments += 1 return "data" }
    if (make().take(key()) != 4 || calls != 1 || arguments != 1) { panic("temporary receiver evaluated once") }
}
"#).unwrap();
}
