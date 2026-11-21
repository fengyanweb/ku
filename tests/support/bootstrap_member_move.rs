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
fn Take(first: str): str { return first }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("member-move-source.ku"),
        r#"import { Packet, Mix, Checked, Observe, Take } from "./member-move-providers.ku"
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
fn Concat(packet: Packet): str! { return ok(packet.pair.first + Take(packet.pair.first)) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "member-move-source.ku\nmember-move-providers.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "MoveMembers").unwrap();
    let emit = |mode: &str| {
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
    assert_snapshot_before_consuming_rhs(cwd, &emit);
    assert_owned_array_member(cwd, &emit);
    assert_owned_int_array_member(cwd, &emit);
}

fn assert_owned_int_array_member(
    cwd: &Path,
    emit: &impl Fn(&str) -> super::bounded_process::BoundedOutput,
) {
    fs::write(
        cwd.join("int-array-member.ku"),
        r#"struct IntRecord { values: [int] tag: str }
struct Outer { inner: IntRecord tag: str }
fn TransferInts(outer: Outer): IntRecord! {
    inner = outer.inner
    integers = inner.values
    return ok(IntRecord { values: integers, tag: "done" })
}
fn RefuseBorrow(&outer: Outer): IntRecord! {
    integers = outer.inner.values
    return ok(IntRecord { values: integers, tag: "no" })
}
fn RefuseRepeatedMove(outer: Outer): IntRecord! {
    integers = outer.inner.values
    return ok(IntRecord { values: outer.inner.values, tag: "no" })
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "int-array-member.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "TransferInts").unwrap();
    let emitted = emit("linked-null");
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let signature = code
        .lines()
        .find(|line| line.contains(" ku_linked_null_"))
        .expect("entry declaration");
    let (result, definition) = signature.split_once(' ').unwrap();
    let (entry, arguments) = definition.split_once('(').unwrap();
    let outer = arguments.split_whitespace().next().unwrap();
    let host = r#"
int main(void) {
    (void)test_realloc;
    int64_t *integers = malloc(2 * sizeof(*integers));
    integers[0] = INT64_MIN;
    integers[1] = 42;
    __OUTER__ packet = {0};
    packet.f0.f0 = (KuArrayInt){2, integers, 2};
    packet.f0.f1 = (KuProjectionString){malloc(1), 1, 1, KU_PROJECTION_STRING_OWNED};
    packet.f1 = (KuProjectionString){malloc(1), 1, 1, KU_PROJECTION_STRING_OWNED};
    __RESULT__ result = __ENTRY__(packet);
    assert(result.ok && result.value.f0.data == integers && result.value.f0.len == 2);
    assert(result.value.f0.data[0] == INT64_MIN && result.value.f0.data[1] == 42);
    assert(outstanding() == 1 && attempts == 3);
    free(result.value.f0.data);
    assert(outstanding() == 0);
    return 0;
}
"#
    .replace("__OUTER__", outer)
    .replace("__RESULT__", result)
    .replace("__ENTRY__", entry);
    let path = cwd.join("int-array-member.c");
    let app = cwd.join(format!("int-array-member{}", std::env::consts::EXE_SUFFIX));
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&path, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    for (function, diagnostic) in [
        ("RefuseBorrow", "linked_view_escape"),
        ("RefuseRepeatedMove", "linked_moved_member"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = emit("linked-null");
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}

fn assert_snapshot_before_consuming_rhs(
    cwd: &Path,
    emit: &impl Fn(&str) -> super::bounded_process::BoundedOutput,
) {
    fs::write(cwd.join("function.txt"), "Concat").unwrap();
    let typed = emit("linked-null-plan");
    assert_ok(&typed);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(typed.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected a concat plan")
    };
    let Value::Object(root) = *value else {
        panic!("expected plan object")
    };
    let Value::Object(entry) = &root["export"] else {
        panic!("expected export")
    };
    let Value::Object(signature) = &entry["signature"] else {
        panic!("expected signature")
    };
    let Value::Array(parameters) = &signature["parameters"] else {
        panic!("expected parameters")
    };
    let Value::Object(parameter) = &parameters[0] else {
        panic!("expected parameter type")
    };
    let Value::String(packet) = &parameter["c_type"] else {
        panic!("expected C parameter type")
    };
    let Value::String(symbol) = &entry["symbol"] else {
        panic!("expected export symbol")
    };
    let Value::Array(dependencies) = &root["dependencies"] else {
        panic!("expected dependencies")
    };
    let [Value::Object(provider)] = dependencies.as_slice() else {
        panic!("expected one owned provider")
    };
    let Value::String(take) = &provider["symbol"] else {
        panic!("expected provider symbol")
    };
    let driver = r#"
static uint8_t *original;
static unsigned calls, expected_calls;
static void check_cleanup(void) {
    if (outstanding() || calls != expected_calls) _Exit(90);
}
KuProjectionString __TAKE__(KuProjectionString value) {
    assert(value.ptr == original && value.len == 5);
    ++calls;
    return value;
}
int main(int argc, char **argv) {
    (void)test_realloc;
    __PACKET__ packet = {0};
    original = malloc(5);
    memcpy(original, "first", 5);
    packet.f0.f0 = (KuProjectionString){original,5,5,KU_PROJECTION_STRING_OWNED};
    assert(atexit(check_cleanup) == 0);
    expected_calls = 1;
    if (argc > 1) {
        fail_after = argv[1][0] == '0' ? 0 : 1;
        expected_calls = (unsigned)fail_after;
    }
    KuBootstrapResult_KuProjectionString result = __ENTRY__(packet);
    assert(argc == 1 && result.ok && result.value.len == 10);
    assert(memcmp(result.value.ptr, "firstfirst", result.value.len) == 0);
    ku_projection_string_drop(&result.value);
    return 0;
}
"#
    .replace("__PACKET__", packet)
    .replace("__ENTRY__", symbol)
    .replace("__TAKE__", take);
    let c = emit("linked-null");
    assert_ok(&c);
    let path = cwd.join("concat-operand.c");
    let app = cwd.join(format!("concat-operand{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &path,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{}{driver}",
            String::from_utf8(c.stdout).unwrap()
        ),
    )
    .unwrap();
    compile_projection(&path, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    for phase in ["0", "1"] {
        let failed = execute(Command::new(&app).arg(phase), cwd, 15);
        assert_eq!(failed.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&failed.stderr).contains("out of memory"));
    }
}

fn assert_owned_array_member(
    cwd: &Path,
    emit: &impl Fn(&str) -> super::bounded_process::BoundedOutput,
) {
    fs::write(
        cwd.join("array-member-source.ku"),
        r#"struct Row { name: str }
struct Packet { rows: [Row] other: str }
fn Take(packet: Packet): [Row]! { return ok(packet.rows) }
fn TakePlain(packet: Packet): [Row] { return packet.rows }
fn TakeThroughPlain(packet: Packet): [Row]! { return ok(TakePlain(packet)) }
fn Repack(packet: Packet): Packet! { return ok(Packet { rows: packet.rows, other: "kept" }) }
fn Refuse(&packet: Packet): [Row]! { return ok(packet.rows) }
fn RefusePlain(&packet: Packet): [Row] { return packet.rows }
fn RefuseBorrowInField(&packet: Packet): Packet! {
    return ok(Packet { rows: packet.rows, other: "invalid" })
}
fn RejectUnchecked(): null! { return ok(null) }
fn RefuseEscapingError(packet: Packet): [Row] {
    RejectUnchecked()?
    return packet.rows
}
fn Decide(&value: str): bool {
    if (value == "ku") { return true }
    return false
}
fn Duplicate(packet: Packet): [Row]! {
    rows = packet.rows
    return ok(packet.rows)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "array-member-source.ku\n").unwrap();
    assert_array_transfer(cwd, emit, "Take", true, "");
    assert_array_transfer(cwd, emit, "TakePlain", false, "");
    assert_array_transfer(cwd, emit, "Repack", true, ".f0");
    assert_direct_bool(cwd, emit);
    fs::write(cwd.join("function.txt"), "TakeThroughPlain").unwrap();
    let forward = emit("linked-null-plan");
    assert_ok(&forward);
    let plan = String::from_utf8(forward.stdout).unwrap();
    assert!(plan.contains("\"source_name\":\"TakePlain\""));
    assert!(plan.contains("\"returns_result\":false"));
    for (function, diagnostic) in [
        ("Refuse", "linked_result_return"),
        ("RefusePlain", "linked_value_return"),
        ("RefuseBorrowInField", "linked_literal_field_type"),
        ("RefuseEscapingError", "linked_value_failure"),
        ("Duplicate", "linked_moved_member"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = emit("linked-null");
        assert_eq!(rejected.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}

fn assert_direct_bool(cwd: &Path, emit: &impl Fn(&str) -> super::bounded_process::BoundedOutput) {
    fs::write(cwd.join("function.txt"), "Decide").unwrap();
    let plan = emit("linked-null-plan");
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains("\"c_type\":\"bool\""));
    assert!(plan.contains("\"returns_result\":false"));
    let c = emit("linked-null");
    assert_ok(&c);
    let code = String::from_utf8(c.stdout).unwrap();
    let symbol = code
        .lines()
        .find_map(|line| line.strip_prefix("bool ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
int main(void) {
    KuProjectionString match = {(uint8_t*)"ku",2,2,0};
    KuProjectionString miss = {(uint8_t*)"no",2,2,0};
    assert(__ENTRY__(&match) && !__ENTRY__(&miss));
    assert(match.ptr[1] == 'u' && miss.ptr[1] == 'o');
    return 0;
}
"#
    .replace("__ENTRY__", &format!("ku_linked_null_{symbol}"));
    let path = cwd.join("direct-bool.c");
    let app = cwd.join(format!("direct-bool{}", std::env::consts::EXE_SUFFIX));
    fs::write(&path, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(&path, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
}

fn assert_array_transfer(
    cwd: &Path,
    emit: &impl Fn(&str) -> super::bounded_process::BoundedOutput,
    function: &str,
    checked: bool,
    field: &str,
) {
    fs::write(cwd.join("function.txt"), function).unwrap();
    let typed = emit("linked-null-plan");
    assert_ok(&typed);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(typed.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected a move plan")
    };
    let Value::Object(root) = *value else {
        panic!("expected plan object")
    };
    let Value::Object(entry) = &root["export"] else {
        panic!("expected export")
    };
    let Value::Object(signature) = &entry["signature"] else {
        panic!("expected signature")
    };
    assert_eq!(signature["returns_result"], Value::Bool(checked));
    let Value::Array(parameters) = &signature["parameters"] else {
        panic!("expected parameters")
    };
    let Value::Object(parameter) = &parameters[0] else {
        panic!("expected parameter type")
    };
    let Value::Object(result) = &signature["result"] else {
        panic!("expected result type")
    };
    let Value::String(packet) = &parameter["c_type"] else {
        panic!("expected C parameter type")
    };
    let Value::String(result_type) = &result["c_type"] else {
        panic!("expected C result type")
    };
    let Value::String(symbol) = &entry["symbol"] else {
        panic!("expected export symbol")
    };
    let invocation = if checked {
        format!("{result_type} result = {symbol}(packet);\n    assert(result.ok);\n#define returned result.value{field}")
    } else {
        format!("{result_type} returned = {symbol}(packet);")
    };
    let driver = r#"
int main(void) {
    (void)test_realloc;
    __PACKET__ packet = {0};
    packet.f0.data = malloc(sizeof(*packet.f0.data));
    uint8_t *bytes = malloc(4);
    memcpy(bytes, "name", 4);
    packet.f0.data[0].f0 = (KuProjectionString){bytes,4,4,KU_PROJECTION_STRING_OWNED};
    packet.f0.len = packet.f0.capacity = 1;
    packet.f1 = (KuProjectionString){malloc(1),1,1,KU_PROJECTION_STRING_OWNED};
    const void *array = packet.f0.data;
    __INVOKE__
    assert(returned.data == array && returned.len == 1);
    assert(returned.data[0].f0.ptr == bytes && outstanding() == 2);
    ku_projection_string_drop(&returned.data[0].f0);
    free(returned.data);
    assert(outstanding() == 0);
    return 0;
}
"#
    .replace("__PACKET__", packet)
    .replace("__INVOKE__", &invocation);
    let c = emit("linked-null");
    assert_ok(&c);
    let path = cwd.join("array-member-move.c");
    let app = cwd.join(format!("array-member-move{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &path,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{}{driver}",
            String::from_utf8(c.stdout).unwrap()
        ),
    )
    .unwrap();
    compile_projection(&path, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
}
