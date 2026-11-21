use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

pub(super) fn assert_named_array_parameter_move(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("array-parameter.ku"),
        r#"struct Item { text: str }
fn Transfer(input: [Item]): [Item]! {
    local: [Item] = input
    return ok(local)
}
fn Consumed(input: [Item]): int! {
    local: [Item] = input
    return ok(input.len())
}
fn Borrowed(&input: [Item]): [Item]! {
    local: [Item] = input
    return ok(local)
}
fn Snapshot(input: [Item]): [Item]! {
    copy = input.clone()
    if (copy.len() != input.len()) {
        fail { domain: "clone", code: "length", message: "array length changed" }
    }
    return ok(copy)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "array-parameter.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Transfer").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let entry = code
        .lines()
        .find(|line| line.starts_with("KuBootstrapResult_KuArray_"))
        .unwrap();
    let (signature, parameter) = entry.split_once('(').unwrap();
    let (result, symbol) = signature.split_once(' ').unwrap();
    let array = parameter.strip_suffix(" a0) {").unwrap();
    let element = array.strip_prefix("KuArray_").unwrap();
    let host = r#"
#include <assert.h>
int main(int argc, char **argv) {
    (void)argv;
    if (argc > 1) { (void)ENTRY((ARRAY){2, NULL, 1}); return 2; }
    ELEMENT *item = calloc(1, sizeof(*item));
    uint8_t *text = malloc(1);
    assert(item && text);
    *text = 42;
    item->f0 = (KuProjectionString){text, 1, 1, KU_PROJECTION_STRING_OWNED};
    RESULT output = ENTRY((ARRAY){1, item, 1});
    assert(output.ok && output.value.len == 1 && output.value.data == item);
    assert(output.value.data[0].f0.ptr == text && *text == 42);
    DROP(&output.value);
    return 0;
}
"#
    .replace("ENTRY", symbol)
    .replace("RESULT", result)
    .replace("ARRAY", array)
    .replace("ELEMENT", element)
    .replace("DROP", &format!("ku_linked_view_{element}_drop"));
    let source = cwd.join("array-parameter.c");
    let program = cwd.join(format!("array-parameter{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{code}{host}")).unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 10));
    let malformed = execute(Command::new(program).arg("malformed"), cwd, 10);
    assert_eq!(malformed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&malformed.stderr).trim(),
        "invalid struct array ownership"
    );
    for (name, error) in [
        ("Consumed", "unavailable_linked_binding"),
        ("Borrowed", "expected a matching named array owner"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(error));
    }

    fs::write(cwd.join("function.txt"), "Snapshot").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (snapshot, _) = code
        .lines()
        .find(|line| line.starts_with("KuBootstrapResult_KuArray_"))
        .and_then(|line| line.strip_prefix(result))
        .and_then(|line| line.trim_start().split_once('('))
        .unwrap();
    let host = r#"
static void after_clone_failure(void) {
    assert(attempts == 6 && !outstanding());
}
int main(int argc, char **argv) {
    (void)argv;
    (void)test_realloc;
    ELEMENT *items = malloc(2 * sizeof(*items));
    uintptr_t former[2];
    for (size_t i = 0; i < 2; ++i) {
        uint8_t *text = malloc(1);
        *text = (uint8_t)(42 + i);
        former[i] = (uintptr_t)text;
        items[i].f0 = (KuProjectionString){text, 1, 1, KU_PROJECTION_STRING_OWNED};
    }
    uintptr_t former_data = (uintptr_t)items;
    if (argc > 1) {
        assert(atexit(after_clone_failure) == 0);
        fail_after = 2;
    }
    RESULT output = SNAPSHOT((ARRAY){2, items, 2});
    assert(output.ok && output.value.len == 2 && outstanding() == 3);
    assert((uintptr_t)output.value.data != former_data);
    for (size_t i = 0; i < 2; ++i) {
        assert((uintptr_t)output.value.data[i].f0.ptr != former[i]);
        assert(*output.value.data[i].f0.ptr == 42 + i);
    }
    DROP(&output.value);
    assert(!outstanding());
    return 0;
}
"#
    .replace("SNAPSHOT", snapshot)
    .replace("RESULT", result)
    .replace("ARRAY", array)
    .replace("ELEMENT", element)
    .replace("DROP", &format!("ku_linked_view_{element}_drop"));
    let source = cwd.join("array-snapshot.c");
    let program = cwd.join(format!("array-snapshot{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 10));
    let failed = execute(Command::new(program).arg("fail-second-element"), cwd, 10);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(failed.stderr).unwrap().trim(),
        "out of memory"
    );
}

pub(super) fn assert_checked_callback_forwarding(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("callback-forward.ku"),
        r#"struct Payload { values: [int] }
struct Envelope { payload: Payload }
struct Other { values: [int] }
struct Reply { text: str }

fn Take(callback: fn(int, int): Payload!): Envelope! {
    values: [int] = []
    payload = Payload { values: values }
    return ok(Envelope { payload: payload })
}
fn Forward(callback: fn(int, int): Payload!): Envelope! { return Take(callback) }
fn ForwardCloned(callback: fn(int, int): Payload!): Envelope! { return Take(callback.clone()) }
fn Wrong(callback: fn(int, int): Other!): Envelope! { return Take(callback) }
fn Alias(callback: fn(int, int): Payload!): Envelope! {
    alias = callback
    return Take(alias)
}
fn StoreClone(callback: fn(int, int): Payload!): Envelope! {
    alias = callback.clone()
    return Take(alias)
}
fn First(): int { return 3 }
fn Second(): int { return 7 }
fn Invoke(callback: fn(int, int): Reply!, retained: str): Reply! {
    reply = callback(First(), Second())?
    return ok(reply)
}
fn WrongArity(callback: fn(int, int): Reply!): Reply! {
    reply = callback(0)?
    return ok(reply)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "callback-forward.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Forward").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let document = String::from_utf8(emitted.stdout).unwrap();
    assert_eq!(
        document
            .matches("\"kind\":\"IntPairResultCallback\"")
            .count(),
        2
    );
    assert_eq!(document.matches("\"source_name\":\"Payload\"").count(), 2);
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let forwarded = String::from_utf8(emitted.stdout).unwrap();
    assert!(forwarded.contains("(*entry)(void*, int64_t, int64_t)"));
    fs::write(cwd.join("function.txt"), "ForwardCloned").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let cloned = String::from_utf8(emitted.stdout).unwrap();
    let symbol = |code: &str| {
        code.lines()
            .find(|line| line.starts_with("KuBootstrapResult_KuStruct_"))
            .and_then(|line| line.split_once('('))
            .and_then(|(head, _)| head.split_once(' '))
            .map(|(_, name)| name.to_owned())
            .expect("generated forwarding entry")
    };
    assert_eq!(
        cloned.replace(&symbol(&cloned), &symbol(&forwarded)),
        forwarded
    );
    for (name, code) in [
        ("Wrong", "linked_callback_forward"),
        ("Alias", "linked_callback_escape"),
        ("StoreClone", "unsupported_linked_clone"),
        ("WrongArity", "linked_callback_call"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(code));
    }

    fs::write(cwd.join("function.txt"), "Invoke").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let document = String::from_utf8(emitted.stdout).unwrap();
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(document)], Span::default()).unwrap()
    else {
        panic!("expected the callback consumer link plan");
    };
    let Value::Object(plan) = *value else {
        panic!("expected a link plan object");
    };
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected the argument dependencies");
    };
    let dependency_symbol = |name: &str| {
        dependencies
            .iter()
            .find_map(|value| match value {
                Value::Object(fields)
                    if matches!(&fields["source_name"], Value::String(source) if source.as_str() == name) =>
                {
                    match &fields["symbol"] {
                        Value::String(symbol) => Some(symbol.as_str()),
                        _ => None,
                    }
                }
                _ => None,
            })
            .expect("known callback argument dependency")
    };

    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (head, parameters) = code
        .lines()
        .find(|line| line.starts_with("KuBootstrapResult_KuStruct_"))
        .and_then(|line| line.split_once('('))
        .unwrap();
    let (result, entry) = head.split_once(' ').unwrap();
    let (callback, _) = parameters.split_once(" a0,").unwrap();
    let host = r#"
static unsigned order;
typedef struct { unsigned calls; bool fail; uint8_t *message; } Probe;
int64_t FIRST(void) { assert(order == 0); order = 1; return 3; }
int64_t SECOND(void) { assert(order == 1); order = 2; return 7; }
static KuProjectionString owner(uint8_t byte) {
    uint8_t *data = malloc(1);
    assert(data);
    *data = byte;
    return (KuProjectionString){data, 1, 1, KU_PROJECTION_STRING_OWNED};
}
static RESULT dynamic(void *environment, int64_t left, int64_t right) {
    Probe *probe = environment;
    assert(order == 2 && left == 3 && right == 7);
    order = 3;
    ++probe->calls;
    if (probe->fail) {
        RESULT value = {.error={owner('d'), owner('e'), owner('m')}};
        probe->message = value.error.message.ptr;
        return value;
    }
    return (RESULT){.ok=true, .value={owner('r')}};
}
int main(void) {
    (void)test_realloc;
    Probe probe = {0};
    CALLBACK callback = {dynamic, &probe};
    RESULT result = ENTRY(callback, owner('k'));
    assert(result.ok && order == 3 && probe.calls == 1 && outstanding() == 1);
    assert(*result.value.f0.ptr == 'r');
    ku_projection_string_drop(&result.value.f0);
    assert(!outstanding());

    order = 0;
    probe.fail = true;
    result = ENTRY(callback, owner('k'));
    assert(!result.ok && order == 3 && probe.calls == 2 && outstanding() == 3);
    assert(result.error.message.ptr == probe.message && *result.error.message.ptr == 'm');
    ku_projection_string_drop(&result.error.domain);
    ku_projection_string_drop(&result.error.code);
    ku_projection_string_drop(&result.error.message);
    assert(!outstanding());

    order = 0;
    result = ENTRY((CALLBACK){0}, owner('k'));
    assert(!result.ok && order == 2 && probe.calls == 2 && !outstanding());
    assert(result.error.code.len == 16 && !memcmp(result.error.code.ptr, "invalid_callback", 16));
    assert(result.error.domain.storage == KU_PROJECTION_STRING_STATIC);
    return 0;
}
"#
    .replace("FIRST", dependency_symbol("First"))
    .replace("SECOND", dependency_symbol("Second"))
    .replace("RESULT", result)
    .replace("CALLBACK", callback)
    .replace("ENTRY", entry);
    let source = cwd.join("callback-invoke.c");
    let program = cwd.join(format!("callback-invoke{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 10));
}

pub(super) fn assert_local_callback_thunk(seed: &Path, python: &str, cwd: &Path) {
    fs::write(
        cwd.join("callback-local.ku"),
        r#"struct Sample { amount: int, label: str }
struct Reply { total: int, first: int, second: int }

fn Read(&source: [Sample], index: int): int! {
    if (index < 0) { fail { domain: "test", code: "index", message: "negative index" } }
    return ok(source[index].amount)
}
fn Invoke(callback: fn(int, int): Reply!, left: int, right: int): Reply! {
    result = callback(left, right)?
    return ok(result)
}
fn Run(source: [Sample], base: int, enabled: bool): Reply! {
    counter = 0
    cursor = base
    fn Local(left: int, right: int): Reply! {
        a = Read(source, cursor + left)?
        if (enabled) { counter = counter * 10 + a }
        b = Read(source, cursor + right)?
        counter = counter * 10 + b
        return ok(Reply { total: counter, first: a, second: b })
    }
    picker: fn(int, int): Reply! = Local
    first = Invoke(picker.clone(), 0, 1)?
    cursor = cursor + 1
    second = Invoke(picker, 0, 1)?
    return ok(Reply { total: counter, first: first.total, second: second.total })
}
fn Eat(source: [Sample]): Reply! {
    return ok(Reply { total: source.len(), first: 0, second: 0 })
}
fn Moved(source: [Sample]): Reply! {
    fn Local(left: int, right: int): Reply! {
        amount = Read(source, left + right)?
        return ok(Reply { total: amount, first: 0, second: 0 })
    }
    skipped = Eat(source)?
    return Invoke(Local, skipped.total, 0)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "callback-local.ku\n").unwrap();
    let emit = |name, mode| {
        fs::write(cwd.join("function.txt"), name).unwrap();
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        execute(&mut Command::new(seed), cwd, 30)
    };
    let rejected = emit("Moved", "linked-null");
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_callback_owner"));

    let linker = Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap/tools/link_c.py");
    let c_compiler = std::env::var("CC").unwrap_or_else(|_| {
        if cfg!(windows) {
            "clang".into()
        } else {
            "cc".into()
        }
    });
    let mut link = Command::new(python);
    link.arg("-B").arg(linker).arg("--cc").arg(c_compiler);
    let mut entry_c = String::new();
    for name in ["Run", "Invoke", "Read"] {
        let plan = emit(name, "linked-null-plan");
        assert_ok(&plan);
        let generated = emit(name, "linked-null");
        assert_ok(&generated);
        let plan_path = cwd.join(format!("local-{name}.json"));
        let source_path = cwd.join(format!("local-{name}.c"));
        fs::write(&plan_path, plan.stdout).unwrap();
        fs::write(&source_path, &generated.stdout).unwrap();
        link.arg("--unit").args([plan_path, source_path]);
        if name == "Run" {
            entry_c = String::from_utf8(generated.stdout).unwrap();
        }
    }
    let (head, parameters) = entry_c
        .lines()
        .find(|line| line.starts_with("KuBootstrapResult_") && line.contains(" ku_linked_null_"))
        .and_then(|line| line.split_once('('))
        .unwrap();
    let (result, entry) = head.split_once(' ').unwrap();
    let (array, _) = parameters.split_once(" a0,").unwrap();
    let element = array.strip_prefix("KuArray_").unwrap();
    assert!(entry_c.contains(&format!("const {array}* c")));
    assert!(entry_c.contains("void *raw, int64_t a0, int64_t a1"));
    assert!(entry_c.contains(", &environment"));
    assert!(!entry_c.contains("malloc("));
    let host = r#"
#include <assert.h>
static ARRAY sample(void) {
    ELEMENT *data = calloc(3, sizeof *data);
    assert(data);
    for (size_t i = 0; i < 3; ++i) {
        uint8_t *label = malloc(1);
        assert(label);
        *label = (uint8_t)('a' + i);
        data[i].f0 = (int64_t)i + 1;
        data[i].f1 = (KuProjectionString){label, 1, 1, KU_PROJECTION_STRING_OWNED};
    }
    return (ARRAY){3, data, 3};
}
static void check(bool enabled, int64_t first, int64_t total) {
    RESULT result = ENTRY(sample(), 0, enabled);
    assert(result.ok && result.value.f0 == total && result.value.f1 == first && result.value.f2 == total);
}
int main(void) {
    check(true, 12, 1223);
    check(false, 2, 23);
    RESULT failed = ENTRY(sample(), -1, true);
    assert(!failed.ok && failed.error.code.len == 5 && !memcmp(failed.error.code.ptr, "index", 5));
    check(true, 12, 1223);
    return 0;
}
"#
    .replace("ENTRY", entry)
    .replace("RESULT", result)
    .replace("ARRAY", array)
    .replace("ELEMENT", element);
    let host_path = cwd.join("callback-local-caller.c");
    let exe = cwd.join(format!("callback-local{}", std::env::consts::EXE_SUFFIX));
    fs::write(&host_path, host).unwrap();
    link.arg("--entry")
        .args([
            "callback-local.ku",
            entry.strip_prefix("ku_linked_null_").unwrap(),
        ])
        .arg("--caller")
        .arg(host_path)
        .arg("-o")
        .arg(&exe);
    assert_ok(&execute(&mut link, cwd, 45));
    assert_ok(&execute(&mut Command::new(exe), cwd, 10));
}

pub(super) fn assert_empty_owned_int_argument(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("empty-argument.ku"),
        r#"fn Left(text: str): str! { return ok(text) }
fn Right(failing: bool): str! {
    if (failing) { fail { domain: "witness", code: "right", message: "right failed" } }
    late = "late"
    return ok(late)
}
fn Take(left: str, children: [int], right: str): null! { return ok(null) }
fn Use(owner: str, failing: bool): null! { return Take(Left(owner)?, [], Right(failing)?) }
fn Nonempty(left: str, right: str): null! { return Take(left, [1], right) }
fn View(&children: [int]): null! { return ok(null) }
fn ViewEmpty(): null! { return View([]) }
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "empty-argument.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emit = |name| {
        fs::write(cwd.join("function.txt"), name).unwrap();
        execute(&mut Command::new(seed), cwd, 30)
    };
    let projected = emit("Use");
    assert_ok(&projected);
    let code = String::from_utf8(projected.stdout).unwrap();
    assert!(code.contains(" = (KuArrayInt){0};"));
    assert!(!code.contains("array_append_move("));
    let entry = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let provider = |result: &str, signature: &str| {
        code.lines()
            .filter_map(|line| line.strip_prefix(&format!("extern {result} ")))
            .find_map(|line| line.strip_suffix(signature))
            .expect("matching generated dependency")
    };
    let string_result = "KuBootstrapResult_KuProjectionString";
    let host = r#"
static int order;
static KuProjectionString owned(uint8_t value) {
    uint8_t *data = malloc(1);
    assert(data);
    *data = value;
    return (KuProjectionString){data, 1, 1, KU_PROJECTION_STRING_OWNED};
}
KuBootstrapResult_KuProjectionString LEFT(KuProjectionString value) {
    assert(order == 0 && *value.ptr == 'l');
    order = 1;
    return (KuBootstrapResult_KuProjectionString){.ok = true, .value = value};
}
KuBootstrapResult_KuProjectionString RIGHT(bool failing) {
    assert(order == 1);
    order = 2;
    if (failing) return (KuBootstrapResult_KuProjectionString){
        .error.code = {(uint8_t*)"right", 5, 0, KU_PROJECTION_STRING_STATIC}};
    return (KuBootstrapResult_KuProjectionString){.ok = true, .value = owned('r')};
}
KuBootstrapResultNull TAKE(KuProjectionString left, KuArrayInt empty, KuProjectionString right) {
    assert(order == 2 && *left.ptr == 'l' && *right.ptr == 'r');
    assert(!empty.len && !empty.data && !empty.capacity);
    order = 3;
    ku_projection_string_drop(&left);
    ku_projection_string_drop(&right);
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    (void)test_realloc;
    KuBootstrapResultNull result = ENTRY(owned('l'), false);
    assert(result.ok && order == 3 && attempts == 2 && !outstanding());
    order = 0;
    result = ENTRY(owned('l'), true);
    assert(!result.ok && order == 2 && attempts == 3 && !outstanding());
    assert(result.error.code.len == 5 && !memcmp(result.error.code.ptr, "right", 5));
    return 0;
}
"#
    .replace("ENTRY", &format!("ku_linked_null_{entry}"))
    .replace("LEFT", provider(string_result, "(KuProjectionString);"))
    .replace("RIGHT", provider(string_result, "(bool);"))
    .replace(
        "TAKE",
        provider(
            "KuBootstrapResultNull",
            "(KuProjectionString, KuArrayInt, KuProjectionString);",
        ),
    );
    let source = cwd.join("empty-argument.c");
    let exe = cwd.join(format!("empty-argument{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 10));

    let nonempty = emit("Nonempty");
    assert_ok(&nonempty);
    assert!(String::from_utf8_lossy(&nonempty.stdout).contains("array_append_move("));
    let borrowed = emit("ViewEmpty");
    assert_eq!(borrowed.status.code(), Some(1));
    assert!(borrowed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&borrowed.stderr).contains("unsupported_linked_operand"));
}

pub(super) fn assert_terminal_branch_ownership(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("guard-provider.ku"),
        "fn Take(payload: str): null! { return ok(null) }\nfn Keep(payload: str): null! { return ok(null) }\n",
    )
    .unwrap();
    fs::write(
        cwd.join("guard-entry.ku"),
        r#"import { Take, Keep } from "./guard-provider.ku"
fn Inspect(entered: bool, early: bool, failed: bool, text: str): null! {
    if (entered) {
        local = text.clone()
        if (early) { return Take(text) }
        if (failed) { fail { domain: "test", code: "take", message: local } }
        Keep(local)?
    }
    return Take(text)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "guard-entry.ku\nguard-provider.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Inspect").unwrap();
    let emit = |mode| {
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        let result = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&result);
        String::from_utf8(result.stdout).unwrap()
    };
    let document = emit("linked-null-plan");
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(document)], Span::default()).unwrap()
    else {
        panic!("expected a typed guard link plan");
    };
    let Value::Object(plan) = *value else {
        panic!("expected a plan")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected a guard export")
    };
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected checked providers in link plan")
    };
    let symbol = |value: &Value| match value {
        Value::String(string) => string.clone(),
        _ => panic!("expected a generated symbol"),
    };
    let provider = |name: &str| {
        dependencies
            .iter()
            .find_map(|dependency| match dependency {
                Value::Object(decl) if decl["source_name"] == Value::String(name.into()) => {
                    Some(symbol(&decl["symbol"]))
                }
                _ => None,
            })
            .expect("expected bound provider")
    };
    let host = r#"
static int taken, kept;
static uint8_t *original;
KuBootstrapResultNull __TAKE__(KuProjectionString payload) {
    assert(payload.ptr == original && payload.len == 7);
    ++taken;
    ku_projection_string_drop(&payload);
    return (KuBootstrapResultNull){.ok=true};
}
KuBootstrapResultNull __KEEP__(KuProjectionString payload) {
    assert(payload.ptr != original && payload.len == 7);
    assert(memcmp(payload.ptr, original, 7) == 0);
    ++kept;
    ku_projection_string_drop(&payload);
    return (KuBootstrapResultNull){.ok=true};
}
static KuProjectionString owner(void) {
    original = malloc(7);
    memcpy(original, "payload", 7);
    return (KuProjectionString){original,7,7,KU_PROJECTION_STRING_OWNED};
}
static void verify_failure(void) {
    if (outstanding() || taken || kept) _Exit(81);
}
int main(int argc, char **argv) {
    (void)test_realloc;
    (void)argv;
    if (argc == 2) {
        KuProjectionString input = owner();
        fail_after = 0;
        assert(atexit(verify_failure) == 0);
        (void)__ENTRY__(true, false, false, input);
        return 82;
    }
    KuBootstrapResultNull result = __ENTRY__(false, true, true, owner());
    assert(result.ok && taken == 1 && !kept && !outstanding());
    result = __ENTRY__(true, true, true, owner());
    assert(result.ok && taken == 2 && !kept && !outstanding());
    result = __ENTRY__(true, false, false, owner());
    assert(result.ok && taken == 3 && kept == 1 && !outstanding());
    result = __ENTRY__(true, false, true, owner());
    assert(!result.ok && taken == 3 && kept == 1 && outstanding() == 1);
    assert(result.error.message.ptr != original && result.error.message.len == 7);
    assert(memcmp(result.error.message.ptr, "payload", 7) == 0);
    ku_projection_string_drop(&result.error.message);
    assert(!outstanding());
    return 0;
}
"#
    .replace("__ENTRY__", &symbol(&export["symbol"]))
    .replace("__TAKE__", &provider("Take"))
    .replace("__KEEP__", &provider("Keep"));
    let generated = emit("linked-null");
    let source = cwd.join("guard-owned.c");
    let program = cwd.join(format!("guard-owned{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!("{CHECKED_ARRAY_ALLOCATOR}{generated}{host}"),
    )
    .unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 15));
    let recorded_failure = execute(Command::new(&program).arg("oom"), cwd, 15);
    assert_eq!(recorded_failure.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&recorded_failure.stderr).trim(),
        "out of memory"
    );
}

pub(super) fn assert_checked_output_ownership(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("output-source.ku"),
        r#"import { Load } from "./output-dependency.ku"
fn Publish(&prefix: str, fail_read: bool): null! {
    print(prefix)
    println(Load(fail_read)?)
    print(prefix)
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("output-dependency.ku"),
        r#"fn Load(fail_read: bool): str! { return ok("Ku🙂") }
fn Borrow(&value: str): str! { return ok(value) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "output-source.ku\noutput-dependency.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Publish").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let generated = String::from_utf8(emitted.stdout).unwrap();
    let entry = generated
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResultNull ku_linked_null_"))
        .and_then(|line| line.split_once('('))
        .map(|(number, _)| format!("ku_linked_null_{number}"))
        .unwrap();
    let load = generated
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResult_KuProjectionString "))
        .and_then(|line| line.split_once('('))
        .map(|(name, _)| name)
        .unwrap();
    let limited_write = r#"
static int fail_output;
static size_t output_bytes;
static size_t limited_write(const void *ptr, size_t size, size_t count, FILE *stream) {
    assert(size == 1 && count && stream == stdout);
    if (fail_output && output_bytes == 4) return 0;
    if (count > 2) count = 2;
    size_t written = fwrite(ptr, 1, count, stream);
    output_bytes += written;
    return written;
}
#define fwrite limited_write
"#;
    let host = r#"
#ifdef _WIN32
#include <fcntl.h>
#include <io.h>
#endif
static int calls;
static KuProjectionString prefix;
KuBootstrapResult_KuProjectionString __LOAD__(bool reject) {
    static const uint8_t spelling[] = {'x',0,0xe7,0xba,0xb8};
    uint8_t *owned = malloc(sizeof(spelling));
    memcpy(owned, spelling, sizeof(spelling));
    KuProjectionString data = {owned,sizeof(spelling),sizeof(spelling),KU_PROJECTION_STRING_OWNED};
    ++calls;
    if (reject) return (KuBootstrapResult_KuProjectionString){.error = {.message = data}};
    return (KuBootstrapResult_KuProjectionString){.ok = true, .value = data};
}
static void output_failure_cleanup(void) {
    if (outstanding() != 1 || calls != 1 || output_bytes != 4) _Exit(91);
    ku_projection_string_drop(&prefix);
}
int main(int argc, char **argv) {
    (void)test_realloc;
    (void)argv;
#ifdef _WIN32
    assert(_setmode(_fileno(stdout), _O_BINARY) != -1);
#endif
    uint8_t *bytes = malloc(2);
    memcpy(bytes, "H:", 2);
    prefix = (KuProjectionString){bytes,2,2,KU_PROJECTION_STRING_OWNED};
    fail_output = argc == 3;
    if (fail_output) assert(atexit(output_failure_cleanup) == 0);
    KuBootstrapResultNull result = __ENTRY__(&prefix, argc == 2);
    assert(calls == 1 && prefix.ptr == bytes && prefix.len == 2);
    if (argc == 2) {
        assert(!result.ok && result.error.message.len == 5 && outstanding() == 2);
        ku_projection_string_drop(&result.error.message);
    } else {
        assert(result.ok && outstanding() == 1);
    }
    ku_projection_string_drop(&prefix);
    assert(!outstanding());
    return 0;
}
"#
    .replace("__ENTRY__", &entry)
    .replace("__LOAD__", load);
    let source = cwd.join("stdout-owner.c");
    let program = cwd.join(format!("stdout-owner{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!("{CHECKED_ARRAY_ALLOCATOR}{limited_write}{generated}{host}"),
    )
    .unwrap();
    compile_projection(&source, &program, cwd);
    let success = execute(&mut Command::new(&program), cwd, 15);
    assert_ok(&success);
    assert_eq!(success.stdout, "H:x\0纸\nH:".as_bytes());
    let failed_read = execute(Command::new(&program).arg("read-error"), cwd, 15);
    assert_ok(&failed_read);
    assert_eq!(failed_read.stdout, b"H:");
    let failed_output = execute(Command::new(&program).args(["output", "failed"]), cwd, 15);
    assert_eq!(failed_output.status.code(), Some(1));
    assert_eq!(failed_output.stdout, b"H:x\0");
    assert_eq!(
        String::from_utf8_lossy(&failed_output.stderr).trim(),
        "output write failed"
    );

    fs::write(cwd.join("modules.txt"), "output-dependency.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Load").unwrap();
    let static_output = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&static_output);
    let code = String::from_utf8(static_output.stdout).unwrap();
    let load = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResult_KuProjectionString "))
        .and_then(|line| line.split_once('('))
        .unwrap()
        .0;
    let host = r#"
int main(void) {
    (void)test_malloc; (void)test_realloc; (void)test_free;
    static const uint8_t expected[] = {0x4b,0x75,0xf0,0x9f,0x99,0x82};
    KuBootstrapResult_KuProjectionString result = __LOAD__(false);
    assert(result.ok && result.value.len == sizeof(expected));
    assert(!memcmp(result.value.ptr, expected, sizeof(expected)));
    assert(result.value.storage == KU_PROJECTION_STRING_STATIC && !result.value.capacity);
    assert(!outstanding() && !attempts);
    return 0;
}
"#
    .replace("__LOAD__", load);
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 15));

    fs::write(cwd.join("function.txt"), "Borrow").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_result_return"));
}

const TRY_SOURCE: &str = r#"import { Supply, Take, Observe } from "./try-provider.ku"
fn Inspect(recover: bool, clone_first: bool): str! {
    try {
        first: str = Supply(1)?
        if (clone_first) { copy = first.clone() }
        second = Supply(2)?
        Take(first)?
        return ok(second)
    } catch (err) {
        Observe()?
        if (recover) {
            replacement = Supply(3)?
            return ok(replacement)
        }
        fail err
    }
}
fn PreserveOuter(text: str): str! {
    try {
        temporary = Supply(1)?
        return ok(text)
    } catch (err) {
        return ok(text)
    }
}
fn WrongDeclared(): null! {
    typed: int = Supply(1)?
    return ok(null)
}
fn RejectMoved(text: str): str! {
    try {
        Take(text)?
        result = Supply(1)?
        return ok(result)
    } catch (err) {
        return ok(text)
    }
}
"#;

const TRY_PROVIDER: &str = r#"
fn Supply(stage: int): str! { return ok("test") }
fn Take(text: str): null! { return ok(null) }
fn Observe(): null! { return ok(null) }
"#;

const TRY_HOST: &str = r#"
static int failed_stage, taken, observed;
static KuProjectionString owned(const char *text, size_t len) {
    uint8_t *data = malloc(len);
    assert(data);
    memcpy(data, text, len);
    return (KuProjectionString){data,len,len,KU_PROJECTION_STRING_OWNED};
}
KuBootstrapResult_KuProjectionString __SUPPLY__(int64_t stage) {
    KuBootstrapResult_KuProjectionString result = {0};
    if (stage == failed_stage) {
        result.error = (KuBootstrapError){owned("scope",5),owned("failure",7),owned("original",8)};
    } else {
        result.ok = true;
        result.value = owned(stage == 3 ? "recovery" : "provided",8);
    }
    return result;
}
KuBootstrapResultNull __TAKE__(KuProjectionString value) {
    ++taken;
    ku_projection_string_drop(&value);
    return (KuBootstrapResultNull){.ok = true};
}
KuBootstrapResultNull __OBSERVE__(void) {
    ++observed;
    // The protected first owner is moved only on success. A failing
    // second call still requires its drop before the catch runs.
    assert(outstanding() == 3);
    return (KuBootstrapResultNull){.ok = true};
}
static void verify_fatal(void) {
    if (outstanding() || taken || observed || attempts != 2) _Exit(81);
}
int main(int argc, char **argv) {
    (void)argv;
    (void)test_realloc;
    if (argc == 2) {
        fail_after = 1;
        assert(atexit(verify_fatal) == 0);
        (void)__ENTRY__(false, true);
        return 82;
    }
    KuBootstrapResult_KuProjectionString result = __ENTRY__(false, true);
    assert(result.ok && !observed && taken == 1 && outstanding() == 1);
    ku_projection_string_drop(&result.value);

    for (failed_stage = 1; failed_stage <= 2; ++failed_stage) {
        result = __ENTRY__(false, false);
        assert(!result.ok && !result.value.ptr && taken == 1 && outstanding() == 3);
        assert(result.error.domain.len == 5 && !memcmp(result.error.domain.ptr,"scope",5));
        assert(result.error.code.len == 7 && !memcmp(result.error.code.ptr,"failure",7));
        assert(result.error.message.len == 8 && !memcmp(result.error.message.ptr,"original",8));
        ku_projection_string_drop(&result.error.domain);
        ku_projection_string_drop(&result.error.code);
        ku_projection_string_drop(&result.error.message);
        assert(!outstanding());
    }
    failed_stage = 2;
    result = __ENTRY__(true, false);
    assert(result.ok && observed == 3 && taken == 1 && outstanding() == 1);
    assert(result.value.len == 8 && !memcmp(result.value.ptr,"recovery",8));
    ku_projection_string_drop(&result.value);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_terminal_try_unwind(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("try.ku"), TRY_SOURCE).unwrap();
    fs::write(cwd.join("try-provider.ku"), TRY_PROVIDER).unwrap();
    fs::write(cwd.join("modules.txt"), "try.ku\ntry-provider.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Inspect").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let planned = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&planned);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(planned.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected a try/catch link plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected link plan object")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::Array(deps) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let symbol = |value: &Value| match value {
        Value::String(text) => text.clone(),
        _ => panic!("expected actual symbol"),
    };
    let mut host = TRY_HOST.replace("__ENTRY__", &symbol(&export["symbol"]));
    for (name, marker) in [
        ("Supply", "__SUPPLY__"),
        ("Take", "__TAKE__"),
        ("Observe", "__OBSERVE__"),
    ] {
        let provider = deps
            .iter()
            .find_map(|dep| match dep {
                Value::Object(decl) if decl["source_name"] == Value::String(name.into()) => {
                    Some(symbol(&decl["symbol"]))
                }
                _ => None,
            })
            .expect("required checked dependency");
        host = host.replace(marker, &provider);
    }
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let source = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&source);
    let file = cwd.join("try-owner.c");
    let exe = cwd.join(format!("try-owner{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &file,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{}{host}",
            String::from_utf8(source.stdout).unwrap()
        ),
    )
    .unwrap();
    compile_projection(&file, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    let fatal = execute(Command::new(exe).arg("oom"), cwd, 15);
    assert_eq!(fatal.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&fatal.stderr).trim(),
        "out of memory"
    );

    fs::write(cwd.join("function.txt"), "PreserveOuter").unwrap();
    assert_ok(&execute(&mut Command::new(seed), cwd, 30));
    for (name, reason) in [
        ("RejectMoved", "unavailable_linked_binding"),
        ("WrongDeclared", "linked_local_type"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let refused = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(refused.status.code(), Some(1));
        assert!(refused.stdout.is_empty());
        assert!(String::from_utf8_lossy(&refused.stderr).contains(reason));
    }
}

pub(super) fn assert_ignored_result_cleanup(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("ignored-source.ku"),
        r#"import { Open, Observe } from "./ignored-dependency.ku"
fn Use(failed: bool): null! {
    Open(failed)?
    Observe()?
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("ignored-dependency.ku"),
        r#"struct Item { label: str }
struct Packet { item: Item }
fn Open(failed: bool): Packet! { return ok(Packet { item: Item { label: "payload" } }) }
fn Observe(): null! { return ok(null) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "ignored-source.ku\nignored-dependency.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Use").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let generated = String::from_utf8(emitted.stdout).unwrap();
    let declaration = |prefix: &str| {
        let line = generated
            .lines()
            .find(|line| line.starts_with(prefix))
            .expect("generated declaration");
        let (function, _) = line.split_once('(').expect("function parameters");
        function.rsplit_once(' ').unwrap()
    };
    let (_, entry) = declaration("KuBootstrapResultNull ku_linked_null_");
    let (open_result, open) = declaration("extern KuBootstrapResult_KuStruct_");
    let (_, observe) = declaration("extern KuBootstrapResultNull ku_value_dependency_");
    let host = r#"
static int opens, checks;
__RESULT__ __OPEN__(bool bad) {
    uint8_t *bytes = malloc(7);
    memcpy(bytes, "payload", 7);
    KuProjectionString owned = { bytes,7,7,KU_PROJECTION_STRING_OWNED };
    ++opens;
    if (bad) return (__RESULT__){.error = {.message = owned}};
    __RESULT__ result = {.ok = true};
    result.value.f0.f0 = owned;
    return result;
}
KuBootstrapResultNull __OBSERVE__(void) {
    assert(outstanding() == 0);
    ++checks;
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    (void)test_realloc;
    assert(__ENTRY__(false).ok);
    assert(opens == 1 && checks == 1 && outstanding() == 0);
    KuBootstrapResultNull errored = __ENTRY__(true);
    assert(!errored.ok && opens == 2 && checks == 1 && outstanding() == 1);
    assert(memcmp(errored.error.message.ptr,"payload",7) == 0);
    ku_projection_string_drop(&errored.error.message);
    assert(outstanding() == 0);
    return 0;
}
"#
    .replace("__ENTRY__", entry)
    .replace("__OPEN__", open)
    .replace("__RESULT__", open_result.trim_start_matches("extern "))
    .replace("__OBSERVE__", observe);
    let source = cwd.join("ignored-result.c");
    let program = cwd.join(format!("ignored-result{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!("{CHECKED_ARRAY_ALLOCATOR}{generated}{host}"),
    )
    .unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 15));
}

const REBIND_SOURCE: &str = r#"import { Supply } from "./try-provider.ku"
fn Replace(first: bool): str! {
    owner: str = Supply(1)?
    try {
        if (first) { owner = Supply(2)? }
        else { owner = Supply(3)? }
        return ok(owner)
    } catch (err) {
        return ok(owner)
    }
}
fn Append(first: bool): str! {
    owner: str = Supply(1)?
    try {
        if (first) { owner += Supply(2)? }
        else { owner += Supply(3)? }
        return ok(owner)
    } catch (err) {
        return ok(owner)
    }
}
fn Consume(value: str): null! { return ok(null) }
fn Keep(value: str): str! { return ok(value) }
fn KeepRight(first: bool): str! {
    left = Supply(1)?
    right = Supply(1)?
    if (first) { Consume(left)? }
    else { right = Supply(3)? }
    return ok(right)
}
fn Rebound(first: bool): str! {
    owner = Supply(1)?
    if (first) { Consume(owner)? }
    owner = Supply(2)?
    return ok(owner)
}
fn ReadMaybeMoved(first: bool): int! {
    owner = Supply(1)?
    if (first) { Consume(owner)? }
    return ok(owner.len())
}
struct Bundle { left: str right: str }
fn WrongMembers(bundle: Bundle, first: bool): null! {
    if (first) { Consume(bundle.left)? }
    else { Consume(bundle.right)? }
    return ok(null)
}
fn CatchMoved(owner: str): str! {
    try {
        owner = Keep(owner)?
        return ok(owner)
    } catch (err) {
        return ok(owner)
    }
}
"#;

const REBIND_HOST: &str = r#"
static int refused, selected;
static uint8_t *original;
static uintptr_t original_address;
KuBootstrapResult_KuProjectionString __SUPPLY__(int64_t stage) {
    KuBootstrapResult_KuProjectionString result = {0};
    if (stage != 1) {
        assert(outstanding() == 1 && *original == 1);
        selected = (int)stage;
    }
    uint8_t *data = malloc(1);
    *data = (uint8_t)stage;
    KuProjectionString text = {data,1,1,KU_PROJECTION_STRING_OWNED};
    if (stage == refused) {
        result.error.message = text;
        return result;
    }
    if (stage == 1) { original = data; original_address = (uintptr_t)data; }
    result.ok = true;
    result.value = text;
    return result;
}
int main(void) {
    (void)test_realloc;
    for (int i = 2; i <= 3; ++i) {
        refused = 0;
        KuBootstrapResult_KuProjectionString result = __ENTRY__(i == 2);
        assert(result.ok && selected == i && (uintptr_t)result.value.ptr != original_address);
        assert(outstanding() == 1);
        if (__APPEND__) {
            assert(result.value.len == 2 && result.value.ptr[0] == 1 && result.value.ptr[1] == i);
        } else { assert(result.value.len == 1 && *result.value.ptr == i); }
        ku_projection_string_drop(&result.value);
        assert(!outstanding());

        refused = i;
        result = __ENTRY__(i == 2);
        assert(result.ok && selected == i && result.value.ptr == original);
        assert(outstanding() == 1 && *result.value.ptr == 1 && !result.error.message.ptr);
        ku_projection_string_drop(&result.value);
        assert(!outstanding());
    }
    return 0;
}
"#;

pub(super) fn assert_checked_string_rebind(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("rebind.ku"), REBIND_SOURCE).unwrap();
    fs::write(cwd.join("try-provider.ku"), TRY_PROVIDER).unwrap();
    fs::write(cwd.join("modules.txt"), "rebind.ku\ntry-provider.ku\n").unwrap();
    for name in ["Replace", "Append"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
        let planned = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&planned);
        let Some(Value::Result { ok: true, value }) = json::eval(
            "decode",
            &[Value::String(String::from_utf8(planned.stdout).unwrap())],
            Span::default(),
        )
        .unwrap() else {
            panic!("expected a typed checked replacement plan")
        };
        let Value::Object(plan) = *value else {
            panic!("expected plan object")
        };
        let Value::Object(export) = &plan["export"] else {
            panic!("expected export")
        };
        let Value::Array(deps) = &plan["dependencies"] else {
            panic!("expected dependencies")
        };
        let [Value::Object(supply)] = deps.as_slice() else {
            panic!("expected one supplier")
        };
        let (Value::String(entry), Value::String(provider)) =
            (&export["symbol"], &supply["symbol"])
        else {
            panic!("expected the bound symbols")
        };
        let host = REBIND_HOST
            .replace("__ENTRY__", entry)
            .replace("__SUPPLY__", provider)
            .replace(
                "__APPEND__",
                if name == "Append" { "true" } else { "false" },
            );
        fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let source = cwd.join("rebind-owner.c");
        let exe = cwd.join(format!("rebind-owner{}", std::env::consts::EXE_SUFFIX));
        fs::write(
            &source,
            format!(
                "{CHECKED_ARRAY_ALLOCATOR}{}{host}",
                String::from_utf8(emitted.stdout).unwrap()
            ),
        )
        .unwrap();
        compile_projection(&source, &exe, cwd);
        assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    }
    fs::write(cwd.join("function.txt"), "KeepRight").unwrap();
    assert_ok(&execute(&mut Command::new(seed), cwd, 30));
    for (name, diagnostic) in [
        ("WrongMembers", "linked_control_move"),
        ("ReadMaybeMoved", "unavailable_linked_binding"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let wrong = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(wrong.status.code(), Some(1));
        assert!(wrong.stdout.is_empty());
        assert!(String::from_utf8_lossy(&wrong.stderr).contains(diagnostic));
    }
    fs::write(cwd.join("function.txt"), "CatchMoved").unwrap();
    let moved = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(moved.status.code(), Some(1));
    assert!(moved.stdout.is_empty());
    assert!(String::from_utf8_lossy(&moved.stderr).contains("unavailable_linked_binding"));

    fs::write(cwd.join("function.txt"), "Rebound").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let symbol = |prefix: &str, suffix: &str| {
        code.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap()
            .split_once(suffix)
            .unwrap()
            .0
    };
    let host = r#"
static bool consume_fails, supply_fails;
static unsigned calls;
KuBootstrapResult_KuProjectionString __SUPPLY__(int64_t stage) {
    KuBootstrapResult_KuProjectionString result = {0};
    ++calls;
    uint8_t *data = malloc(1);
    *data = (uint8_t)stage;
    KuProjectionString text = {data,1,1,KU_PROJECTION_STRING_OWNED};
    if (stage == 2 && supply_fails) { result.error.message = text; return result; }
    result.ok = true;
    result.value = text;
    return result;
}
KuBootstrapResultNull __CONSUME__(KuProjectionString text) {
    assert(outstanding() == 1 && text.len == 1 && *text.ptr == 1);
    if (consume_fails) return (KuBootstrapResultNull){.error={.message=text}};
    ku_projection_string_drop(&text);
    return (KuBootstrapResultNull){.ok=true};
}
int main(void) {
    (void)test_realloc;
    for (int first = 0; first <= 1; ++first) {
        calls = 0;
        KuBootstrapResult_KuProjectionString output = __ENTRY__(first != 0);
        assert(output.ok && calls == 2 && outstanding() == 1 && *output.value.ptr == 2);
        ku_projection_string_drop(&output.value);
        assert(!outstanding());
    }
    calls = 0;
    consume_fails = true;
    KuBootstrapResult_KuProjectionString output = __ENTRY__(true);
    assert(!output.ok && calls == 1 && outstanding() == 1 && *output.error.message.ptr == 1);
    ku_projection_string_drop(&output.error.message);
    assert(!outstanding());
    calls = 0;
    consume_fails = false;
    supply_fails = true;
    output = __ENTRY__(false);
    assert(!output.ok && calls == 2 && outstanding() == 1 && *output.error.message.ptr == 2);
    ku_projection_string_drop(&output.error.message);
    assert(!outstanding());
    return 0;
}
"#
    .replace(
        "__SUPPLY__",
        symbol("extern KuBootstrapResult_KuProjectionString ", "("),
    )
    .replace("__CONSUME__", symbol("extern KuBootstrapResultNull ", "("))
    .replace(
        "__ENTRY__",
        symbol("KuBootstrapResult_KuProjectionString ", "("),
    );
    let source = cwd.join("joined-string-owner.c");
    let binary = cwd.join(format!(
        "joined-string-owner{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &binary, cwd);
    assert_ok(&execute(&mut Command::new(binary), cwd, 15));
}

pub(super) fn assert_conditional_whole_record_move(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("conditional-record.ku"),
        r#"struct Parcel { label: str }

fn Receive(parcel: Parcel, fail_now: bool): null! { return ok(null) }
fn Observe(&parcel: Parcel): null! { return ok(null) }
fn End(fail_later: bool): null! { return ok(null) }

fn Handle(move: bool, fail_now: bool, fail_later: bool, parcel: Parcel): null! {
    if (move) {
        Receive(parcel, fail_now)?
    } else {
        Observe(parcel)?
    }
    End(fail_later)?
    return ok(null)
}

fn Replace(move: bool, parcel: Parcel, replacement: Parcel): null! {
    if (move) { Receive(parcel, false)? }
    parcel = replacement
    Observe(parcel)?
    return ok(null)
}

fn InvalidRead(move: bool, parcel: Parcel): null! {
    if (move) { Receive(parcel, false)? }
    Observe(parcel)?
    return ok(null)
}

fn InvalidBorrow(move: bool, parcel: Parcel): null! {
    if (move) { Receive(parcel, false)? }
    alias: Parcel = parcel.clone()
    Observe(alias)?
    return ok(null)
}

fn InvalidBackedge(move: bool, upper: int, parcel: Parcel): null! {
    count = 0
    while (count < upper) {
        if (move) { Receive(parcel, false)? }
        count = count + 1
    }
    return ok(null)
}

fn TakeLabel(label: str): null! { return ok(null) }
fn InvalidPartial(move: bool, parcel: Parcel): null! {
    if (move) { Receive(parcel, false)? }
    else { TakeLabel(parcel.label)? }
    return ok(null)
}

fn InvalidView(move: bool, &parcel: Parcel): null! {
    if (move) { Receive(parcel, false)? }
    return ok(null)
}

fn TakeArray(boxes: [Parcel]): null! { return ok(null) }
fn InvalidArray(move: bool, boxes: [Parcel]): null! {
    if (move) { TakeArray(boxes)? }
    return ok(null)
}

fn ReboundStep(move: bool, count: int, parcel: Parcel): null! {
    step = 0
    while (step < count) {
        next = parcel.clone()
        if (move) { Receive(parcel, false)? }
        parcel = next
        step = step + 1
    }
    Observe(parcel)?
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "conditional-record.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();

    for name in ["Handle", "Replace", "ReboundStep"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let code = String::from_utf8(emitted.stdout).unwrap();
        let mut dependencies = code
            .lines()
            .filter_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
            .filter_map(|signature| signature.split_once('('));
        let (consume, parameters) = dependencies.next().unwrap();
        let record = parameters.split_once(',').unwrap().0;
        let (observe, _) = dependencies.next().unwrap();
        let end = if let Some((symbol, _)) = dependencies.next() {
            symbol
        } else {
            "unused_final_check"
        };
        let entry = code
            .lines()
            .find_map(|line| line.strip_prefix("KuBootstrapResultNull "))
            .and_then(|signature| signature.split_once('('))
            .unwrap()
            .0;
        let host = r#"
static unsigned consumed, observed, final_checks;
static unsigned live_before_final;
static uint8_t expected;

static __RECORD__ parcel(uint8_t byte) {
    uint8_t *label = malloc(1);
    *label = byte;
    return (__RECORD__){{label,1,1,KU_PROJECTION_STRING_OWNED}};
}
KuBootstrapResultNull __CONSUME__(__RECORD__ value, bool fail_now) {
    ++consumed;
    assert(value.f0.len == 1 && *value.f0.ptr == 3);
    if (fail_now) return (KuBootstrapResultNull){.error={.message=value.f0}};
    ku_projection_string_drop(&value.f0);
    return (KuBootstrapResultNull){.ok=true};
}
KuBootstrapResultNull __OBSERVE__(const __RECORD__ *value) {
    ++observed;
    assert(value->f0.len == 1 && *value->f0.ptr == expected);
    return (KuBootstrapResultNull){.ok=true};
}
KuBootstrapResultNull __END__(bool fail_later) {
    ++final_checks;
    assert(outstanding() == (int)live_before_final);
    if (fail_later) return (KuBootstrapResultNull){.error={.message=parcel(8).f0}};
    return (KuBootstrapResultNull){.ok=true};
}
int main(void) {
    (void)test_realloc;
#if __REPLACE__
    expected = 9;
    assert(__ENTRY__(false,parcel(3),parcel(9)).ok);
    assert(!outstanding() && consumed == 0 && observed == 1);
    assert(__ENTRY__(true,parcel(3),parcel(9)).ok);
    assert(!outstanding() && consumed == 1 && observed == 2);
#elif __REBOUND__
    expected = 3;
    assert(__ENTRY__(true,2,parcel(3)).ok);
    assert(!outstanding() && consumed == 2 && observed == 1);
    assert(__ENTRY__(false,2,parcel(3)).ok);
    assert(!outstanding() && consumed == 2 && observed == 2);
#else
    expected = 3;
    for (unsigned move = 0; move < 2; ++move) {
        live_before_final = move ? 0 : 1;
        assert(__ENTRY__(move,false,false,parcel(3)).ok);
        assert(!outstanding());
        KuBootstrapResultNull late = __ENTRY__(move,false,true,parcel(3));
        assert(!late.ok && outstanding() == 1 && *late.error.message.ptr == 8);
        ku_projection_string_drop(&late.error.message);
        assert(!outstanding());
    }
    assert(consumed == 2 && observed == 2 && final_checks == 4);
    KuBootstrapResultNull early = __ENTRY__(true,true,false,parcel(3));
    assert(!early.ok && consumed == 3 && final_checks == 4);
    assert(outstanding() == 1 && *early.error.message.ptr == 3);
    ku_projection_string_drop(&early.error.message);
    assert(!outstanding());
#endif
    return 0;
}
"#
        .replace("__ENTRY__", entry)
        .replace("__RECORD__", record)
        .replace("__CONSUME__", consume)
        .replace("__OBSERVE__", observe)
        .replace("__END__", end)
        .replace("__REBOUND__", if name == "ReboundStep" { "1" } else { "0" })
        .replace("__REPLACE__", if name == "Replace" { "1" } else { "0" });
        let source = cwd.join("conditional-record.c");
        let exe = cwd.join(format!(
            "conditional-record{}",
            std::env::consts::EXE_SUFFIX
        ));
        fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
        compile_projection(&source, &exe, cwd);
        assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    }

    for (name, diagnostic) in [
        ("InvalidRead", "unavailable_linked_binding"),
        ("InvalidBorrow", "unavailable_linked_binding"),
        ("InvalidBackedge", "linked_control_move"),
        ("InvalidPartial", "linked_control_move"),
        ("InvalidView", "linked_view_escape"),
        ("InvalidArray", "linked_control_move"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}

pub(super) fn assert_local_bool_while(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("local-bool-while.ku"),
        r#"struct Parcel { label: str }

fn Visit(step: int, producer: str): null! { return ok(null) }
fn ReadLabel(parcel: Parcel): Parcel! { return ok(parcel) }
fn Inspect(&parcel: Parcel): null! { return ok(null) }

fn Count(begin: bool, limit: int, &label: str): int! {
    active = begin
    step = 0
    while (active) {
        current = label.clone()
        Visit(step, current)?
        step = step + 1
        if (step >= limit) { active = false }
    }
    return ok(step)
}

fn TakeNow(begin: bool, parcel: Parcel): Parcel! {
    active = begin
    while (active) {
        local = ReadLabel(parcel)?
        return ok(local)
    }
    return ok(parcel)
}

fn Nested(begin: bool): int! {
    active = true
    step = 0
    if (begin) {
        while (active) {
            step = step + 1
            active = false
        }
    }
    return ok(step)
}

fn ForbiddenParameter(active: bool): int! {
    step = 0
    while (active) { step = step + 1 }
    return ok(step)
}
fn ForbiddenExpression(begin: bool): int! {
    active = begin
    while (!active) { active = true }
    return ok(0)
}
fn ForbiddenMove(begin: bool, parcel: Parcel): null! {
    active = begin
    while (active) {
        ReadLabel(parcel)?
        active = false
    }
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "local-bool-while.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();

    for name in ["Count", "TakeNow", "Nested"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let code = String::from_utf8(emitted.stdout).unwrap();
        assert!(code.contains("while (l") && !code.contains("while (true)"));
        let entry = code
            .lines()
            .find(|line| {
                line.starts_with("KuBootstrapResult_") && line.contains(" ku_linked_null_")
            })
            .and_then(|signature| signature.split_once(' '))
            .and_then(|(_, signature)| signature.split_once('('))
            .unwrap()
            .0;
        let provider = code
            .lines()
            .find_map(|line| line.strip_prefix("extern KuBootstrapResult"))
            .and_then(|signature| signature.split_once(' '))
            .and_then(|(_, signature)| signature.split_once('('));
        let (provider, parameters) = provider.unwrap_or(("", ""));
        let record = parameters.split_once(')').map_or("", |(record, _)| record);
        let host = r#"
#if __COUNT__
static unsigned visits;
static int fail_step = -1;
static const uint8_t *source_owner;
KuBootstrapResultNull __PROVIDER__(int64_t step, KuProjectionString label) {
    assert(step == (int64_t)visits++ && label.len == 1 && *label.ptr == 6);
    assert(outstanding() == 2 && label.ptr != source_owner);
    if (step == fail_step) return (KuBootstrapResultNull){.error={.message=label}};
    ku_projection_string_drop(&label);
    return (KuBootstrapResultNull){.ok=true};
}
#elif __TAKE__
static unsigned calls;
static bool fail_now;
KuBootstrapResult___RECORD__ __PROVIDER__(__RECORD__ parcel) {
    ++calls;
    assert(parcel.f0.len == 1 && *parcel.f0.ptr == 3);
    if (fail_now) return (KuBootstrapResult___RECORD__){.error={.message=parcel.f0}};
    return (KuBootstrapResult___RECORD__){.ok=true,.value=parcel};
}
#endif
int main(void) {
    (void)test_malloc;
    (void)test_realloc;
    (void)test_free;
#if __COUNT__
    uint8_t *bytes = malloc(1);
    *bytes = 6;
    source_owner = bytes;
    KuProjectionString label = {bytes,1,1,KU_PROJECTION_STRING_OWNED};
    KuBootstrapResult_int64_t output = __ENTRY__(false,4,&label);
    assert(output.ok && output.value == 0 && visits == 0 && outstanding() == 1);
    output = __ENTRY__(true,4,&label);
    assert(output.ok && output.value == 4 && visits == 4 && outstanding() == 1);
    fail_step = 1;
    visits = 0;
    output = __ENTRY__(true,4,&label);
    assert(!output.ok && visits == 2 && outstanding() == 2 && *output.error.message.ptr == 6);
    ku_projection_string_drop(&output.error.message);
    assert(label.ptr == source_owner && *label.ptr == 6 && outstanding() == 1);
    ku_projection_string_drop(&label);
    assert(!outstanding());
#elif __TAKE__
    for (int take = 0; take <= 1; ++take) {
        uint8_t *bytes = malloc(1);
        *bytes = 3;
        __RECORD__ parcel = {{bytes,1,1,KU_PROJECTION_STRING_OWNED}};
        KuBootstrapResult___RECORD__ output = __ENTRY__(take, parcel);
        assert(output.ok && calls == (unsigned)take && outstanding() == 1);
        assert(output.value.f0.ptr == bytes);
        ku_projection_string_drop(&output.value.f0);
        assert(!outstanding());
    }
    fail_now = true;
    uint8_t *bytes = malloc(1);
    *bytes = 3;
    __RECORD__ parcel = {{bytes,1,1,KU_PROJECTION_STRING_OWNED}};
    KuBootstrapResult___RECORD__ output = __ENTRY__(true, parcel);
    assert(!output.ok && calls == 2 && outstanding() == 1 && output.error.message.ptr == bytes);
    ku_projection_string_drop(&output.error.message);
    assert(!outstanding());
#else
    KuBootstrapResult_int64_t output = __ENTRY__(false);
    assert(output.ok && output.value == 0);
    output = __ENTRY__(true);
    assert(output.ok && output.value == 1);
#endif
    assert(!outstanding());
    return 0;
}
"#
        .replace("__ENTRY__", entry)
        .replace("__PROVIDER__", provider)
        .replace("__RECORD__", record)
        .replace("__COUNT__", if name == "Count" { "1" } else { "0" })
        .replace("__TAKE__", if name == "TakeNow" { "1" } else { "0" });
        let source = cwd.join("local-bool-while.c");
        let exe = cwd.join(format!("local-bool-while{}", std::env::consts::EXE_SUFFIX));
        fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
        compile_projection(&source, &exe, cwd);
        assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    }

    for (name, diagnostic) in [
        ("ForbiddenParameter", "unsupported_linked_loop"),
        ("ForbiddenExpression", "unsupported_linked_loop"),
        ("ForbiddenMove", "linked_control_move"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}

const ADD_CALL_SOURCE: &str = r#"import { Decorate, Suffix } from "./plus-provider.ku"
fn Compose(path: str): str! {
    result = Decorate(path) + Suffix(1)
    return ok(result)
}
fn CloneViews(&prefix: str, &path: str): str! {
    result = prefix.clone() + path.clone()
    return ok(result)
}
fn ReadBeforeMove(path: str): str! {
    result = path + Decorate(path)
    return ok(result)
}
fn ReadAfterMove(path: str): str! {
    moved = Decorate(path)
    return ok(path)
}
fn PlainLiteral(): str { return "源🙂" }
fn PlainOwner(source: str): str { return source }
fn PlainBorrow(&source: str): str { return source }
fn PlainFormat(value: int): str { return str(value) }
"#;

const ADD_CALL_PROVIDER: &str = r#"
fn Decorate(path: str): str { return path }
fn Suffix(value: int): str { return "!" }
"#;

const ADD_CALL_HOST: &str = r#"
static int sequence;
KuProjectionString __DECORATE__(KuProjectionString path) {
    assert(sequence == 0 && path.len == 4);
    sequence = 1;
    return path;
}
KuProjectionString __SUFFIX__(int64_t value) {
    assert(sequence == 1 && value == 1);
    sequence = 2;
    uint8_t *text = malloc(1);
    *text = '!';
    return (KuProjectionString){text,1,1,KU_PROJECTION_STRING_OWNED};
}
int main(void) {
    (void)test_realloc;
    uint8_t *path = malloc(4);
    memcpy(path,"path",4);
    KuBootstrapResult_KuProjectionString result = __ENTRY__(
        (KuProjectionString){path,4,4,KU_PROJECTION_STRING_OWNED});
    assert(result.ok && sequence == 2 && outstanding() == 1);
    assert(result.value.len == 5 && memcmp(result.value.ptr,"path!",5) == 0);
    ku_projection_string_drop(&result.value);
    assert(outstanding() == 0 && attempts == 3);
    return 0;
}
"#;

pub(super) fn assert_linked_call_addition(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("plus.ku"), ADD_CALL_SOURCE).unwrap();
    fs::write(cwd.join("plus-provider.ku"), ADD_CALL_PROVIDER).unwrap();
    fs::write(cwd.join("modules.txt"), "plus.ku\nplus-provider.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Compose").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(plan.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected call addition link plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected plan object")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::String(entry) = &export["symbol"] else {
        panic!("expected entry symbol")
    };
    let Value::Array(deps) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let mut host = ADD_CALL_HOST.replace("__ENTRY__", entry);
    for (name, marker) in [("Decorate", "__DECORATE__"), ("Suffix", "__SUFFIX__")] {
        let symbol = deps
            .iter()
            .find_map(|dependency| match dependency {
                Value::Object(decl) if decl["source_name"] == Value::String(name.into()) => {
                    match &decl["symbol"] {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                _ => None,
            })
            .expect("expected bound operand call");
        host = host.replace(marker, symbol);
    }
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let source = cwd.join("call-addition.c");
    let exe = cwd.join(format!("call-addition{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{}{host}",
            String::from_utf8(emitted.stdout).unwrap()
        ),
    )
    .unwrap();
    compile_projection(&source, &exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    fs::write(cwd.join("function.txt"), "CloneViews").unwrap();
    assert_ok(&execute(&mut Command::new(seed), cwd, 30));
    fs::write(cwd.join("function.txt"), "ReadBeforeMove").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    assert!(String::from_utf8_lossy(&emitted.stdout).contains("ku_linked_clone_string(a0, &lhs"));
    fs::write(cwd.join("function.txt"), "ReadAfterMove").unwrap();
    let bad = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(bad.status.code(), Some(1));
    assert!(bad.stdout.is_empty());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unavailable_linked_binding"));
    for name in ["PlainLiteral", "PlainOwner"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let result = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&result);
        let code = String::from_utf8(result.stdout).unwrap();
        let entry = code
            .lines()
            .find_map(|line| line.strip_prefix("KuProjectionString ku_linked_null_"))
            .unwrap()
            .split_once('(')
            .unwrap()
            .0;
        let expression = if name == "PlainLiteral" {
            "()"
        } else {
            "((KuProjectionString){data, 7, 7, KU_PROJECTION_STRING_OWNED})"
        };
        let main = r#"
int main(void) {
    (void)test_realloc;
    uint8_t *data = NULL;
    if (__OWNED__) {
        data = malloc(7);
        assert(data);
        memcpy(data, "payload", 7);
    }
    KuProjectionString result = __CALL__;
    assert(result.len == 7);
    if (__OWNED__) {
        assert(result.storage == KU_PROJECTION_STRING_OWNED && result.ptr == data && outstanding() == 1);
        free(result.ptr);
    } else { assert(result.storage == KU_PROJECTION_STRING_STATIC && outstanding() == 0); }
    assert(outstanding() == 0);
    return 0;
}
"#
        .replace("__CALL__", &format!("ku_linked_null_{entry}{expression}"))
        .replace(
            "__OWNED__",
            if name == "PlainOwner" {
                "true"
            } else {
                "false"
            },
        );
        fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{main}")).unwrap();
        let binary = cwd.join(format!("plain-string{}", std::env::consts::EXE_SUFFIX));
        compile_projection(&source, &binary, cwd);
        assert_ok(&execute(&mut Command::new(&binary), cwd, 15));
    }
    fs::write(cwd.join("function.txt"), "PlainFormat").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let entry = code
        .lines()
        .find_map(|line| line.strip_prefix("KuProjectionString ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let main = r#"
int main(void) {
    (void)test_realloc;
    int64_t signed_values[] = {0, INT64_MIN, INT64_MAX};
    const char* expected[] = {"0", "-9223372036854775808", "9223372036854775807"};
    for (size_t i = 0; i < 3; ++i) {
        KuProjectionString value = ENTRY(signed_values[i]);
        assert(value.storage == KU_PROJECTION_STRING_OWNED && outstanding() == 1);
        assert(value.len == strlen(expected[i]) && !memcmp(value.ptr, expected[i], value.len));
        free(value.ptr);
        assert(!outstanding());
    }
    assert(attempts == 3);
    return 0;
}
"#
    .replace("ENTRY", &format!("ku_linked_null_{entry}"));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{main}")).unwrap();
    let binary = cwd.join(format!("format-int{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&source, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 15));
    fs::write(cwd.join("function.txt"), "PlainBorrow").unwrap();
    let escaped = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(escaped.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&escaped.stderr).contains("linked_value_return"));
}

const DECORATED_CATCH_SOURCE: &str = r#"import { Supply, Decorate } from "./decorated-provider.ku"
fn Finish(path: str): str! {
    try {
        ready = Supply()?
        return ok(ready)
    } catch (err) {
        fail { domain: err.domain, code: err.code, message: Decorate(path) + ": " + err.message }
    }
}
fn Duplicate(): null! {
    try {
        ready = Supply()?
        return ok(null)
    } catch (err) {
        fail { domain: err.domain, code: err.code, message: "bad " + err.domain }
    }
}
"#;

const DECORATED_CATCH_PROVIDER: &str = r#"
fn Supply(): str! { return ok("success") }
fn Decorate(path: str): str { return path }
"#;

const DECORATED_CATCH_HOST: &str = r#"
static bool succeeds;
static uint8_t *domain, *error_code;
static KuProjectionString owned(const char *string, size_t len) {
    uint8_t *data = malloc(len);
    assert(data);
    memcpy(data,string,len);
    return (KuProjectionString){data,len,len,KU_PROJECTION_STRING_OWNED};
}
KuBootstrapResult_KuProjectionString __SUPPLY__(void) {
    KuBootstrapResult_KuProjectionString out = {0};
    if (succeeds) {
        out.ok = true;
        out.value = owned("success",7);
    } else {
        out.error.domain = owned("parse",5);
        out.error.code = owned("invalid",7);
        out.error.message = owned("structure",9);
        domain = out.error.domain.ptr;
        error_code = out.error.code.ptr;
    }
    return out;
}
KuProjectionString __DECORATE__(KuProjectionString path) {
    assert(path.len == 4 && memcmp(path.ptr,"file",4) == 0);
    return path;
}
static void verify_partial_oom(void) {
    if (outstanding() || attempts != 6) _Exit(81);
}
int main(int argc, char **argv) {
    (void)argv;
    (void)test_realloc;
    if (argc == 2) {
        KuProjectionString path = owned("file",4);
        fail_after = 4;
        assert(atexit(verify_partial_oom) == 0);
        (void)__ENTRY__(path);
        return 82;
    }
    KuBootstrapResult_KuProjectionString result = __ENTRY__(owned("file",4));
    assert(!result.ok && !result.value.ptr && attempts == 6 && outstanding() == 3);
    assert(result.error.domain.ptr == domain && result.error.domain.len == 5);
    assert(result.error.code.ptr == error_code && result.error.code.len == 7);
    assert(result.error.message.len == 15 && memcmp(result.error.message.ptr,"file: structure",15) == 0);
    ku_projection_string_drop(&result.error.domain);
    ku_projection_string_drop(&result.error.code);
    ku_projection_string_drop(&result.error.message);
    assert(!outstanding());
    succeeds = true;
    result = __ENTRY__(owned("file",4));
    assert(result.ok && result.value.len == 7 && outstanding() == 1);
    ku_projection_string_drop(&result.value);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_decorated_caught_error(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("decorated.ku"), DECORATED_CATCH_SOURCE).unwrap();
    fs::write(cwd.join("decorated-provider.ku"), DECORATED_CATCH_PROVIDER).unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "decorated.ku\ndecorated-provider.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Finish").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let metadata = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&metadata);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(metadata.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected caught Error link plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected plan object")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::String(entry) = &export["symbol"] else {
        panic!("expected entry symbol")
    };
    let Value::Array(deps) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let markers = [("Supply", "__SUPPLY__"), ("Decorate", "__DECORATE__")];
    let mut host = DECORATED_CATCH_HOST.replace("__ENTRY__", entry);
    for (name, marker) in markers {
        let symbol = deps
            .iter()
            .find_map(|dependency| match dependency {
                Value::Object(decl) if decl["source_name"] == Value::String(name.into()) => {
                    match &decl["symbol"] {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                _ => None,
            })
            .expect("expected bound dependency");
        host = host.replace(marker, symbol);
    }
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let code = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&code);
    let source = cwd.join("decorated-error.c");
    let exe = cwd.join(format!("decorated-error{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{}{host}",
            String::from_utf8(code.stdout).unwrap()
        ),
    )
    .unwrap();
    compile_projection(&source, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    let oom = execute(Command::new(exe).arg("oom"), cwd, 15);
    assert_eq!(oom.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&oom.stderr).trim(), "out of memory");
    fs::write(cwd.join("function.txt"), "Duplicate").unwrap();
    let duplicate = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(duplicate.status.code(), Some(1));
    assert!(duplicate.stdout.is_empty());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("linked_error_member_moved"));
}

pub(super) fn assert_local_string_lengths(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("local-string-lengths.ku"),
        r#"struct Measurement { scalars: int bytes: int }
fn Read(source: str): Measurement! {
    scalars = source.len()
    octets = source.byte_len()
    return ok(Measurement { scalars: scalars, bytes: octets })
}
fn ReadView(&source: str): Measurement! {
    scalars = source.len()
    octets = source.byte_len()
    return ok(Measurement { scalars: scalars, bytes: octets })
}
fn Refuse(source: int): int! {
    local = source.byte_len()
    return ok(local)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "local-string-lengths.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    for name in ["Read", "ReadView"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let code = String::from_utf8(emitted.stdout).unwrap();
        let (result, signature) = code
            .lines()
            .find_map(|line| line.split_once(" ku_linked_null_"))
            .unwrap();
        let entry = signature.split_once('(').unwrap().0;
        let host = r#"
#include <assert.h>
#include <string.h>
static void visit(const uint8_t *text, size_t len, int64_t scalars) {
    size_t cap = len ? len : 1;
    uint8_t *bytes = malloc(cap);
    assert(bytes);
    memcpy(bytes,text,len);
    KuProjectionString value = {bytes,len,cap,KU_PROJECTION_STRING_OWNED};
#if __VIEW__
    __RESULT__ result = __ENTRY__(&value);
    assert(value.ptr == bytes && !memcmp(value.ptr,text,len));
    free(bytes);
#else
    __RESULT__ result = __ENTRY__(value);
#endif
    assert(result.ok && result.value.f0 == scalars && result.value.f1 == (int64_t)len);
}
int main(void) {
    static const uint8_t ascii[] = {0x61,0x62,0x63};
    static const uint8_t mixed[] = {0x61,0xe4,0xb8,0xad,0xf0,0x9f,0x99,0x82};
    visit(ascii,0,0);
    visit(ascii,sizeof(ascii),3);
    visit(mixed,sizeof(mixed),3);
    return 0;
}
"#
        .replace("__RESULT__", result)
        .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
        .replace("__VIEW__", if name == "ReadView" { "1" } else { "0" });
        let source = cwd.join("local-string-lengths.c");
        let exe = cwd.join(format!(
            "local-string-lengths{}",
            std::env::consts::EXE_SUFFIX
        ));
        fs::write(&source, format!("{code}{host}")).unwrap();
        compile_projection(&source, &exe, cwd);
        assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    }
    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unsupported_linked_read"));
}

pub(super) fn assert_checked_copy_int(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("checked-int.ku"),
        r#"fn Copy(value: int, zero: bool, deny: bool): int! {
    if (deny) { fail { domain: "test", code: "denied", message: "int" } }
    if (zero) { return ok(0) }
    return ok(value)
}
fn Read(value: int): int! {
    out = Copy(value, false, false)?
    return ok(out)
}
fn Wrong(): int! { return ok("wrong") }
fn Indexed(&items: [int], slot: int): int { return items[slot] }
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "checked-int.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Copy").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let entry = text
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResult_int64_t "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
#include <assert.h>
int main(void) {
    KuBootstrapResult_int64_t zero = ENTRY(INT64_MIN, true, false);
    KuBootstrapResult_int64_t value = ENTRY(INT64_MIN, false, false);
    KuBootstrapResult_int64_t failure = ENTRY(INT64_MAX, false, true);
    assert(zero.ok && zero.value == 0);
    assert(value.ok && value.value == INT64_MIN);
    assert(!failure.ok && failure.error.code.len == 6 && failure.error.code.ptr[0] == 'd');
    return 0;
}
"#
    .replace("ENTRY", entry);
    let c = cwd.join("checked-int.c");
    let exe = cwd.join(format!("checked-int{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{text}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    fs::write(cwd.join("function.txt"), "Read").unwrap();
    let forwarded = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&forwarded);
    let text = String::from_utf8(forwarded.stdout).unwrap();
    assert_eq!(text.matches("} KuBootstrapResult_int64_t;").count(), 1);
    assert!(text.contains("extern KuBootstrapResult_int64_t"));
    fs::write(cwd.join("function.txt"), "Wrong").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_result_return"));

    fs::write(cwd.join("function.txt"), "Indexed").unwrap();
    let plain = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plain);
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(!text.contains("KuBootstrapResult_int64_t"));
    let entry = text
        .lines()
        .find_map(|line| line.strip_prefix("int64_t ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
#include <assert.h>
int main(int argc, char **argv) {
    (void)argv;
    int64_t values[] = {INT64_MIN, 0, INT64_MAX};
    KuArrayInt items = {3, values, 3};
    if (argc > 1) { (void)ENTRY(&items, 3); return 5; }
    for (int64_t index = 0; index < 3; ++index) {
        assert(ENTRY(&items, index) == values[index]);
    }
    assert(items.data == values && items.len == 3);
    return 0;
}
"#
    .replace("ENTRY", &format!("ku_linked_null_{entry}"));
    let c = cwd.join("plain-int.c");
    let exe = cwd.join(format!("plain-int{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{text}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    let bounds = execute(Command::new(&exe).arg("bounds"), cwd, 15);
    assert_eq!(bounds.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bounds.stderr).contains("index 3 out of bounds"));
    fs::write(
        cwd.join("wrong-plain.ku"),
        "fn WrongPlain(): int { return \"wrong\" }\n",
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "wrong-plain.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "WrongPlain").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("return_type"));
}

pub(super) fn assert_checked_str_predicate(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("predicate.ku"),
        r#"fn Project(&path: str, mode: int): str! {
    empty = ""
    return ok(empty)
}
fn Observe(): null! { return ok(null) }
fn Validate(&path: str, mode: int): null! {
    if (Project(path, mode)? != path) { Observe()? }
    return ok(null)
}
fn Contains(&hay: str, &needle: str): int! {
    if (hay.contains(needle)) { return ok(1) }
    return ok(0)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "predicate.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Validate\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null\n").unwrap();
    let output = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&output);
    let program = String::from_utf8(output.stdout).unwrap();
    let symbol = |prefix| {
        program
            .lines()
            .find_map(|line| line.strip_prefix(prefix)?.split_once('('))
            .map(|(name, _)| name)
            .expect("generated typed entry or callback")
    };
    let caller = r#"
static unsigned normalized, observed;
KuBootstrapResult_KuProjectionString __PROJECT__(const KuProjectionString* original, int64_t mode) {
    assert(original && original->len == 1u && original->ptr[0] == 'a');
    ++normalized;
    uint8_t* data = malloc(1u);
    assert(data);
    *data = mode == 0 ? 'a' : 'b';
    KuProjectionString allocated = {data, 1u, 1u, KU_PROJECTION_STRING_OWNED};
    if (mode == 2) {
        return (KuBootstrapResult_KuProjectionString){.error.message = allocated};
    }
    return (KuBootstrapResult_KuProjectionString){.ok = true, .value = allocated};
}
KuBootstrapResultNull __OBSERVE__(void) {
    assert(!outstanding());
    ++observed;
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    (void)test_realloc;
    const KuProjectionString path = {(uint8_t*)"a", 1u, 0u, KU_PROJECTION_STRING_STATIC};
    assert(__ENTRY__(&path, 0).ok && normalized == 1u && observed == 0u && !outstanding());
    assert(__ENTRY__(&path, 1).ok && normalized == 2u && observed == 1u && !outstanding());
    KuBootstrapResultNull failed = __ENTRY__(&path, 2);
    assert(!failed.ok && normalized == 3u && observed == 1u && outstanding() == 1);
    assert(failed.error.message.len == 1u && failed.error.message.ptr[0] == 'b');
    ku_projection_string_drop(&failed.error.message);
    assert(!outstanding());
    return 0;
}
"#
    .replace("__ENTRY__", symbol("KuBootstrapResultNull "))
    .replace(
        "__PROJECT__",
        symbol("extern KuBootstrapResult_KuProjectionString "),
    )
    .replace("__OBSERVE__", symbol("extern KuBootstrapResultNull "));
    let source = cwd.join("checked-predicate.c");
    let executable = cwd.join(format!("checked-predicate{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!("{CHECKED_ARRAY_ALLOCATOR}{program}{caller}"),
    )
    .unwrap();
    compile_projection(&source, &executable, cwd);
    assert_ok(&execute(&mut Command::new(executable), cwd, 10));

    fs::write(cwd.join("function.txt"), "Contains\n").unwrap();
    let output = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&output);
    let program = String::from_utf8(output.stdout).unwrap();
    let (entry, _) = program
        .lines()
        .find_map(|line| {
            line.strip_prefix("KuBootstrapResult_int64_t ")?
                .split_once('(')
        })
        .unwrap();
    let caller = r#"
#include <assert.h>
int main(void) {
    const KuProjectionString hay = {(uint8_t*)"a\0b", 3u, 0u, KU_PROJECTION_STRING_STATIC};
    const KuProjectionString needle = {(uint8_t*)"\0b", 2u, 0u, KU_PROJECTION_STRING_STATIC};
    const KuProjectionString miss = {(uint8_t*)"ab", 2u, 0u, KU_PROJECTION_STRING_STATIC};
    const KuProjectionString empty = {0};
    const KuBootstrapResult_int64_t hit = __ENTRY__(&hay, &needle);
    const KuBootstrapResult_int64_t absent = __ENTRY__(&hay, &miss);
    const KuBootstrapResult_int64_t vacuous = __ENTRY__(&empty, &empty);
    assert(hit.ok && hit.value == 1 && absent.ok && absent.value == 0);
    assert(vacuous.ok && vacuous.value == 1);
    return 0;
}
"#
    .replace("__ENTRY__", entry);
    let source = cwd.join("checked-contains.c");
    let executable = cwd.join(format!("checked-contains{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{program}{caller}")).unwrap();
    compile_projection(&source, &executable, cwd);
    assert_ok(&execute(&mut Command::new(executable), cwd, 10));
}
pub(super) fn assert_countdown_edges(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("countdown-edges.ku"),
        r#"struct Cursor { index: int }
fn CheckEdges(&edges: [int], first: int, count: int, index: int): Cursor! {
    slice_cursor = first
    slice_remaining = count
    parent_id = index + 1
    while (slice_remaining > 0) {
        child = edges[slice_cursor]
        if (child <= 0 || child >= parent_id) {
            fail { domain: "bootstrap.ast", code: "invalid_node_id", message: "invalid child" }
        }
        slice_cursor = slice_cursor + 1
        slice_remaining = slice_remaining - 1
    }
    return ok(Cursor { index: slice_cursor })
}
fn WrongStep(start: int): null! {
    remaining = start
    while (remaining > 0) { remaining = remaining + 1 }
    return ok(null)
}
fn NestedWrite(start: int): null! {
    remaining = start
    while (remaining > 0) {
        if (remaining == 2) { remaining = remaining + 2 }
        remaining = remaining - 1
    }
    return ok(null)
}
fn OutsideWrite(&items: [int], start: int): int! {
    total = start
    seen = false
    cursor = 0
    while (cursor < items.len()) {
        total = total + items[cursor]
        seen = true
        cursor = cursor + 1
    }
    if (seen) { total = total + 1 }
    return ok(total)
}
fn NestedIncrease(&edges: [int], start: int): null! {
    remaining = start
    while (remaining > 0) {
        cursor = 0
        while (cursor < edges.len()) {
            cursor = cursor - 1
            cursor = cursor + 1
        }
        remaining = remaining - 1
    }
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "countdown-edges.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "CheckEdges").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let definition = code
        .lines()
        .find(|line| line.contains(" ku_linked_null_"))
        .unwrap();
    let (result, entry) = definition.split_once(' ').unwrap();
    let entry = entry.split_once('(').unwrap().0;
    let host = r#"
int main(void) {
    int64_t data[] = {0, 1, 2, 4};
    KuArrayInt edges = {4, data, 4};
    __RESULT__ result = __ENTRY__(&edges, 1, 2, 2);
    assert(result.ok && result.value.f0 == 3);
    result = __ENTRY__(&edges, 3, 1, 2);
    assert(!result.ok && result.error.code.len == sizeof("invalid_node_id")-1);
    assert(!memcmp(result.error.code.ptr, "invalid_node_id", result.error.code.len));
    result = __ENTRY__(&edges, 3, 0, 2);
    assert(result.ok && result.value.f0 == 3);
    result = __ENTRY__(&edges, 3, INT64_MIN, 2);
    assert(result.ok && result.value.f0 == 3);
    assert(edges.data == data && edges.len == 4 && data[3] == 4);
    return 0;
}
"#
    .replace("__RESULT__", result)
    .replace("__ENTRY__", entry);
    let c = cwd.join("countdown-edges.c");
    let app = cwd.join(format!("countdown-edges{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &c,
        format!("#include <assert.h>\n#include <string.h>\n{code}{host}"),
    )
    .unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    fs::write(cwd.join("function.txt"), "OutsideWrite").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let entry = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResult_int64_t "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
#include <assert.h>
int main(void) {
    int64_t values[] = {2, 7};
    KuArrayInt full = {2, values, 2};
    const KuArrayInt empty = {0};
    assert(ENTRY(&full, 3).value == 13);
    assert(ENTRY(&empty, -3).value == -3);
    assert(full.data == values && values[1] == 7);
    return 0;
}
"#
    .replace("ENTRY", entry);
    fs::write(&c, format!("{code}{host}")).unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    for (function, diagnostic) in [
        ("WrongStep", "unsupported_linked_countdown"),
        ("NestedWrite", "linked_loop_cursor_write"),
        ("NestedIncrease", "linked_loop_cursor_write"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}

pub(super) fn assert_named_counted_bound(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("counted-int-bound.ku"),
        r#"struct CountResult { value: int }
fn Count(&available: [int], skip: int): CountResult! {
    work_limit = available.len() - skip
    cursor = 0
    while (cursor < work_limit) { cursor = cursor + 1 }
    return ok(CountResult { value: cursor })
}
fn CountSeparated(&available: [int], skip: int): CountResult! {
    cursor = 0
    work_limit = available.len() - skip
    observed = skip
    while (cursor < work_limit) {
        if (observed < 0) { fail { domain: "fixture", code: "negative", message: "negative skip" } }
        cursor = cursor + 1
    }
    return ok(CountResult { value: cursor })
}
fn CountWhileAvailable(&available: [int], skip: int): CountResult! {
    work_limit = available.len() - skip
    cursor = 0
    while (cursor < work_limit && available[cursor] > 0) { cursor = cursor + 1 }
    return ok(CountResult { value: cursor })
}
fn MutableBound(start: int): null! {
    work_limit = start
    cursor = 0
    while (cursor < work_limit) {
        if (cursor == 1) { work_limit = work_limit + 1 }
        cursor = cursor + 1
    }
    return ok(null)
}
fn MutableCursor(start: int): null! {
    work_limit = start
    cursor = 0
    while (cursor < work_limit) {
        if (cursor == 1) { cursor = cursor - 1 }
        cursor = cursor + 1
    }
    return ok(null)
}
fn ImmutableWrite(): int! {
    COUNT = 1
    COUNT = 2
    return ok(COUNT)
}
fn ImmutableCursor(limit: int): int! {
    COUNT = 0
    while (COUNT < limit) { COUNT = COUNT + 1 }
    return ok(COUNT)
}
fn SameBinding(): null! {
    cursor = 0
    while (cursor < cursor) { cursor = cursor + 1 }
    return ok(null)
}
fn InterveningWrite(start: int): null! {
    remaining = start
    while (remaining > 0) {
        cursor = 0
        work_limit = start
        if (remaining > 1) { cursor = cursor + 1 }
        while (cursor < work_limit) { cursor = cursor + 1 }
        remaining = remaining - 1
    }
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "counted-int-bound.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let host = r#"
int main(void) {
    int64_t data[] = {1, 2, 3};
    KuArrayInt available = {3, data, 3};
    __RESULT__ result = __ENTRY__(&available, 1);
    assert(result.ok && result.value.f0 == 2);
    result = __ENTRY__(&available, 3);
    assert(result.ok && result.value.f0 == 0);
    result = __ENTRY__(&available, INT64_MAX);
    assert(result.ok && result.value.f0 == 0);
    const KuArrayInt empty = {0};
    result = __ENTRY__(&empty, 0);
    assert(result.ok && result.value.f0 == 0);
    assert(available.data == data && available.len == 3);
    return 0;
}
"#;
    let c = cwd.join("counted-int-bound.c");
    let app = cwd.join(format!("counted-int-bound{}", std::env::consts::EXE_SUFFIX));
    for function in ["Count", "CountSeparated", "CountWhileAvailable"] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let code = String::from_utf8(emitted.stdout).unwrap();
        let definition = code
            .lines()
            .find(|line| line.contains(" ku_linked_null_"))
            .unwrap();
        let (result, entry) = definition.split_once(' ').unwrap();
        let entry = entry.split_once('(').unwrap().0;
        let caller = host
            .replace("__RESULT__", result)
            .replace("__ENTRY__", entry);
        fs::write(&c, format!("#include <assert.h>\n{code}{caller}")).unwrap();
        compile_projection(&c, &app, cwd);
        assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    }
    for (function, diagnostic) in [
        ("MutableBound", "linked_loop_bound_write"),
        ("MutableCursor", "linked_loop_cursor_write"),
        ("SameBinding", "linked_loop_bound_cursor"),
        ("InterveningWrite", "linked_loop_initial_write"),
        ("ImmutableWrite", "linked_immutable_binding"),
        ("ImmutableCursor", "linked_immutable_binding"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
    assert_named_counted_start(seed, cwd);
}

fn assert_named_counted_start(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("counted-int-start.ku"),
        r#"struct CountResult { value: int }
fn Visit(value: int): null! { return ok(null) }
fn CountFrom(start: int, stop: int): CountResult! {
    cursor = start
    while (cursor < stop) {
        Visit(cursor)?
        cursor = cursor + 1
    }
    return ok(CountResult { value: cursor })
}
fn LegacyStart(&values: [int], start: int): CountResult! {
    cursor = start
    while (cursor < values.len()) { cursor = cursor + 1 }
    return ok(CountResult { value: cursor })
}
fn StrideFrom(start: int, stop: int): CountResult! {
    cursor = start + 2
    while (cursor < stop) {
        Visit(cursor)?
        cursor = cursor + 2
    }
    return ok(CountResult { value: cursor })
}
fn LegacyStride(&values: [int]): CountResult! {
    cursor = 0
    while (cursor < values.len()) { cursor = cursor + 2 }
    return ok(CountResult { value: cursor })
}
fn ZeroStride(start: int, stop: int): CountResult! {
    cursor = start + 2
    while (cursor < stop) { cursor = cursor + 0 }
    return ok(CountResult { value: cursor })
}
fn ChangesBound(start: int, stop: int): CountResult! {
    bound = stop
    cursor = start + 2
    while (cursor < bound) {
        bound = bound + 1
        cursor = cursor + 2
    }
    return ok(CountResult { value: cursor })
}
fn MovesOwner(start: int, stop: int, input: str): CountResult! {
    cursor = start + 2
    while (cursor < stop) {
        moved = input
        cursor = cursor + 2
    }
    return ok(CountResult { value: cursor })
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "counted-int-start.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "CountFrom").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let definition = code
        .lines()
        .find(|line| line.contains(" ku_linked_null_"))
        .unwrap();
    let (result, entry) = definition.split_once(' ').unwrap();
    let entry = entry.split_once('(').unwrap().0;
    let visit = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
static int64_t observed[8];
static size_t called;
KuBootstrapResultNull __VISIT__(int64_t value) {
    assert(called < sizeof(observed)/sizeof(observed[0]));
    observed[called++] = value;
    if (value == 6) return (KuBootstrapResultNull){.error = {
        .code = {(uint8_t*)"stop",4,0,KU_PROJECTION_STRING_STATIC}}};
    return (KuBootstrapResultNull){.ok = true};
}
int main(void) {
    __RESULT__ result = __ENTRY__(-2, 0);
    assert(result.ok && result.value.f0 == 0 && called == 2);
    assert(observed[0] == -2 && observed[1] == -1);
    result = __ENTRY__(-1, -2);
    assert(result.ok && result.value.f0 == -1 && called == 2);
    result = __ENTRY__(INT64_MAX-1, INT64_MAX);
    assert(result.ok && result.value.f0 == INT64_MAX && called == 3);
    assert(observed[2] == INT64_MAX-1);
    result = __ENTRY__(5, 8);
    assert(!result.ok && result.error.code.len == 4 && called == 5);
    assert(observed[3] == 5 && observed[4] == 6);
    return 0;
}
"#
    .replace("__RESULT__", result)
    .replace("__ENTRY__", entry)
    .replace("__VISIT__", visit);
    let c = cwd.join("counted-int-start.c");
    let app = cwd.join(format!("counted-int-start{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));

    fs::write(cwd.join("function.txt"), "LegacyStart").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("requires initialization to zero"));
    assert_named_counted_stride(seed, cwd);
}

fn assert_named_counted_stride(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("function.txt"), "StrideFrom").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let definition = code
        .lines()
        .find(|line| line.contains(" ku_linked_null_"))
        .unwrap();
    let (result, entry) = definition.split_once(' ').unwrap();
    let entry = entry.split_once('(').unwrap().0;
    let visit = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResultNull "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
static int64_t observed[5];
static size_t called, expect_calls;
KuBootstrapResultNull __VISIT__(int64_t value) {
    assert(called < sizeof(observed)/sizeof(observed[0]));
    observed[called++] = value;
    if (value == 6) return (KuBootstrapResultNull){.error = {
        .code = {(uint8_t*)"stop",4,0,KU_PROJECTION_STRING_STATIC}}};
    return (KuBootstrapResultNull){.ok = true};
}
static void check_overflow(void) {
    if (called != expect_calls || (called && observed[0] != INT64_MAX-1)) _Exit(80);
}
int main(int argc, char **argv) {
    if (argc > 1) {
        expect_calls = argv[1][0] == 's';
        assert(atexit(check_overflow) == 0);
        int64_t start = expect_calls ? INT64_MAX-3 : INT64_MAX-1;
        (void)__ENTRY__(start,INT64_MAX);
        return 81;
    }
    __RESULT__ result = __ENTRY__(-5,0);
    assert(result.ok && result.value.f0 == 1 && called == 2);
    assert(observed[0] == -3 && observed[1] == -1);
    result = __ENTRY__(4,6);
    assert(result.ok && result.value.f0 == 6 && called == 2);
    result = __ENTRY__(2,8);
    assert(!result.ok && result.error.code.len == 4 && called == 4);
    assert(observed[2] == 4 && observed[3] == 6);
    return 0;
}
"#
    .replace("__RESULT__", result)
    .replace("__ENTRY__", entry)
    .replace("__VISIT__", visit);
    let c = cwd.join("counted-int-stride.c");
    let app = cwd.join(format!(
        "counted-int-stride{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&c, format!("#include <assert.h>\n{code}{host}")).unwrap();
    compile_projection(&c, &app, cwd);
    assert_ok(&execute(&mut Command::new(&app), cwd, 15));
    for overflow in ["offset", "step"] {
        let trapped = execute(Command::new(&app).arg(overflow), cwd, 15);
        assert_eq!(trapped.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&trapped.stderr).trim(),
            "integer overflow"
        );
    }
    for (function, diagnostic) in [
        ("LegacyStride", "unsupported_linked_loop"),
        ("ZeroStride", "unsupported_linked_loop"),
        ("ChangesBound", "linked_loop_bound_write"),
        ("MovesOwner", "linked_control_move"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        let error = String::from_utf8_lossy(&rejected.stderr);
        assert!(error.contains(diagnostic), "{diagnostic}, got {error}");
    }
}

pub(super) fn assert_guarded_copy_field_set(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("guarded-copy-field.ku"),
        r#"struct Parcel { id: int label: str active: bool }
struct Holder { box: Parcel }

fn Next(&parcel: Parcel, stop: bool): int! { return ok(parcel.id + 1) }
fn Take(parcel: Parcel): int! { return ok(parcel.id) }

fn Run(input: Parcel, changed: bool, stop: bool): Parcel! {
    value = input
    if (changed) {
        value.id = Next(value, stop)?
    } else {
        value.id = value.id + 2
    }
    return ok(value)
}

fn Recover(input: Parcel, changed: bool, stop: bool): Parcel! {
    value = input
    try {
        if (changed) { value.id = Next(value, stop)? }
        return ok(value)
    } catch (error) {
        return ok(value)
    }
}

fn BadParameter(input: Parcel, changed: bool): Parcel! {
    if (changed) { input.id = 1 }
    return ok(input)
}
fn BadView(&input: Parcel, changed: bool): int! {
    if (changed) { input.id = 1 }
    return ok(input.id)
}
fn BadNested(input: Holder, changed: bool): Holder! {
    value = input
    if (changed) { value.box.id = 1 }
    return ok(value)
}
fn BadString(input: Parcel, replacement: str, changed: bool): Parcel! {
    value = input
    if (changed) { value.label = replacement }
    return ok(value)
}
fn SetFlag(input: Parcel, changed: bool, flag: bool): Parcel! {
    value = input
    if (changed) { value.active = flag }
    return ok(value)
}
fn BadType(input: Parcel, changed: bool): Parcel! {
    value = input
    if (changed) { value.id = false }
    return ok(value)
}
fn BadMove(input: Parcel, changed: bool): Parcel! {
    value = input
    if (changed) { value.id = Take(value)? }
    return ok(value)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "guarded-copy-field.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();

    for name in ["Run", "Recover", "SetFlag"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let code = String::from_utf8(emitted.stdout).unwrap();
        let next = code
            .lines()
            .find_map(|line| line.strip_prefix("extern KuBootstrapResult_int64_t "))
            .and_then(|signature| signature.split_once('('))
            .map_or("unused_checked_next", |(symbol, _)| symbol);
        let (result, signature) = code
            .lines()
            .find_map(|line| line.split_once(" ku_linked_null_"))
            .unwrap();
        let (entry, args) = signature.split_once('(').unwrap();
        let record = args.split_once(" a0").unwrap().0;
        let host = r#"
static uint8_t *original;
#if !__FLAG__
static unsigned calls;

KuBootstrapResult_int64_t __NEXT__(const __RECORD__ *parcel, bool stop) {
    ++calls;
    assert(parcel->f0 == 10 && parcel->f1.ptr == original && *original == 5 && parcel->f2);
    if (stop) {
        uint8_t *message = malloc(1);
        *message = 7;
        return (KuBootstrapResult_int64_t){.error={.message={message,1,1,KU_PROJECTION_STRING_OWNED}}};
    }
    return (KuBootstrapResult_int64_t){.ok=true,.value=11};
}
#endif

int main(void) {
    (void)test_realloc;
#if __FLAG__
    for (int apply = 0; apply < 2; ++apply) {
        for (int flag = 0; flag < 2; ++flag) {
            original = malloc(1);
            *original = 5;
            __RECORD__ input = {10,{original,1,1,KU_PROJECTION_STRING_OWNED},true};
            __RESULT__ result = __ENTRY__(input, apply, flag);
            assert(result.ok && result.value.f0 == 10 && result.value.f2 == (!apply || flag));
            assert(result.value.f1.ptr == original && *original == 5 && outstanding() == 1);
            ku_projection_string_drop(&result.value.f1);
            assert(!outstanding());
        }
    }
#else
    for (int i = 0; i < 3; ++i) {
        bool changed = i != 0, stop = i != 1;
        calls = 0;
        original = malloc(1);
        *original = 5;
        __RECORD__ input = {10,{original,1,1,KU_PROJECTION_STRING_OWNED},true};
        __RESULT__ result = __ENTRY__(input, changed, stop);
        assert(calls == (unsigned)changed && outstanding() == 1);
        if (changed && stop && !__RECOVER__) {
            assert(!result.ok && result.error.message.len == 1 && *result.error.message.ptr == 7);
            ku_projection_string_drop(&result.error.message);
        } else {
            int expected = !changed ? (__RECOVER__ ? 10 : 12) : (stop ? 10 : 11);
            assert(result.ok && result.value.f0 == expected && result.value.f2);
            assert(result.value.f1.ptr == original && *original == 5);
            ku_projection_string_drop(&result.value.f1);
        }
        assert(!outstanding());
    }
#endif
    return 0;
}
"#
        .replace("__ENTRY__", &format!("ku_linked_null_{entry}"))
        .replace("__RECORD__", record)
        .replace("__RESULT__", result)
        .replace("__NEXT__", next)
        .replace("__FLAG__", if name == "SetFlag" { "1" } else { "0" })
        .replace("__RECOVER__", if name == "Recover" { "1" } else { "0" });
        let source = cwd.join("guarded-copy-field.c");
        let exe = cwd.join(format!(
            "guarded-copy-field{}",
            std::env::consts::EXE_SUFFIX
        ));
        fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
        compile_projection(&source, &exe, cwd);
        assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    }

    for (name, diagnostic) in [
        ("BadParameter", "linked_member_target"),
        ("BadView", "linked_member_target"),
        ("BadNested", "linked_member_target"),
        ("BadString", "linked_member_target"),
        ("BadType", "linked_member_rebind_type"),
        ("BadMove", "unavailable_linked_binding"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}

pub(super) fn assert_owned_local_member_set(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("member-set.ku"),
        r#"struct Entry { label: str number: int }
fn Update(input: Entry, text: str): Entry! {
    slot = input.clone()
    slot.label = text.clone()
    slot.number = 42
    return ok(slot)
}

fn Refuse(input: Entry): Entry! {
    input.label = "new"
    return ok(input)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "member-set.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Update").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, args) = signature.split_once('(').unwrap();
    let (carrier, _) = args.split_once(" a0").unwrap();
    let host = r#"
static KuProjectionString owned_text(const char *value) {
    size_t len = strlen(value);
    uint8_t *data = malloc(len);
    memcpy(data, value, len);
    return (KuProjectionString){data,len,len,KU_PROJECTION_STRING_OWNED};
}
static void check_failed_assignment(void) {
    if (outstanding() || attempts != 4) _Exit(82);
}
int main(int argc, char **argv) {
    (void)test_realloc;
    (void)argv;
    ITEM item = {owned_text("old"), -3};
    KuProjectionString replacement = owned_text("new");
    if (argc > 1) {
        assert(!atexit(check_failed_assignment));
        // The old record is successfully cloned. Preparing the replacement
        // fails while the old field and both original input owners are live.
        fail_after = 1;
    }
    RESULT result = ENTRY(item, replacement);
    assert(result.ok && result.value.f1 == 42 && !memcmp(result.value.f0.ptr, "new", 3));
    assert(outstanding() == 1);
    ku_projection_string_drop(&result.value.f0);
    assert(outstanding() == 0);
    return 0;
}
"#
    .replace("RESULT", result)
    .replace("ENTRY", &format!("ku_linked_null_{entry}"))
    .replace("ITEM", carrier);
    let path = cwd.join("owned-local-member-set.c");
    let binary = cwd.join(format!(
        "owned-local-member-set{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&path, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 10));
    let failed = execute(Command::new(binary).arg("replacement-clone"), cwd, 10);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&failed.stderr).trim(),
        "out of memory"
    );

    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_member_target"));
}

pub(super) fn assert_owned_int_array_store(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("int-array-store.ku"),
        r#"struct IntStore { values: [int] }
fn Replacement(input: int): int! { return ok(input) }
fn Store(index: int, divisor: int, input: int): IntStore! {
    values: [int] = [5, 9]
    limit = 1
    step = 0
    while (step < limit) {
        values[index / divisor] = Replacement(input)?
        step = step + 1
    }
    return ok(IntStore { values: values })
}
fn Refuse(&values: [int]): null! {
    values[0] = 1
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "int-array-store.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Store").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let definition = code
        .lines()
        .find(|line| line.contains(" ku_linked_null_"))
        .unwrap();
    let (result, entry) = definition.split_once(' ').unwrap();
    let entry = entry.split_once('(').unwrap().0;
    let (dependency_type, dependency) = code
        .lines()
        .find_map(|line| line.strip_prefix("extern "))
        .unwrap()
        .split_once(' ')
        .unwrap();
    let dependency = dependency.split_once('(').unwrap().0;
    let host = r#"
static size_t seen;
DEPENDENCY_TYPE DEPENDENCY(int64_t input) {
    ++seen;
    if (input < 0) return (DEPENDENCY_TYPE){.error={.code={(uint8_t*)"rhs",3,0,KU_PROJECTION_STRING_STATIC}}};
    return (DEPENDENCY_TYPE){.ok=true,.value=input};
}
static void check_no_owned_array(void) {
    if (outstanding() || seen != 1) _Exit(83);
}
int main(int argc, char **argv) {
    (void)test_malloc;
    (void)argv;
    if (argc > 1) {
        assert(!atexit(check_no_owned_array));
        (void) ENTRY(2, 1, 7);
        return 9;
    }
    RESULT result = ENTRY(1, 1, 7);
    assert(result.ok && result.value.f0.len == 2 && result.value.f0.data[0] == 5 && result.value.f0.data[1] == 7);
    assert(seen == 1 && outstanding() == 1);
    free(result.value.f0.data);
    result = ENTRY(1, 0, -1);
    assert(!result.ok && result.error.code.len == 3 && !memcmp(result.error.code.ptr, "rhs", 3));
    assert(seen == 2 && outstanding() == 0);
    return 0;
}
"#
    .replace("DEPENDENCY_TYPE", dependency_type)
    .replace("DEPENDENCY", dependency)
    .replace("ENTRY", entry)
    .replace("RESULT", result);
    let path = cwd.join("owned-int-array-store.c");
    let binary = cwd.join(format!(
        "owned-int-array-store{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&path, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 10));
    let invalid = execute(Command::new(binary).arg("out-of-bounds"), cwd, 10);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("index_out_of_bounds"));

    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_index_target"));
}

const FIELD_REBIND_SOURCE: &str = r#"struct Alias { text: str remaining: str }
fn Acquire(): Alias! { return ok(Alias { text: "new", remaining: "keep" }) }
fn Inspect(&remaining: str): null! { return ok(null) }

fn ReplaceField(old: str, use_alias: bool): str! {
    local = old
    if (use_alias) {
        alias = Acquire()?
        local = alias.text
        Inspect(alias.remaining)?
    }
    return ok(local)
}

fn BorrowField(&alias: Alias, old: str): str! {
    local = old
    local = alias.text
    return ok(local)
}
"#;

const FIELD_REBIND_DRIVER: &str = r#"
static uintptr_t prior, text, remaining;
static bool fail_acquire, fail_inspect;
static unsigned acquired, inspected;

static KuProjectionString owned(char value, uintptr_t *identity) {
    uint8_t *data = malloc(1);
    assert(data);
    *data = (uint8_t)value;
    *identity = (uintptr_t)data;
    return (KuProjectionString){data, 1, 1, KU_PROJECTION_STRING_OWNED};
}

static bool retains(uintptr_t identity) {
    for (size_t i = 0; i < sizeof(live)/sizeof(live[0]); ++i)
        if (live[i] && (uintptr_t)live[i] == identity) return true;
    return false;
}

__RESULT__ __ACQUIRE__(void) {
    ++acquired;
    assert(retains(prior));
    if (fail_acquire) {
        __RESULT__ error = {0};
        error.error.code = (KuProjectionString){
            (uint8_t*)"acquire", 7, 0, KU_PROJECTION_STRING_STATIC
        };
        return error;
    }
    __ELEMENT__ value = {0};
    value.f0 = owned('n', &text);
    value.f1 = owned('k', &remaining);
    return (__RESULT__){.ok = true, .value = value};
}

KuBootstrapResultNull __INSPECT__(const KuProjectionString *field) {
    ++inspected;
    assert((uintptr_t)field->ptr == remaining && field->ptr[0] == 'k');
    assert(!retains(prior) && retains(text) && retains(remaining));
    if (fail_inspect) {
        KuBootstrapResultNull error = {0};
        error.error.code = (KuProjectionString){
            (uint8_t*)"inspect", 7, 0, KU_PROJECTION_STRING_STATIC
        };
        return error;
    }
    return (KuBootstrapResultNull){.ok = true};
}

int main(void) {
    (void)test_realloc;
    KuBootstrapResult_KuProjectionString result = __ENTRY__(owned('o', &prior), false);
    assert(result.ok && (uintptr_t)result.value.ptr == prior && acquired == 0 && inspected == 0);
    assert(attempts == 1 && outstanding() == 1);
    test_free(result.value.ptr);
    assert(!outstanding());

    attempts = 0;
    result = __ENTRY__(owned('o', &prior), true);
    assert(result.ok && (uintptr_t)result.value.ptr == text && result.value.ptr[0] == 'n');
    assert(acquired == 1 && inspected == 1 && attempts == 3 && outstanding() == 1);
    test_free(result.value.ptr);
    assert(!outstanding());

    attempts = 0;
    fail_acquire = true;
    result = __ENTRY__(owned('o', &prior), true);
    assert(!result.ok && result.error.code.len == 7 && !memcmp(result.error.code.ptr, "acquire", 7));
    assert(acquired == 2 && inspected == 1 && attempts == 1 && !outstanding());

    attempts = 0;
    fail_acquire = false;
    fail_inspect = true;
    result = __ENTRY__(owned('o', &prior), true);
    assert(!result.ok && result.error.code.len == 7 && !memcmp(result.error.code.ptr, "inspect", 7));
    assert(acquired == 3 && inspected == 2 && attempts == 3 && !outstanding());
    return 0;
}
"#;

pub(super) fn assert_record_field_string_rebind(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("record-field-rebind.ku"), FIELD_REBIND_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "record-field-rebind.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "ReplaceField\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null\n").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let source = String::from_utf8(emitted.stdout).unwrap();
    let symbol = |prefix: &str| {
        source
            .lines()
            .find_map(|line| line.strip_prefix(prefix)?.split_once('('))
            .map(|(name, _)| name)
            .expect("generated typed function")
    };
    let carrier = source
        .lines()
        .find_map(|line| {
            let carrier = line.strip_prefix("extern ")?.split_once(' ')?.0;
            carrier
                .starts_with("KuBootstrapResult_KuStruct_")
                .then_some(carrier)
        })
        .expect("owning record Result declaration");
    let host = FIELD_REBIND_DRIVER
        .replace("__RESULT__", carrier)
        .replace(
            "__ELEMENT__",
            carrier.strip_prefix("KuBootstrapResult_").unwrap(),
        )
        .replace("__ACQUIRE__", symbol(&format!("extern {carrier} ")))
        .replace("__INSPECT__", symbol("extern KuBootstrapResultNull "))
        .replace("__ENTRY__", symbol("KuBootstrapResult_KuProjectionString "));
    let path = cwd.join("record-field-rebind.c");
    let binary = cwd.join(format!(
        "record-field-rebind{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{source}{host}")).unwrap();
    compile_projection(&path, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 10));

    fs::write(cwd.join("function.txt"), "BorrowField\n").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("|linked_rebind_type|"));
}

pub(super) fn assert_guarded_record_field_append(seed: &Path, cwd: &Path) {
    let fixture = r#"struct Item { text: str }
struct Bag { items: [Item] }
fn Append(include: bool): int! {
    items: [Item] = []
    bag = Bag { items: items }
    if (include) {
        bag.items = bag.items.push(Item { text: str(8) })
    }
    return ok(bag.items.len())
}
"#;
    fs::write(cwd.join("field-append.ku"), fixture).unwrap();
    fs::write(cwd.join("modules.txt"), "field-append.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Append").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let entry = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResult_int64_t "))
        .and_then(|line| line.split_once('('))
        .unwrap()
        .0;
    let host = r#"
static int fail_on;
static void verify_oom(void) {
    if (outstanding() || attempts != fail_on) _Exit(94);
}
int main(int argc, char **argv) {
    (void)test_realloc;
    if (argc == 2) {
        fail_on = argv[1][0] - '0';
        fail_after = fail_on - 1;
        assert(atexit(verify_oom) == 0);
        (void)__ENTRY__(true);
        return 95;
    }
    KuBootstrapResult_int64_t result = __ENTRY__(false);
    assert(result.ok && result.value == 0 && !outstanding() && !attempts);
    result = __ENTRY__(true);
    assert(result.ok && result.value == 1 && !outstanding() && attempts == 2);
    return 0;
}
"#
    .replace("__ENTRY__", entry);
    let source = cwd.join("field-append-owning.c");
    let binary = cwd.join(format!("field-append-owning{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 15));
    for (point, expected) in [("1", "out of memory"), ("2", "array allocation failed")] {
        let failed = execute(Command::new(&binary).arg(point), cwd, 15);
        assert_eq!(failed.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&failed.stderr).trim(), expected);
    }
}

pub(super) fn assert_local_append_thunk(seed: &Path, cwd: &Path) {
    let body = r#"struct Location { line: int }
struct Entry { kind: str, text: str, score: int, location: Location, first: int }
struct ClosingToken { kind: str, lexeme: str, score: int }

fn Run(first: str, second: str, third: str, fourth: str, choices: [int], more: [int], reject: bool): int! {
    nodes: [Entry] = []
    edges: [int] = []
    cursor = 7
    fn Append(kind: str, text: str, score: int, location: Location, children: [int]): int! {
        if (reject && score < 0) { fail { domain: kind.clone(), code: kind, message: text } }
        offset = edges.len()
        for edge in children { edges = edges.push(edge) }
        item = Entry { kind: kind, text: text, score: score + cursor, location: location, first: offset }
        nodes = nodes.push(item)
        return ok(nodes.len() + cursor)
    }
    fn Advance(): null! {
        edges = edges.push(cursor)
        cursor = cursor + 1
        return ok(null)
    }
    fn Close(token: ClosingToken): null! {
        Advance()?
        added = Append(token.kind, token.lexeme, token.score, Location { line: 5 }, [cursor, cursor, cursor])?
        edges = edges.push(added)
        cursor = added
        return ok(null)
    }
    left = Append(first, second, 3, Location { line: 2 }, choices)?
    cursor = 8
    Close(ClosingToken { kind: third, lexeme: fourth, score: 0 - 5 })?
    return ok(left * 1000 + cursor * 100 + nodes.len() + edges.len() + cursor)
}
"#;
    let file = cwd.join("append-thunk.ku");
    fs::write(&file, body).unwrap();
    fs::write(cwd.join("modules.txt"), "append-thunk.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Run").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let positive = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&positive);
    let code = String::from_utf8(positive.stdout).unwrap();
    let entry = code
        .lines()
        .find_map(|line| line.strip_prefix("KuBootstrapResult_int64_t ku_linked_null_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    assert!(code.contains("_append_move(&(*capture->"));
    assert!(code.contains("&environment"));
    assert!(code.contains("(capture->f"));
    assert!(!code.contains("memcpy(copy, (*capture"));
    let host = r#"
static int failure_attempt;
static void after_nested_failure(void) {
    assert(attempts == failure_attempt);
    assert(!outstanding());
    fputs("allocator: cleared every scope\n", stderr);
}

static KuProjectionString owned(uint8_t marker) {
    uint8_t *data = malloc(1);
    *data = marker;
    return (KuProjectionString){data, 1, 1, KU_PROJECTION_STRING_OWNED};
}
static KuArrayInt children(size_t len, int64_t first) {
    int64_t *data = malloc(len * sizeof(*data));
    for (size_t i = 0; i < len; ++i) data[i] = first + (int64_t)i;
    return (KuArrayInt){len, data, len};
}
static KuBootstrapResult_int64_t run(bool reject, int allocation_before_failure) {
    KuProjectionString a = owned('A'), b = owned('B'), c = owned('C'), d = owned('D');
    KuArrayInt left = children(7, 1), right = children(3, 8);
    if (allocation_before_failure >= 0) {
        fail_after = allocation_before_failure;
        failure_attempt = attempts + allocation_before_failure + 1;
        assert(atexit(after_nested_failure) == 0);
    }
    return ENTRY(a, b, c, d, left, right, reject);
}
int main(int argc, char **argv) {
    if (argc > 1) {
        (void)run(false, !strcmp(argv[1], "fail-snapshot") ? 4 : 5);
        return 2;
    }
    KuBootstrapResult_int64_t first = run(false, -1);
    assert(first.ok && first.value == 9125 && !outstanding());
    KuBootstrapResult_int64_t next = run(true, -1);
    assert(!next.ok && next.error.domain.len == 1 && *next.error.domain.ptr == 'C');
    assert(next.error.code.len == 1 && *next.error.code.ptr == 'C');
    assert(next.error.message.len == 1 && *next.error.message.ptr == 'D');
    assert(outstanding() == 3);
    ku_projection_string_drop(&next.error.domain);
    ku_projection_string_drop(&next.error.code);
    ku_projection_string_drop(&next.error.message);
    assert(!outstanding());
    KuBootstrapResult_int64_t last = run(false, -1);
    assert(last.ok && last.value == 9125 && !outstanding());
    return 0;
}
"#
    .replace("ENTRY", &format!("ku_linked_null_{entry}"));
    let source = cwd.join("append-thunk.c");
    let program = cwd.join(format!("append-thunk{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&source, &program, cwd);
    assert_ok(&execute(&mut Command::new(&program), cwd, 10));
    for (fault, expected) in [
        ("fail-second-growth", "array allocation failed"),
        ("fail-snapshot", "out of memory"),
    ] {
        let failed = execute(Command::new(&program).arg(fault), cwd, 10);
        assert_eq!(failed.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&failed.stderr).trim(),
            format!("{expected}\nallocator: cleared every scope")
        );
    }

    for (from, to, diagnostic) in [
        (
            "edges = edges.push(edge)",
            "edges = []",
            "linked_capture_rebind",
        ),
        (
            "offset = edges.len()",
            "moved: [int] = edges\n        offset = moved.len()",
            "linked_callback_owner",
        ),
        (
            "Advance()?",
            "wrong = Advance()?",
            "linked_dependency_result",
        ),
        ("Advance()?", "Advance(9)?", "linked_local_append_arity"),
        (
            "Advance()?\n        added",
            "alias = Advance\n        Advance()?\n        added",
            "unavailable_linked_binding",
        ),
        (
            "Advance()?\n        added",
            "Close(token)?\n        Advance()?\n        added",
            "unsupported_linked_local_capture",
        ),
        (
            "Close(ClosingToken { kind: third, lexeme: fourth, score: 0 - 5 })?",
            "Close(0)?",
            "linked_argument_type",
        ),
    ] {
        assert!(body.contains(from));
        fs::write(&file, body.replace(from, to)).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }
}
