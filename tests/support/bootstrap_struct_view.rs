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

const INT_VIEW_SOURCE: &str = r#"struct Graph { edges: [int] }
fn See(value: int): null! { return ok(null) }
fn Check(&edges: [int]): null! {
    first = edges[0]
    See(first)?
    return ok(null)
}
fn Inspect(&graph: Graph): null! {
    Check(graph.edges)?
    See(graph.edges[0])?
    return ok(null)
}
fn Wrong(&edges: [str]): null! {
    Check(edges)?
    return ok(null)
}
"#;

const INT_VIEW_HOST: &str = r#"
static int calls;
KuBootstrapResultNull __SEE__(int64_t value) {
    assert(value == INT64_MIN);
    ++calls;
    return (KuBootstrapResultNull){.ok = true};
}
int main(int argc, char **argv) {
    int64_t data[] = { INT64_MIN };
    KuArrayInt edges = {1, data, 1};
    if (argc == 2) {
        if (!strcmp(argv[1], "bounds")) { edges.len = 0; edges.capacity = 0; }
        else { edges.len = 2; }
    }
    KuBootstrapResultNull result = __ENTRY__(&edges);
    assert(argc == 1 && result.ok && calls == 1);
    assert(edges.data == data && edges.len == 1 && edges.capacity == 1 && data[0] == INT64_MIN);
    return 0;
}
"#;

pub(super) fn assert_checked_int_view(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("int-view.ku"), INT_VIEW_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "int-view.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Check").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(plan.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected integer View link plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected typed plan")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::Object(signature) = &export["signature"] else {
        panic!("expected signature")
    };
    let Value::Array(params) = &signature["parameters"] else {
        panic!("expected params")
    };
    let [Value::Object(view)] = params.as_slice() else {
        panic!("expected one View")
    };
    assert_eq!(view["kind"], Value::String("BuiltinArray".into()));
    assert_eq!(view["source_name"], Value::String("int".into()));
    assert_eq!(view["c_type"], Value::String("const KuArrayInt*".into()));
    let Value::Array(deps) = &plan["dependencies"] else {
        panic!("expected dependency")
    };
    let [Value::Object(see)] = deps.as_slice() else {
        panic!("expected Copy consumer")
    };
    let (Value::String(entry), Value::String(see)) = (&export["symbol"], &see["symbol"]) else {
        panic!("expected symbols")
    };
    let host = INT_VIEW_HOST
        .replace("__ENTRY__", entry)
        .replace("__SEE__", see);
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let c = cwd.join("int-view.c");
    let exe = cwd.join(format!("int-view{}", std::env::consts::EXE_SUFFIX));
    let source = String::from_utf8(emitted.stdout).unwrap();
    fs::write(&c, format!("#include <assert.h>\n{source}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    for (argument, reason) in [
        ("bounds", "index 0 out of bounds for length 0"),
        ("header", "invalid struct array ownership"),
    ] {
        let outcome = execute(Command::new(&exe).arg(argument), cwd, 15);
        assert_eq!(outcome.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&outcome.stderr).contains(reason));
    }
    fs::write(cwd.join("function.txt"), "Inspect").unwrap();
    let nested = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&nested);
    fs::write(&c, nested.stdout).unwrap();
    super::compile_projection_product(&c, &cwd.join("int-view.o"), cwd, true, true);

    fs::write(cwd.join("function.txt"), "Wrong").unwrap();
    let wrong = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(wrong.status.code(), Some(1));
    assert!(wrong.stdout.is_empty());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("linked_argument_type"));
}

const FIELD_CLONE_SOURCE: &str = r#"struct Stamp { value: int }
struct Index { stamp: Stamp offsets: [int] }
struct Wrapped { index: Index }
fn See(value: int): null! { return ok(null) }
fn Clone(&owner: Wrapped, stamp: Stamp): null! {
    copied = stamp.clone()
    See(copied.value)?
    return ok(null)
}
fn Refuse(&values: [Wrapped]): null! {
    copied = values[0].clone()
    return ok(null)
}
struct Pair { left: Index right: Index }
fn Take(stamp: Stamp): null! { return ok(null) }
fn TakeIndex(index: Index): null! { return ok(null) }
fn Guard(&owner: Wrapped, stamp: Stamp, active: bool): null! {
    if (active) { Take(stamp)? }
    return ok(null)
}
fn ReuseAfterMove(&owner: Wrapped, stamp: Stamp, active: bool): null! {
    if (active) { Take(stamp)? }
    See(stamp.value)?
    return ok(null)
}
fn ConditionalArrayMove(index: Index, active: bool): null! {
    if (active) { TakeIndex(index)? }
    return ok(null)
}
fn DifferentMembers(pair: Pair, active: bool): null! {
    if (active) { TakeIndex(pair.left)? }
    else { TakeIndex(pair.right)? }
    return ok(null)
}
"#;

fn scalar_record_test_symbols(code: &str) -> (&str, &str, &str, &str) {
    let (entry, params) = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
        .unwrap()
        .split_once("(const ")
        .unwrap();
    let (owner, stamp) = params.split_once("* a0, ").unwrap();
    let (stamp, _) = stamp.split_once(' ').unwrap();
    let dependency = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    (entry, owner, stamp, dependency)
}

pub(super) fn assert_checked_field_clone(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("field-clone.ku"), FIELD_CLONE_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "field-clone.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Clone").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (entry, owner, stamp, see) = scalar_record_test_symbols(&code);
    assert!(!code.contains("_clone_element"));
    let c = cwd.join("field-clone.c");
    let exe = cwd.join(format!("field-clone{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &c,
        format!(
            "#include <assert.h>\n{code}\n\
            KuBootstrapResultNull {see}(int64_t v) {{ assert(v == 47); return (KuBootstrapResultNull){{.ok = true}}; }}\n\
            int main(void) {{ {owner} owner = {{0}}; {stamp} stamp = {{47}}; \
            assert(ku_linked_null_{entry}(&owner, stamp).ok); return 0; }}\n"
        ),
    )
    .unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));

    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_nested_array_clone"));

    fs::write(cwd.join("function.txt"), "Guard").unwrap();
    let guard = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&guard);
    let code = String::from_utf8(guard.stdout).unwrap();
    let (entry, owner, stamp, take) = scalar_record_test_symbols(&code);
    let host = r#"
static int called;
KuBootstrapResultNull __TAKE__(__STAMP__ stamp) {
    ++called;
    assert(stamp.f0 == 47 || stamp.f0 == -47);
    if (stamp.f0 > 0) return (KuBootstrapResultNull){.ok = true};
    KuBootstrapResultNull error = {0};
    const char *code = "injected";
    error.error.code = (KuProjectionString){(uint8_t *)code,8,0,KU_PROJECTION_STRING_STATIC};
    return error;
}

int main(void) {
    __OWNER__ parent = {0};
    assert(__GUARD__(&parent, (__STAMP__){47}, false).ok && called == 0);
    assert(__GUARD__(&parent, (__STAMP__){47}, true).ok && called == 1);
    KuBootstrapResultNull rejected = __GUARD__(&parent, (__STAMP__){-47}, true);
    assert(!rejected.ok && called == 2 && rejected.error.code.len == 8);
    assert(!memcmp(rejected.error.code.ptr, "injected", 8));
    return 0;
}
"#
    .replace("__GUARD__", &format!("ku_linked_null_{entry}"))
    .replace("__TAKE__", take)
    .replace("__OWNER__", owner)
    .replace("__STAMP__", stamp);
    fs::write(
        &c,
        format!("#include <assert.h>\n#include <string.h>\n{code}{host}"),
    )
    .unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));

    fs::write(cwd.join("function.txt"), "ConditionalArrayMove").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (entry, arguments) = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap();
    let record = arguments.split_once(' ').unwrap().0;
    let take = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
static int visits;
KuBootstrapResultNull __TAKE__(__RECORD__ value) {
    ++visits;
    assert(value.f0.f0 == 47 && value.f1.len == 2 && outstanding() == 1);
    assert(value.f1.data[0] == 7);
    bool accept = value.f1.data[1] == 8;
    free(value.f1.data);
    if (accept) return (KuBootstrapResultNull){.ok = true};
    static uint8_t code[] = "refused";
    return (KuBootstrapResultNull){
        .error.code = {code, sizeof(code) - 1, 0, KU_PROJECTION_STRING_STATIC}
    };
}
static __RECORD__ value(int64_t last) {
    int64_t *data = malloc(2 * sizeof(*data));
    data[0] = 7;
    data[1] = last;
    return (__RECORD__){{47},{2,data,2}};
}
int main(void) {
    (void)test_realloc;
    assert(__ENTRY__(value(8), false).ok && visits == 0 && !outstanding());
    assert(__ENTRY__(value(8), true).ok && visits == 1 && !outstanding());
    KuBootstrapResultNull failed = __ENTRY__(value(9), true);
    assert(!failed.ok && visits == 2 && !outstanding() && attempts == 3);
    assert(failed.error.code.len == 7 && !memcmp(failed.error.code.ptr, "refused", 7));
    return 0;
}
"#
    .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
    .replace("__TAKE__", take)
    .replace("__RECORD__", record);
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));

    for (name, reason) in [
        ("ReuseAfterMove", "unavailable_linked_binding"),
        ("DifferentMembers", "linked_control_move"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains(reason),
            "{name}"
        );
    }
}

const OWNED_INT_SOURCE: &str = r#"struct Index { values: [int] size: int }
fn See(&values: [int], size: int): null! { return ok(null) }
fn Build(value: int): Index! {
    values = [value, 2]
    values = values.push(3)
    values = values.push(4)
    values = values.push(5)
    values = values.push(6)
    values = values.push(7)
    values = values.push(8)
    values = values.push(values[0])
    size = values.len()
    See(values, size)?
    return ok(Index { values: values, size: size })
}
fn Wrong(&values: [int]): Index! {
    return ok(Index { values: values, size: 0 })
}
fn WrongLiteral(value: int): Index! {
    values = [value, true]
    return ok(Index { values: values, size: 2 })
}
fn Carry(values: [int], extra: int): [int] {
    values = values.push(extra)
    return values
}
fn Arithmetic(left: int, divisor: int, factor: int): Index! {
    values = [left]
    quotient = values[0] / divisor
    product = quotient * factor
    return ok(Index { values: values, size: product })
}
fn Reuse(value: int): Index! {
    values: [int] = []
    result = Index { values: values, size: 0 }
    size = values.len()
    return ok(result)
}
struct Limit { count: int wrong: bool }
fn Counted(&limit: Limit): Index! {
    values: [int] = []
    index = 0
    while (index < limit.count) {
        values = values.push(index)
        index = index + 1
    }
    return ok(Index { values: values, size: index })
}
fn CountedMember(&limit: Index): Index! {
    values: [int] = []
    index = 0
    while (index < limit.values.len()) {
        values = values.push(index)
        index = index + 1
    }
    return ok(Index { values: values, size: index })
}
fn WrongBound(&limit: Limit): null! {
    index = 0
    while (index < limit.wrong) { index = index + 1 }
    return ok(null)
}
fn WrongIncrement(&limit: Limit): null! {
    index = 0
    while (index < limit.count) { index = index + 2 }
    return ok(null)
}
"#;

const OWNED_INT_HOST: &str = r#"
static const int64_t *observed;
static int want_attempts;
static void check_cleanup(void) {
    if (outstanding() || attempts != want_attempts) _Exit(89);
}
KuBootstrapResultNull __SEE__(const KuArrayInt *values, int64_t size) {
    assert(values->len == 9 && size == 9);
    assert(values->data[0] == INT64_MAX && values->data[8] == INT64_MAX);
    observed = values->data;
    return (KuBootstrapResultNull){.ok = true};
}
int main(int argc, char **argv) {
    (void)test_malloc;
    if (argc == 2) {
        fail_after = !strcmp(argv[1], "growth");
        want_attempts = fail_after + 1;
        assert(atexit(check_cleanup) == 0);
        (void)__BUILD__(INT64_MAX);
        return 90;
    }
    __RESULT__ result = __BUILD__(INT64_MAX);
    assert(result.ok && attempts == 2 && outstanding() == 1);
    assert(result.value.f1 == 9 && result.value.f0.len == 9 && result.value.f0.data == observed);
    __DROP__(&result.value);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_owned_int_locals(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("owned-int.ku"), OWNED_INT_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "owned-int.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Build").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, entry) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, _) = entry.split_once('(').unwrap();
    let see = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let dropped = code
        .lines()
        .find_map(|line| line.strip_prefix("static void ku_drop_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = OWNED_INT_HOST
        .replace("__BUILD__", &format!("ku_linked_null_{entry}"))
        .replace("__RESULT__", result)
        .replace("__SEE__", see)
        .replace("__DROP__", &format!("ku_drop_{dropped}"));
    let c = cwd.join("owned-int.c");
    let exe = cwd.join(format!("owned-int{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    for mode in ["first", "growth"] {
        let failure = execute(Command::new(&exe).arg(mode), cwd, 15);
        assert_eq!(failure.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&failure.stderr).contains("array allocation failed"));
    }
    for (name, reason) in [
        ("Wrong", "linked_view_escape"),
        ("WrongLiteral", "linked_int_array_literal"),
        ("Reuse", "unavailable_linked_binding"),
        ("WrongBound", "unsupported_linked_loop"),
        ("WrongIncrement", "unsupported_linked_loop"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(reason));
    }

    fs::write(cwd.join("function.txt"), "Counted").unwrap();
    let counted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&counted);
    let code = String::from_utf8(counted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, params) = signature.split_once("(const ").unwrap();
    let (limit, _) = params.split_once('*').unwrap();
    let drop = code
        .lines()
        .find_map(|line| line.strip_prefix("static void ku_drop_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
int main(void) {
    __LIMIT__ limit = {-1,false};
    __RESULT__ result = __ENTRY__(&limit);
    assert(result.ok && result.value.f0.len == 0 && result.value.f1 == 0);
    __DROP__(&result.value);
    limit.f0 = 3;
    result = __ENTRY__(&limit);
    assert(result.ok && result.value.f0.len == 3 && result.value.f1 == 3);
    for (int64_t i = 0; i < 3; i++) assert(result.value.f0.data[i] == i);
    assert(limit.f0 == 3 && !limit.f1);
    __DROP__(&result.value);
    return 0;
}
"#
    .replace("__LIMIT__", limit)
    .replace("__RESULT__", result)
    .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
    .replace("__DROP__", &format!("ku_drop_{drop}"));
    fs::write(&c, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    assert_counted_array_member(seed, cwd, &c, &exe);
    assert_linked_math_ownership(seed, cwd, &c, &exe);
    assert_int_array_value_abi(seed, cwd, &c, &exe);
}

fn assert_int_array_value_abi(seed: &Path, cwd: &Path, c: &Path, exe: &Path) {
    fs::write(cwd.join("function.txt"), "Carry").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let typed = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&typed);
    let typed = String::from_utf8(typed.stdout).unwrap();
    assert_eq!(
        typed
            .matches("\"c_type\":\"KuArrayInt\",\"passing\":\"value\"")
            .count(),
        2
    );
    assert!(typed.contains("\"returns_result\":false"));
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let symbol = code
        .lines()
        .find_map(|line| line.strip_prefix("KuArrayInt ku_linked_null_"))
        .unwrap();
    let (symbol, _) = symbol.split_once('(').unwrap();
    let host = r#"
int main(void) {
    int64_t *data = malloc(2 * sizeof(*data));
    assert(data);
    data[0] = 8;
    const KuArrayInt result = __ENTRY__((KuArrayInt){1,data,2}, 9);
    assert(result.len == 2 && result.data == data && result.capacity == 2);
    assert(result.data[0] == 8 && result.data[1] == 9);
    free(result.data);
    return 0;
}
"#
    .replace("__ENTRY__", &format!("ku_linked_null_{symbol}"));
    fs::write(c, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(c, exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
}

fn assert_counted_array_member(seed: &Path, cwd: &Path, c: &Path, exe: &Path) {
    fs::write(cwd.join("function.txt"), "CountedMember").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, params) = signature.split_once("(const ").unwrap();
    let (limit, _) = params.split_once('*').unwrap();
    let host = r#"
int main(void) {
    int64_t input[] = {8,7,6};
    __LIMIT__ limit = {{3,input,3},0};
    __RESULT__ result = __ENTRY__(&limit);
    assert(result.ok && result.value.f1 == 3 && result.value.f0.len == 3);
    assert(result.value.f0.data[0] == 0 && result.value.f0.data[1] == 1 && result.value.f0.data[2] == 2);
    assert(limit.f0.data == input && limit.f0.len == 3 && input[0] == 8 && input[2] == 6);
    free(result.value.f0.data);
    return 0;
}
"#
    .replace("__LIMIT__", limit)
    .replace("__RESULT__", result)
    .replace("__ENTRY__", &format!("ku_linked_null_{entry}"));
    fs::write(c, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(c, exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
}

fn assert_linked_math_ownership(seed: &Path, cwd: &Path, c: &Path, exe: &Path) {
    fs::write(cwd.join("function.txt"), "Arithmetic").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, _) = signature.split_once('(').unwrap();
    let host = r#"
static void require_released(void) {
    if (outstanding() || attempts != 1) _Exit(91);
}
int main(int argc, char **argv) {
    (void)test_malloc;
    assert(atexit(require_released) == 0);
    if (argc == 2) {
        if (!strcmp(argv[1], "zero")) (void)__ENTRY__(3, 0, 1);
        else if (!strcmp(argv[1], "div")) (void)__ENTRY__(INT64_MIN, -1, 1);
        else (void)__ENTRY__(INT64_MAX, 1, 2);
        return 90;
    }
    __RESULT__ result = __ENTRY__(-7, 3, 3);
    assert(result.ok && result.value.f0.len == 1 && result.value.f0.data[0] == -7);
    assert(result.value.f1 == -6);
    free(result.value.f0.data);
    return 0;
}
"#
    .replace("__RESULT__", result)
    .replace("__ENTRY__", &format!("ku_linked_null_{entry}"));
    fs::write(c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(c, exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    for (case, diagnostic) in [
        ("zero", "division by zero"),
        ("div", "integer overflow"),
        ("mul", "integer overflow"),
    ] {
        let error = execute(Command::new(exe).arg(case), cwd, 15);
        assert_eq!(error.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&error.stderr).trim(), diagnostic);
    }
}

const COUNTED_BODY_SOURCE: &str = r#"struct Bound { count: int }
struct Entry { text: str index: int }
fn Branch(&bound: Bound, &marker: str): [Entry]! {
    entries: [Entry] = []
    i = 0
    while (i < bound.count) {
        if (i == 1) {
            entry = Entry { text: marker.clone(), index: i }
            entries = entries.push(entry)
        } else {
            entry = Entry { text: "other", index: i }
            entries = entries.push(entry)
        }
        i = i + 1
    }
    return ok(entries)
}
fn Consume(owner: str): null! { return ok(null) }
fn Lose(&bound: Bound, owner: str): null! {
    i = 0
    while (i < bound.count) {
        Consume(owner)?
        i = i + 1
    }
    return ok(null)
}
"#;

const COUNTED_BODY_HOST: &str = r#"
static void *caller_owned;
static void after_failure(void) {
    free(caller_owned);
    if (outstanding() || attempts != 3) _Exit(88);
}
int main(int argc, char **argv) {
    (void)argv;
    __BOUND__ limit = {3};
    uint8_t *text = malloc(3);
    memcpy(text, "A+B", 3);
    KuProjectionString marker = {text,3,3,KU_PROJECTION_STRING_OWNED};
    if (argc == 2) {
        caller_owned = text;
        fail_after = 1;
        assert(atexit(after_failure) == 0);
        (void)__ENTRY__(&limit, &marker);
        return 89;
    }
    __RESULT__ result = __ENTRY__(&limit, &marker);
    assert(result.ok && result.value.len == 3 && outstanding() == 3);
    assert(result.value.data[0].f0.len == 5 && result.value.data[2].f0.len == 5);
    assert(!memcmp(result.value.data[0].f0.ptr, "other", 5));
    assert(!memcmp(result.value.data[2].f0.ptr, "other", 5));
    assert(result.value.data[1].f0.len == 3 && result.value.data[1].f0.ptr != marker.ptr);
    assert(!memcmp(result.value.data[1].f0.ptr, "A+B", 3));
    for (int64_t i = 0; i < 3; i++) assert(result.value.data[i].f1 == i);
    __DROP__(&result.value);
    assert(outstanding() == 1 && limit.f0 == 3 && !memcmp(marker.ptr, "A+B", 3));
    free(text);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_counted_branch_owners(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("counted-branch.ku"), COUNTED_BODY_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "counted-branch.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Branch").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, params) = signature.split_once("(const ").unwrap();
    let (bound, _) = params.split_once('*').unwrap();
    let drop = code
        .lines()
        .filter_map(|line| line.strip_prefix("static void ku_linked_view_"))
        .filter_map(|line| line.split_once('('))
        .find(|(_, params)| params.starts_with("KuArray_"))
        .unwrap()
        .0;
    let host = COUNTED_BODY_HOST
        .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
        .replace("__BOUND__", bound)
        .replace("__RESULT__", result)
        .replace("__DROP__", &format!("ku_linked_view_{drop}"));
    let c = cwd.join("counted-branch.c");
    let exe = cwd.join(format!("counted-branch{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    let failed = execute(Command::new(&exe).arg("oom"), cwd, 15);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&failed.stderr).trim(),
        "out of memory"
    );

    fs::write(cwd.join("function.txt"), "Lose").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_control_move"));
}

const READONLY_BOOL_SOURCE: &str = r#"struct Entry { kind: str }
fn Named(&kind: str): bool { return kind == "Function" }
fn Mark(value: int): null! { return ok(null) }
fn Read(&entry: Entry, inspect: bool, skip: bool): null! {
    if (inspect && !Named(entry.kind)) { Mark(1)? }
    if (skip || Named(entry.kind)) { Mark(2)? }
    return ok(null)
}
fn Own(kind: str): bool { return kind == "Function" }
fn RefuseOwned(kind: str): null! {
    if (Own(kind)) { return ok(null) }
    return ok(null)
}
fn Measure(&kind: str): int { return 1 }
fn RefuseNonBool(&entry: Entry): null! {
    if (Measure(entry.kind)) { return ok(null) }
    return ok(null)
}
"#;

const READONLY_BOOL_HOST: &str = r#"
static const KuProjectionString *expected;
static int trail;
bool __NAMED__(const KuProjectionString *kind) {
    assert(kind == expected && kind->ptr && kind->len > 0);
    trail = trail * 10 + 3;
    return kind->ptr[0] == 'F';
}
KuBootstrapResultNull __MARK__(int64_t value) {
    assert(value == 1 || value == 2);
    trail = trail * 10 + (int)value;
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    uint8_t other[] = "Other", fn[] = "Function";
    __RECORD__ entry = {{other,5,0,KU_PROJECTION_STRING_STATIC}};
    expected = &entry.f0;
    assert(__ENTRY__(&entry, false, true).ok && trail == 2);
    trail = 0;
    assert(__ENTRY__(&entry, true, false).ok && trail == 313);
    trail = 0;
    entry.f0.ptr = fn;
    entry.f0.len = 8;
    assert(__ENTRY__(&entry, true, false).ok && trail == 332);
    assert(entry.f0.ptr == fn && entry.f0.len == 8);
    return 0;
}
"#;

pub(super) fn assert_lazy_readonly_bool_call(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("readonly-bool.ku"), READONLY_BOOL_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "readonly-bool.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Read").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (entry, params) = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
        .unwrap()
        .split_once("(const ")
        .unwrap();
    let (record, _) = params.split_once('*').unwrap();
    let dependency = |prefix| {
        code.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap()
            .split_once('(')
            .unwrap()
            .0
    };
    let host = READONLY_BOOL_HOST
        .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
        .replace("__RECORD__", record)
        .replace("__NAMED__", dependency("extern bool "))
        .replace("__MARK__", dependency("extern KuBootstrapResultNull "));
    let c = cwd.join("readonly-bool.c");
    let exe = cwd.join(format!("readonly-bool{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));

    for (name, reason) in [
        ("RefuseOwned", "matching str View or scalar Copy argument"),
        ("RefuseNonBool", "requires a plain bool result"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.contains("linked_readonly_call") && stderr.contains(reason));
    }
}

const STR_FIELD_SOURCE: &str = r#"struct Payload { text: str }
struct Envelope { payload: Payload }
fn Clone(&entry: Envelope): str! {
    copy = entry.payload.text.clone()
    return ok(copy)
}
fn Take(payload: Payload): null! { return ok(null) }
fn RefuseMovedField(entry: Envelope): str! {
    Take(entry.payload)?
    copy = entry.payload.text.clone()
    return ok(copy)
}
"#;

const STR_FIELD_HOST: &str = r#"
static const uint8_t original[] = {0xe4,0xb8,0xad,0xf0,0x9f,0x99,0x82,0};
static uint8_t *caller_owned;
static void check_failure(void) {
    if (outstanding() != 1 || attempts != 2 || memcmp(caller_owned, original, sizeof original)) _Exit(88);
    free(caller_owned);
    if (outstanding()) _Exit(89);
}
int main(int argc, char **argv) {
    (void)argv;
    (void)test_realloc;
    caller_owned = malloc(sizeof original);
    memcpy(caller_owned, original, sizeof original);
    __ENVELOPE__ envelope = {{{caller_owned,sizeof original,sizeof original,KU_PROJECTION_STRING_OWNED}}};
    if (argc == 2) {
        fail_after = 0;
        assert(atexit(check_failure) == 0);
        (void)__CLONE__(&envelope);
        return 90;
    }
    __RESULT__ result = __CLONE__(&envelope);
    assert(result.ok && outstanding() == 2 && attempts == 2);
    assert(result.value.len == sizeof original && result.value.ptr != caller_owned);
    assert(envelope.f0.f0.ptr == caller_owned && !memcmp(caller_owned, original, sizeof original));
    free(caller_owned);
    assert(!memcmp(result.value.ptr, original, sizeof original));
    ku_projection_string_drop(&result.value);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_owned_string_field_clone(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("field-string.ku"), STR_FIELD_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "field-string.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Clone").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, params) = signature.split_once("(const ").unwrap();
    let (envelope, _) = params.split_once('*').unwrap();
    let host = STR_FIELD_HOST
        .replace("__CLONE__", &format!("ku_linked_null_{entry}"))
        .replace("__RESULT__", result)
        .replace("__ENVELOPE__", envelope);
    let c = cwd.join("field-string.c");
    let exe = cwd.join(format!("field-string{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    let oom = execute(Command::new(&exe).arg("oom"), cwd, 15);
    assert_eq!(oom.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&oom.stderr).trim(), "out of memory");

    fs::write(cwd.join("function.txt"), "RefuseMovedField").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_moved_member"));
}

const OWNED_BOUND_SOURCE: &str = r#"struct Snapshot { values: [int] count: int }
fn Stable(): Snapshot! {
    bound: [int] = [4, 0, 9]
    sampled: [int] = []
    i = 0
    while (i < bound.len()) {
        sampled = sampled.push(bound[i])
        i = i + 1
    }
    return ok(Snapshot { values: sampled, count: i })
}
fn Grow(): null! {
    bound: [int] = [1]
    i = 0
    while (i < bound.len()) {
        if (i >= 0) { bound = bound.push(0) }
        i = i + 1
    }
    return ok(null)
}
"#;

const OWNED_BOUND_HOST: &str = r#"
int main(void) {
    (void)test_malloc;
    __RESULT__ result = __STABLE__();
    assert(result.ok && result.value.f0.len == 3 && result.value.f1 == 3);
    assert(result.value.f0.data[0] == 4 && result.value.f0.data[1] == 0 && result.value.f0.data[2] == 9);
    assert(outstanding() == 1 && attempts == 2);
    __DROP__(&result.value);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_stable_owned_count_bound(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("owned-bound.ku"), OWNED_BOUND_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "owned-bound.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Stable").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, _) = signature.split_once('(').unwrap();
    let drop = code
        .lines()
        .find_map(|line| line.strip_prefix("static void ku_drop_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = OWNED_BOUND_HOST
        .replace("__STABLE__", &format!("ku_linked_null_{entry}"))
        .replace("__RESULT__", result)
        .replace("__DROP__", &format!("ku_drop_{drop}"));
    let c = cwd.join("owned-bound.c");
    let exe = cwd.join(format!("owned-bound{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));

    fs::write(cwd.join("function.txt"), "Grow").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_loop_bound_write"));
}

const CONDITIONAL_MEMBER_SOURCE: &str = r#"struct Pair { left: str right: str }
fn Consume(text: str): null! { return ok(null) }
fn Keep(): null! { return ok(null) }
fn Read(&text: str): null! { return ok(null) }
fn Branch(owner: Pair, transfer: bool): null! {
    if (transfer) { Consume(owner.left)? }
    else { Keep()? }
    Read(owner.right)?
    return ok(null)
}
fn BranchWithoutElse(owner: Pair, transfer: bool): null! {
    if (transfer) { Consume(owner.left)? }
    Read(owner.right)?
    return ok(null)
}
fn RefuseWithoutElse(owner: Pair, transfer: bool): null! {
    if (transfer) { Consume(owner.left)? }
    Read(owner.left)?
    return ok(null)
}
fn Refuse(owner: Pair, transfer: bool): null! {
    if (transfer) { Consume(owner.left)? }
    else { Keep()? }
    Read(owner.left)?
    return ok(null)
}
"#;

const CONDITIONAL_MEMBER_HOST: &str = r#"
static uint8_t *left_owner, *right_owner;
static int route, read_sibling;
static bool fail_call;
KuBootstrapResultNull __CONSUME__(KuProjectionString text) {
    assert(!route && outstanding() == 2 && text.ptr == left_owner && text.ptr[0] == 'L');
    ku_projection_string_drop(&text);
    route = 1;
    if (!fail_call) return (KuBootstrapResultNull){.ok = true};
    return (KuBootstrapResultNull){.error.code = {
        (uint8_t *)"injected",8,0,KU_PROJECTION_STRING_STATIC
    }};
}
KuBootstrapResultNull __KEEP__(void) {
    assert(!route && outstanding() == 2);
    route = 2;
    return (KuBootstrapResultNull){.ok = true};
}
KuBootstrapResultNull __READ__(const KuProjectionString *text) {
    assert(text->ptr == right_owner && text->ptr[0] == 'R');
    assert(outstanding() == (route == 1 ? 1 : 2));
    read_sibling++;
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    (void)test_realloc;
    for (int scenario = 0; scenario < 3; scenario++) {
        route = 0;
        read_sibling = 0;
        fail_call = scenario == 2;
        left_owner = malloc(1);
        right_owner = malloc(1);
        *left_owner = 'L';
        *right_owner = 'R';
        __PAIR__ pair = {
            {left_owner,1,1,KU_PROJECTION_STRING_OWNED},
            {right_owner,1,1,KU_PROJECTION_STRING_OWNED}
        };
        KuBootstrapResultNull result = __ENTRY__(pair, scenario != 0);
        assert(!outstanding() && route == (scenario == 0 ? __UNTAKEN_ROUTE__ : 1));
        assert(result.ok == !fail_call && read_sibling == (fail_call ? 0 : 1));
        if (fail_call) assert(result.error.code.len == 8 && !memcmp(result.error.code.ptr, "injected", 8));
    }
    return 0;
}
"#;

pub(super) fn assert_conditional_partial_members(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("conditional-member.ku"), CONDITIONAL_MEMBER_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "conditional-member.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    for (branch, explicit_else) in [("Branch", true), ("BranchWithoutElse", false)] {
        fs::write(cwd.join("function.txt"), branch).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let code = String::from_utf8(emitted.stdout).unwrap();
        let signature = code
            .lines()
            .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
            .unwrap();
        let (entry, formal) = signature.split_once('(').unwrap();
        let (pair, _) = formal.split_once(' ').unwrap();
        let dependency = |expected| {
            code.lines()
                .filter_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
                .filter_map(|line| line.split_once('('))
                .find(|(_, args)| *args == expected)
                .unwrap()
                .0
        };
        let host = CONDITIONAL_MEMBER_HOST
            .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
            .replace("__PAIR__", pair)
            .replace("__CONSUME__", dependency("KuProjectionString);"))
            .replace(
                "__KEEP__",
                if explicit_else {
                    dependency("void);")
                } else {
                    "unused_keep"
                },
            )
            .replace("__UNTAKEN_ROUTE__", if explicit_else { "2" } else { "0" })
            .replace("__READ__", dependency("const KuProjectionString*);"));
        let c = cwd.join("conditional-member.c");
        let exe = cwd.join(format!(
            "conditional-member{}",
            std::env::consts::EXE_SUFFIX
        ));
        fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
        compile_projection(&c, &exe, cwd);
        assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    }

    for negative in ["Refuse", "RefuseWithoutElse"] {
        fs::write(cwd.join("function.txt"), negative).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_moved_member"));
    }
}

const STRING_PREFIX_SUFFIX_SOURCE: &str = r#"fn Mark(kind: int): null! { return ok(null) }
fn Probe(&value: str, &pattern: str): null! {
    if (value.starts_with(pattern)) { Mark(1)? }
    if (value.ends_with(pattern)) { Mark(2)? }
    return ok(null)
}
"#;

const STRING_PREFIX_SUFFIX_HOST: &str = r#"
static int observed;
KuBootstrapResultNull __MARK__(int64_t kind) {
    observed = observed * 10 + (int)kind;
    return (KuBootstrapResultNull){.ok = true};
}
static void compare(const KuProjectionString *value, const KuProjectionString *pattern, int expected) {
    KuProjectionString owner = *value, needle = *pattern;
    observed = 0;
    assert(__ENTRY__(value, pattern).ok && observed == expected);
    assert(value->ptr == owner.ptr && value->len == owner.len && value->storage == owner.storage);
    assert(pattern->ptr == needle.ptr && pattern->len == needle.len && pattern->storage == needle.storage);
    assert(attempts == 1 && outstanding() == 1);
}
int main(void) {
    (void)test_realloc;
    const uint8_t input[] = {0xe4,0xb8,0xad,0,0xf0,0x9f,0x99,0x82};
    uint8_t *data = malloc(sizeof input);
    memcpy(data, input, sizeof input);
    KuProjectionString value = {data,sizeof input,sizeof input,KU_PROJECTION_STRING_OWNED};
    KuProjectionString empty = {0};
    KuProjectionString prefix = {data,4,0,KU_PROJECTION_STRING_STATIC};
    KuProjectionString suffix = {data + 3,5,0,KU_PROJECTION_STRING_STATIC};
    uint8_t wrong_data[] = {0xe4,0xb8,0xad,1};
    KuProjectionString wrong = {wrong_data,sizeof wrong_data,0,KU_PROJECTION_STRING_STATIC};
    uint8_t longer_data[] = {0xe4,0xb8,0xad,0,0xf0,0x9f,0x99,0x82,'!'};
    KuProjectionString longer = {longer_data,sizeof longer_data,0,KU_PROJECTION_STRING_STATIC};
    compare(&value, &empty, 12);
    compare(&value, &prefix, 1);
    compare(&value, &suffix, 2);
    compare(&value, &value, 12);
    compare(&value, &wrong, 0);
    compare(&value, &longer, 0);
    compare(&empty, &prefix, 0);
    compare(&empty, &empty, 12);
    assert(!memcmp(data, input, sizeof input));
    free(data);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_string_prefix_suffix(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("prefix-suffix.ku"), STRING_PREFIX_SUFFIX_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "prefix-suffix.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Probe").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let entry = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let mark = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = STRING_PREFIX_SUFFIX_HOST
        .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
        .replace("__MARK__", mark);
    let c = cwd.join("prefix-suffix.c");
    let exe = cwd.join(format!("prefix-suffix{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
}

const PLAIN_RECORD_SOURCE: &str = r#"struct Input { line: int text: str }
struct Position { line: int text: str }
struct Unrelated { rows: [Position] }
fn Extract(&source: Input): Position {
    return Position { line: source.line, text: source.text.clone() }
}
fn Refuse(&source: Input): Input { return source }
"#;

const PLAIN_RECORD_HOST: &str = r#"
int main(void) {
    (void)test_realloc;
    uint8_t *data = malloc(4);
    memcpy(data, "text", 4);
    __INPUT__ input = {INT64_MIN, {data,4,4,KU_PROJECTION_STRING_OWNED}};
    __RESULT__ result = __ENTRY__(&input);
    assert(result.f0 == INT64_MIN && result.f1.len == 4 && result.f1.ptr != data);
    assert(input.f1.ptr == data && outstanding() == 2 && attempts == 2);
    free(data);
    assert(!memcmp(result.f1.ptr, "text", 4) && outstanding() == 1);
    ku_projection_string_drop(&result.f1);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_plain_named_record_result(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("plain-record.ku"), PLAIN_RECORD_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "plain-record.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Extract").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, params) = signature.split_once("(const ").unwrap();
    let (input, _) = params.split_once('*').unwrap();
    assert!(result.starts_with("KuStruct_") && input.starts_with("KuStruct_"));
    let host = PLAIN_RECORD_HOST
        .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
        .replace("__RESULT__", result)
        .replace("__INPUT__", input);
    let c = cwd.join("plain-record.c");
    let exe = cwd.join(format!("plain-record{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));

    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_value_return"));
}
