use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

pub(super) fn assert_named_struct_view(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("struct-view-types.ku"),
        r#"struct Item { label: str count: int }
struct Packet { item: Item offsets: [int] }
fn Probe(&item: Packet): null! { return ok(null) }
fn See(value: int): null! { return ok(null) }
fn Own(item: Packet): null! { return ok(null) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("struct-view.ku"),
        r#"import { Packet as Envelope, Probe, See, Own } from "./struct-view-types.ku"
fn Inspect(&item: Envelope): null! {
    Probe(item)?
    See(item.item.count)?
    return ok(null)
}
fn Wrong(&item: Envelope): null! {
    Own(item)?
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "struct-view.ku\nstruct-view-types.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Inspect").unwrap();
    let output = |mode| {
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        execute(&mut Command::new(seed), cwd, 30)
    };
    let plan_output = output("linked-null-plan");
    assert_ok(&plan_output);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(
            String::from_utf8(plan_output.stdout).unwrap(),
        )],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected named View plan");
    };
    let Value::Object(plan) = *value else {
        panic!("expected plan object")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::Object(signature) = &export["signature"] else {
        panic!("expected signature")
    };
    let Value::Array(parameters) = &signature["parameters"] else {
        panic!("expected parameters")
    };
    let [Value::Object(parameter)] = parameters.as_slice() else {
        panic!("expected one parameter")
    };
    assert_eq!(parameter["kind"], Value::String("Struct".into()));
    assert_eq!(parameter["passing"], Value::String("view".into()));
    assert_eq!(parameter["source_name"], Value::String("Packet".into()));
    assert_eq!(
        parameter["source_path"],
        Value::String("struct-view-types.ku".into())
    );
    let as_text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        _ => panic!("expected a string field"),
    };
    let carrier = as_text(&parameter["c_type"]);
    let packet = carrier
        .strip_prefix("const ")
        .unwrap()
        .trim_end_matches('*');
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let symbol = |name: &str| {
        dependencies
            .iter()
            .find_map(|value| match value {
                Value::Object(decl) if decl["source_name"] == Value::String(name.into()) => {
                    Some(as_text(&decl["symbol"]))
                }
                _ => None,
            })
            .expect("bound dependency symbol")
    };
    let emitted = output("linked-null");
    assert_ok(&emitted);
    let generated = String::from_utf8(emitted.stdout).unwrap();
    let host = r#"
static const __PACKET__ *expected;
static int visited;
KuBootstrapResultNull __PROBE__(const __PACKET__ *item) {
    assert(item == expected && item->f0.f1 == 42);
    assert(item->f0.f0.len == 7 && outstanding() == 2);
    ++visited;
    return (KuBootstrapResultNull){.ok = true};
}
KuBootstrapResultNull __SEE__(int64_t value) {
    assert(value == 42 && visited == 1);
    ++visited;
    return (KuBootstrapResultNull){.ok = true};
}
static void after_null(void) {
    if (visited || outstanding()) _Exit(81);
}
int main(int argc, char **argv) {
    (void)test_realloc;
    (void)argv;
    if (argc == 2) {
        assert(atexit(after_null) == 0);
        (void)__ENTRY__(NULL);
        return 82;
    }
    uint8_t *original = malloc(7);
    memcpy(original, "payload", 7);
    int64_t *offsets = malloc(sizeof(*offsets));
    offsets[0] = 15;
    __PACKET__ owner = {0};
    owner.f0.f0 = (KuProjectionString){original,7,7,KU_PROJECTION_STRING_OWNED};
    owner.f0.f1 = 42;
    owner.f1.data = offsets;
    owner.f1.len = 1;
    owner.f1.capacity = 1;
    expected = &owner;
    assert(__ENTRY__(&owner).ok && visited == 2 && outstanding() == 2);
    assert(owner.f0.f0.ptr == original && owner.f1.data == offsets && owner.f1.data[0] == 15);
    free(original);
    free(offsets);
    assert(!outstanding());
    return 0;
}
"#
    .replace("__ENTRY__", &as_text(&export["symbol"]))
    .replace("__PROBE__", &symbol("Probe"))
    .replace("__SEE__", &symbol("See"))
    .replace("__PACKET__", packet);
    let c = cwd.join("named-record-view.c");
    let app = cwd.join(format!("named-record-view{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{generated}{host}")).unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    let invalid = execute(Command::new(&app).arg("null"), cwd, 15);
    assert_eq!(invalid.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&invalid.stderr).trim(),
        "invalid struct view"
    );
    fs::write(cwd.join("function.txt"), "Wrong").unwrap();
    let rejected = output("linked-null");
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_view_escape"));
}
