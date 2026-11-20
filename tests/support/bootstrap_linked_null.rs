use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

const ENTRY: &str = r#"import { Piece as Content, Envelope, Consume } from "./point-provider.ku"
fn Start(text: str, note: str, reject: bool): null! {
    snapshot = Envelope { inner: Content { text: text }, comment: note.clone() }
    Consume(snapshot, reject)?
    return ok(null)
}
"#;

const PROVIDER: &str = r#"struct Piece { text: str }
struct Envelope { inner: Piece comment: str }
fn Consume(input: Envelope, reject: bool): null! { return ok(null) }
"#;

const DRIVER: &str = r#"
static unsigned called;
static uint8_t *original;
static uint8_t *original_note;
KuBootstrapResultNull __DEPENDENCY__(__ENVELOPE__ input, bool reject) {
    ++called;
    assert(input.f0.f0.ptr == original && input.f0.f0.len == 7);
    assert(memcmp(input.f0.f0.ptr, "payload", 7) == 0);
    assert(input.f1.storage == KU_PROJECTION_STRING_OWNED && input.f1.len == 6 && input.f1.ptr != original_note);
    assert(memcmp(input.f1.ptr, "a note", 6) == 0);
    ku_projection_string_drop(&input.f1);
    KuBootstrapResultNull result = {0};
    if (reject) result.error.message = input.f0.f0;
    else {
        ku_projection_string_drop(&input.f0.f0);
        result.ok = true;
    }
    return result;
}
static void after_failure(void) {
    if (outstanding() || called || attempts != 3) _Exit(82);
}
static KuProjectionString owned(void) {
    original = malloc(7);
    memcpy(original, "payload", 7);
    return (KuProjectionString){original,7,7,KU_PROJECTION_STRING_OWNED};
}
static KuProjectionString owned_note(void) {
    original_note = malloc(6);
    memcpy(original_note, "a note", 6);
    return (KuProjectionString){original_note,6,6,KU_PROJECTION_STRING_OWNED};
}
int main(int argc, char **argv) {
    (void)test_realloc;
    (void)argv;
    if (argc == 2) {
        KuProjectionString input = owned();
        KuProjectionString note = owned_note();
        assert(atexit(after_failure) == 0);
        fail_after = 0;
        (void)__ENTRY__(input, note, false);
        return 83;
    }
    KuBootstrapResultNull result = __ENTRY__(owned(), owned_note(), true);
    assert(!result.ok && result.error.message.ptr == original && result.error.message.len == 7);
    assert(outstanding() == 1 && called == 1 && attempts == 3);
    ku_projection_string_drop(&result.error.message);
    result = __ENTRY__(owned(), owned_note(), false);
    assert(result.ok && result.value == 0 && !result.error.message.ptr);
    assert(!outstanding() && called == 2 && attempts == 6);
    return 0;
}
"#;

pub(super) fn assert_checked_owned_call(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("point-entry.ku"), ENTRY).unwrap();
    fs::write(cwd.join("point-provider.ku"), PROVIDER).unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "point-entry.ku\npoint-provider.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Start").unwrap();
    let emit = |mode: &str| {
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        execute(&mut Command::new(seed), cwd, 30)
    };
    let metadata = emit("linked-null-plan");
    assert_ok(&metadata);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(metadata.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected JSON link plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected link plan object")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let [Value::Object(dependency)] = dependencies.as_slice() else {
        panic!("expected exactly one dependency")
    };
    assert_eq!(
        dependency["source_path"],
        Value::String("point-provider.ku".into())
    );
    let Value::Object(signature) = &dependency["signature"] else {
        panic!("expected dependency signature")
    };
    assert_eq!(signature["returns_result"], Value::Bool(true));
    let Value::Array(parameters) = &signature["parameters"] else {
        panic!("expected positional parameters")
    };
    let Value::Object(owner) = &parameters[0] else {
        panic!("expected bound owner argument")
    };
    let Value::String(owner_type) = &owner["c_type"] else {
        panic!("expected C owner carrier")
    };
    let Value::String(symbol) = &export["symbol"] else {
        panic!("expected C export symbol")
    };
    let Value::String(callee) = &dependency["symbol"] else {
        panic!("expected C dependency symbol")
    };
    let output = emit("linked-null");
    assert_ok(&output);
    let program = String::from_utf8(output.stdout).unwrap();
    let c = cwd.join("checked-owned-call.c");
    let exe = cwd.join(format!(
        "checked-owned-call{}",
        std::env::consts::EXE_SUFFIX
    ));
    let driver = DRIVER
        .replace("__ENTRY__", symbol)
        .replace("__DEPENDENCY__", callee)
        .replace("__ENVELOPE__", owner_type);
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{program}{driver}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 10));
    let failure = execute(Command::new(&exe).arg("allocation-failure"), cwd, 10);
    assert_eq!(failure.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(failure.stderr).unwrap().trim(),
        "out of memory"
    );

    fs::write(cwd.join("modules.txt"), "point-entry.ku\n").unwrap();
    for (program, diagnostic) in [
        ("struct Item { text: str id: int }\nfn Start(): null! { value = Item { text: \"missing\" } return ok(null) }",
         "missing_linked_literal_field"),
        ("struct Item { text: str }\nfn Start(): null! { value = Item { text: 4 } return ok(null) }",
         "linked_literal_field_type"),
        ("fn Target(): int { return 0 }\nfn Start(): null! { Target()? return ok(null) }",
         "linked_dependency_result"),
        ("fn Start(ok: bool): null! { return ok(null) }",
         "unsupported_linked_success"),
    ] {
        fs::write(cwd.join("point-entry.ku"), program).unwrap();
        let rejected = emit("linked-null");
        assert!(!rejected.status.success() && rejected.stdout.is_empty());
        let error = String::from_utf8(rejected.stderr).unwrap();
        assert!(error.contains(diagnostic), "{diagnostic}, got {error}");
    }
}
