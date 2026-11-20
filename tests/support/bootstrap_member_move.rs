use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

pub(super) fn assert_owned_member_transfer(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("member-move-providers.ku"),
        r#"struct Pair { first: str second: str }
struct Packet { pair: Pair spare: str }
fn Mix(first: str, second: str): str { return first + second }
fn Checked(second: str, bad: bool): str! { return ok(second) }
fn Observe(&first: str, second: str): null! { return ok(null) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("member-move-source.ku"),
        r#"import { Packet, Mix, Checked, Observe } from "./member-move-providers.ku"
fn MoveMembers(packet: Packet, bad: bool): str! {
    return ok(Mix(packet.pair.first, Checked(packet.pair.second, bad)?))
}
fn Duplicate(packet: Packet): str! {
    return ok(Mix(packet.pair.first, packet.pair.first))
}
fn InvalidateLoan(packet: Packet): null! {
    Observe(packet.pair.first, packet.pair.first)?
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "member-move-source.ku\nmember-move-providers.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "MoveMembers").unwrap();
    let emit = |mode| {
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        execute(&mut Command::new(seed), cwd, 30)
    };
    let emitted_plan = emit("linked-null-plan");
    assert_ok(&emitted_plan);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(
            String::from_utf8(emitted_plan.stdout).unwrap(),
        )],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected member move plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected plan")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::Object(signature) = &export["signature"] else {
        panic!("expected signature")
    };
    let Value::Array(parameters) = &signature["parameters"] else {
        panic!("expected parameter list")
    };
    let Value::Object(packet) = &parameters[0] else {
        panic!("expected owning record")
    };
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected link dependencies")
    };
    let as_text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        _ => panic!("expected string"),
    };
    let symbol = |name: &str| {
        dependencies
            .iter()
            .find_map(|candidate| match candidate {
                Value::Object(decl) if decl["source_name"] == Value::String(name.into()) => {
                    Some(as_text(&decl["symbol"]))
                }
                _ => None,
            })
            .expect("typed provider")
    };
    let c = emit("linked-null");
    assert_ok(&c);
    let host = r#"
static uint8_t *first_ptr, *second_ptr;
static int checked_calls, mixed_calls;
KuBootstrapResult_KuProjectionString __CHECKED__(KuProjectionString second, bool bad) {
    assert(second.ptr == second_ptr && second.len == 4);
    ++checked_calls;
    if (bad) return (KuBootstrapResult_KuProjectionString){.error = {.message = second}};
    return (KuBootstrapResult_KuProjectionString){.ok = true, .value = second};
}
KuProjectionString __MIX__(KuProjectionString first, KuProjectionString second) {
    assert(first.ptr == first_ptr && first.len == 5);
    assert(second.ptr == second_ptr && second.len == 4);
    ++mixed_calls;
    ku_projection_string_drop(&second);
    return first;
}
static KuProjectionString owned(const char *value, size_t size) {
    uint8_t *bytes = malloc(size);
    memcpy(bytes,value,size);
    return (KuProjectionString){bytes,size,size,KU_PROJECTION_STRING_OWNED};
}
static __PACKET__ packet(void) {
    __PACKET__ value = {0};
    value.f0.f0 = owned("first", 5);
    value.f0.f1 = owned("next", 4);
    value.f1 = owned("spare", 5);
    first_ptr = value.f0.f0.ptr;
    second_ptr = value.f0.f1.ptr;
    return value;
}
int main(void) {
    (void)test_realloc;
    KuBootstrapResult_KuProjectionString result = __ENTRY__(packet(), false);
    assert(result.ok && result.value.ptr == first_ptr);
    assert(mixed_calls == 1 && checked_calls == 1 && outstanding() == 1);
    ku_projection_string_drop(&result.value);
    assert(outstanding() == 0);
    result = __ENTRY__(packet(), true);
    assert(!result.ok && result.error.message.ptr == second_ptr);
    assert(mixed_calls == 1 && checked_calls == 2 && outstanding() == 1);
    ku_projection_string_drop(&result.error.message);
    assert(outstanding() == 0);
    return 0;
}
"#
    .replace("__ENTRY__", &as_text(&export["symbol"]))
    .replace("__CHECKED__", &symbol("Checked"))
    .replace("__MIX__", &symbol("Mix"))
    .replace("__PACKET__", &as_text(&packet["c_type"]));
    let source = cwd.join("member-move.c");
    let app = cwd.join(format!("member-move{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{}{host}",
            String::from_utf8(c.stdout).unwrap()
        ),
    )
    .unwrap();
    compile_projection(&source, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    for (function, diagnostic) in [
        ("Duplicate", "linked_moved_member"),
        ("InvalidateLoan", "linked_view_move"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = emit("linked-null");
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}
