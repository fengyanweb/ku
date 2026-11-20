use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

pub(super) fn assert_named_struct_view(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("struct-view-types.ku"),
        r#"struct Item { label: str count: int }
struct Packet { item: Item offsets: [int] branches: [Item] }
fn Probe(&item: Packet): null! { return ok(null) }
fn See(value: int): null! { return ok(null) }
fn Own(item: Packet): null! { return ok(null) }
fn ProbeArray(&items: [Item]): null! { return ok(null) }
fn Pair(&items: [Item], text: str): null! { return ok(null) }
fn Consume(item: Packet): str! { output = "taken"; return ok(output) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("struct-view.ku"),
        r#"import { Packet as Envelope, Probe, See, Own, ProbeArray, Pair, Consume } from "./struct-view-types.ku"
fn Inspect(&item: Envelope): null! {
    Probe(item)?
    See(item.item.count)?
    return ok(null)
}
fn Wrong(&item: Envelope): null! {
    Own(item)?
    return ok(null)
}
fn InspectArray(&items: [Envelope]): null! {
    See(items[0].item.count)?
    return ok(null)
}
fn InspectNested(&item: Envelope): null! {
    ProbeArray(item.branches)?
    See(item.branches[0].count)?
    return ok(null)
}
fn WrongLoan(item: Envelope): null! {
    Pair(item.branches, Consume(item)?)?
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
    fs::write(cwd.join("function.txt"), "InspectArray").unwrap();
    let array_c = output("linked-null");
    assert_ok(&array_c);
    let array_program = String::from_utf8(array_c.stdout).unwrap();
    let signature = array_program
        .lines()
        .find(|line| line.starts_with("KuBootstrapResultNull ku_linked_null_"))
        .expect("checked array entry");
    let (_, after_result) = signature.split_once(' ').unwrap();
    let (array_entry, formal) = after_result.split_once("(const ").unwrap();
    let (array_type, _) = formal.split_once('*').unwrap();
    let witness = r#"
static int seen;
KuBootstrapResultNull __SEE__(int64_t value) {
    assert(value == 42);
    ++seen;
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    (void)test_realloc;
    __PACKET__ element = {0};
    int64_t *offsets = malloc(sizeof(*offsets));
    offsets[0] = 19;
    uint8_t *text = malloc(3);
    memcpy(text, "abc", 3);
    element.f0.f0 = (KuProjectionString){text,3,3,KU_PROJECTION_STRING_OWNED};
    element.f0.f1 = 42;
    element.f1.data = offsets;
    element.f1.len = 1;
    element.f1.capacity = 1;
    __ARRAY__ collection = {1,&element,1};
    assert(__ENTRY__(&collection).ok && seen == 1);
    assert(outstanding() == 2 && offsets[0] == 19 && memcmp(text, "abc", 3) == 0);
    free(offsets);
    free(text);
    assert(!outstanding());
    return 0;
}
"#
    .replace("__SEE__", &symbol("See"))
    .replace("__PACKET__", packet)
    .replace("__ARRAY__", array_type)
    .replace("__ENTRY__", array_entry);
    let c = cwd.join("nested-record-array-view.c");
    let app = cwd.join(format!(
        "nested-record-array-view{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(
        &c,
        format!("{CHECKED_ARRAY_ALLOCATOR}{array_program}{witness}"),
    )
    .unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));

    fs::write(cwd.join("function.txt"), "InspectNested").unwrap();
    let projected = output("linked-null");
    assert_ok(&projected);
    let projected = String::from_utf8(projected.stdout).unwrap();
    let definition = |prefix: &str| {
        let line = projected
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap();
        let (function, _) = line.split_once('(').unwrap();
        function.rsplit_once(' ').unwrap().1
    };
    let probe_signature = projected
        .lines()
        .find(|line| {
            line.starts_with("extern KuBootstrapResultNull") && line.contains("(const KuArray_")
        })
        .expect("checked nested view prototype");
    let (before_argument, argument) = probe_signature.split_once("(const ").unwrap();
    let (_, nested_probe) = before_argument.rsplit_once(' ').unwrap();
    let (nested_type, _) = argument.split_once('*').unwrap();
    let nested = r#"
static const __ARRAY__ *expected;
static int checks;
KuBootstrapResultNull __PROBE__(const __ARRAY__ *items) {
    assert(items == expected && items->len == 1 && items->data[0].f1 == 42);
    ++checks;
    return (KuBootstrapResultNull){.ok = true};
}
KuBootstrapResultNull __SEE__(int64_t count) {
    assert(count == 42 && checks == 1);
    ++checks;
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    (void)test_realloc;
    __PACKET__ owner = {0};
    owner.f2.data = malloc(sizeof(*owner.f2.data));
    memset(owner.f2.data, 0, sizeof(*owner.f2.data));
    owner.f2.len = owner.f2.capacity = 1;
    uint8_t *text = malloc(7);
    memcpy(text, "payload", 7);
    owner.f2.data[0].f0 = (KuProjectionString){text,7,7,KU_PROJECTION_STRING_OWNED};
    owner.f2.data[0].f1 = 42;
    expected = &owner.f2;
    assert(__ENTRY__(&owner).ok && checks == 2 && outstanding() == 2);
    assert(owner.f2.data[0].f0.ptr == text && memcmp(text, "payload", 7) == 0);
    free(text);
    free(owner.f2.data);
    assert(!outstanding());
    return 0;
}
"#
    .replace(
        "__ENTRY__",
        definition("KuBootstrapResultNull ku_linked_null_"),
    )
    .replace("__PACKET__", packet)
    .replace("__PROBE__", nested_probe)
    .replace("__ARRAY__", nested_type)
    .replace("__SEE__", &symbol("See"));
    let c = cwd.join("nested-record-field-view.c");
    let app = cwd.join(format!(
        "nested-record-field-view{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{projected}{nested}")).unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    fs::write(cwd.join("function.txt"), "WrongLoan").unwrap();
    let refused = output("linked-null");
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("linked_view_move"));

    fs::write(cwd.join("function.txt"), "Wrong").unwrap();
    let rejected = output("linked-null");
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_view_escape"));
}
