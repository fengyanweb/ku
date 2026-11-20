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

pub(super) fn assert_nested_array_result_cleanup(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("nested-provider.ku"),
        r#"struct Storage { labels: [str] offsets: [int] }
struct Entry { storage: Storage }
fn Read(flag: int): [Entry]! { return ok([]) }
fn ReadOne(flag: int): Entry! { fail { domain: "fixture", code: "empty", message: "empty" } }
fn ReadPath(path: str, &prefix: str): Entry! { fail { domain: "fixture", code: "empty", message: path } }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("nested-consumer.ku"),
        r#"import { Entry, Storage, Read as Make, ReadOne } from "./nested-provider.ku"
struct Other { storage: Storage }
fn Tail(flag: int): Entry! { return ReadOne(flag) }
fn WrongTail(flag: int): Other! { return ReadOne(flag) }
fn InspectRecord(flag: int): null! {
    first = ReadOne(0)?
    second = ReadOne(flag)?
    return ok(null)
}
fn Inspect(flag: int): null! {
    first = Make(0)?
    second = Make(flag)?
    return ok(null)
}
fn RejectClone(): null! {
    entries = Make(0)?
    entry = entries[0].clone()
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "nested-consumer.ku\nnested-provider.ku\n",
    )
    .unwrap();
    let NestedPlan {
        entry,
        dependency: dependency_name,
        dependency_result: carrier,
        code,
        ..
    } = nested_plan(seed, cwd, "Inspect", "StructArray");

    // A C ABI witness fills the declared nested carrier until managed Ku
    // emission supports the provider's owning construction end to end.
    let host = r#"
__RESULT__ __DEPENDENCY__(int64_t flag) {
    __RESULT__ result = {0};
    if (flag < 0) return result;
    result.value.len = 1;
    result.value.capacity = 2;
    result.value.data = malloc(2 * sizeof(*result.value.data));
    memset(result.value.data + 1, 0xff, sizeof(*result.value.data));
    result.value.data[0].f0.f0.len = 1;
    result.value.data[0].f0.f0.capacity = 1;
    result.value.data[0].f0.f0.data = malloc(sizeof(KuProjectionString));
    uint8_t *owned = malloc(1);
    *owned = 'a';
    result.value.data[0].f0.f0.data[0] = (KuProjectionString){owned,1,1,KU_PROJECTION_STRING_OWNED};
    result.value.data[0].f0.f1.len = 1;
    result.value.data[0].f0.f1.capacity = 1;
    result.value.data[0].f0.f1.data = malloc(sizeof(int64_t));
    result.value.data[0].f0.f1.data[0] = 42;
    result.ok = true;
    return result;
}
int main(void) {
    (void)test_realloc;
    assert(__ENTRY__(1).ok && outstanding() == 0 && attempts == 8);
    assert(!__ENTRY__(-1).ok && outstanding() == 0 && attempts == 12);
    return 0;
}
"#
    .replace("__ENTRY__", &entry)
    .replace("__RESULT__", &carrier)
    .replace("__DEPENDENCY__", &dependency_name);
    let c = cwd.join("nested-owner.c");
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    let binary = cwd.join(format!("nested-owner{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&c, &binary, cwd);
    fs::write(cwd.join("function.txt"), "RejectClone").unwrap();
    let clone = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(clone.status.code(), Some(1));
    assert!(clone.stdout.is_empty());
    assert!(String::from_utf8_lossy(&clone.stderr).contains("linked_nested_array_clone"));
    assert_ok(&execute(&mut Command::new(binary), cwd, 15));

    let NestedPlan {
        entry,
        dependency,
        dependency_result: carrier,
        code,
        ..
    } = nested_plan(seed, cwd, "InspectRecord", "Struct");
    let host = format!(
        "{NESTED_RECORD_WITNESS}{}",
        r#"
int main(void) {
    (void)test_realloc;
    assert(__ENTRY__(1).ok && outstanding() == 0 && attempts == 4);
    KuBootstrapResultNull failed = __ENTRY__(-1);
    assert(!failed.ok && failed.error.message.len == 1 && *failed.error.message.ptr == 'x');
    assert(outstanding() == 1 && attempts == 7);
    ku_projection_string_drop(&failed.error.message);
    assert(outstanding() == 0);
    return 0;
}
"#
    )
    .replace("__ENTRY__", &entry)
    .replace("__RESULT__", &carrier)
    .replace("__DEPENDENCY__", &dependency);
    let c = cwd.join("nested-record-owner.c");
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    let binary = cwd.join(format!(
        "nested-record-owner{}",
        std::env::consts::EXE_SUFFIX
    ));
    compile_projection(&c, &binary, cwd);
    assert_ok(&execute(&mut Command::new(binary), cwd, 15));
    assert_tail_record_result(seed, cwd);
    assert_nested_array_append(seed, cwd);
    for name in [
        "nested-provider.ku",
        "nested-consumer.ku",
        "nested-collector.ku",
        "modules.txt",
    ] {
        fs::remove_file(cwd.join(name)).unwrap();
    }
}

const NESTED_RECORD_WITNESS: &str = r#"
__RESULT__ __DEPENDENCY__(int64_t flag) {
    __RESULT__ result = {0};
    uint8_t *text = malloc(1);
    *text = 'x';
    KuProjectionString owner = {text,1,1,KU_PROJECTION_STRING_OWNED};
    if (flag < 0) {
        result.error.message = owner;
        return result;
    }
    KuGuardStringArray *labels = &result.value.f0.f0;
    labels->data = malloc(sizeof(*labels->data));
    labels->data[0] = owner;
    labels->len = 1;
    labels->capacity = 1;
    result.ok = true;
    return result;
}
"#;

struct NestedPlan {
    entry: String,
    entry_result: String,
    dependency: String,
    dependency_result: String,
    code: String,
}

fn nested_plan(seed: &Path, cwd: &Path, function: &str, kind: &str) -> NestedPlan {
    fs::write(cwd.join("function.txt"), function).unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let planned = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&planned);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(planned.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected nested array link plan")
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
    let Value::Object(entry_signature) = &export["signature"] else {
        panic!("expected entry signature")
    };
    let Value::Object(entry_result) = &entry_signature["result"] else {
        panic!("expected entry result")
    };
    let Value::String(entry_carrier) = &entry_result["c_type"] else {
        panic!("expected entry type")
    };
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let [Value::Object(dependency)] = dependencies.as_slice() else {
        panic!("expected one real declaration")
    };
    let Value::String(dependency_name) = &dependency["symbol"] else {
        panic!("expected dependency symbol")
    };
    let Value::Object(signature) = &dependency["signature"] else {
        panic!("expected typed dependency")
    };
    let Value::Object(result) = &signature["result"] else {
        panic!("expected array result")
    };
    assert_eq!(result["kind"], Value::String(kind.into()));
    let Value::String(carrier) = &result["c_type"] else {
        panic!("expected typed Result carrier")
    };
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();

    NestedPlan {
        entry: entry.clone(),
        entry_result: entry_carrier.clone(),
        dependency: dependency_name.clone(),
        dependency_result: carrier.clone(),
        code,
    }
}

fn assert_tail_record_result(seed: &Path, cwd: &Path) {
    let NestedPlan {
        entry,
        entry_result,
        dependency,
        dependency_result,
        code,
    } = nested_plan(seed, cwd, "Tail", "Struct");
    assert_eq!(entry_result, dependency_result);
    let drop = format!(
        "ku_drop_{}",
        entry_result.strip_prefix("KuBootstrapResult_").unwrap()
    );
    let host = format!(
        "{NESTED_RECORD_WITNESS}{}",
        r#"
int main(void) {
    (void)test_realloc;
    __RESULT__ result = __ENTRY__(0);
    assert(result.ok && outstanding() == 2 && attempts == 2);
    assert(*result.value.f0.f0.data[0].ptr == 'x');
    __DROP__(&result.value);
    assert(outstanding() == 0);
    result = __ENTRY__(-1);
    assert(!result.ok && outstanding() == 1 && attempts == 3);
    assert(result.error.message.len == 1 && *result.error.message.ptr == 'x');
    ku_projection_string_drop(&result.error.message);
    assert(outstanding() == 0);
    return 0;
}
"#
    )
    .replace("__ENTRY__", &entry)
    .replace("__RESULT__", &entry_result)
    .replace("__DEPENDENCY__", &dependency)
    .replace("__DROP__", &drop);
    let c = cwd.join("nested-record-tail.c");
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    let exe = cwd.join(format!(
        "nested-record-tail{}",
        std::env::consts::EXE_SUFFIX
    ));
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    fs::write(cwd.join("function.txt"), "WrongTail").unwrap();
    let invalid = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("linked_tail_result"));
}

fn assert_nested_array_append(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("nested-collector.ku"),
        r#"import { Entry, ReadPath } from "./nested-provider.ku"
fn Collect(&paths: [str], &prefix: str): [Entry]! {
    supplied: [Entry] = []
    index = 0
    while (index < paths.len()) {
        path = paths[index].clone()
        entry = ReadPath(path, prefix)?
        supplied = supplied.push(entry)
        index = index + 1
    }
    return ok(supplied)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "nested-collector.ku\nnested-provider.ku\n",
    )
    .unwrap();
    let NestedPlan {
        entry,
        entry_result,
        dependency,
        dependency_result: carrier,
        code,
    } = nested_plan(seed, cwd, "Collect", "Struct");
    let drop = entry_result
        .strip_prefix("KuBootstrapResult_KuArray_")
        .unwrap();
    let drop = format!("ku_linked_view_{drop}_drop");
    assert!(code.contains("_append_move(&") && !code.contains("_append_clone"));
    let host = r#"
static unsigned called;
__RESULT__ __DEPENDENCY__(KuProjectionString path, const KuProjectionString *prefix) {
    __RESULT__ result = {0};
    ++called;
    assert(prefix->len == 2 && memcmp(prefix->ptr, "./", 2) == 0);
    assert(path.len == 1);
    if (*path.ptr == '!') {
        result.error.message = path;
        return result;
    }
    KuGuardStringArray *labels = &result.value.f0.f0;
    labels->data = malloc(sizeof(*labels->data));
    assert(labels->data);
    labels->data[0] = path;
    labels->len = 1;
    labels->capacity = 1;
    result.ok = true;
    return result;
}
static void after_growth_failure(void) {
    if (outstanding() || called != 9 || attempts != 11) _Exit(82);
}
int main(int argc, char **argv) {
    (void)argv;
    static uint8_t names[] = "ABCDEFGHIJKL";
    static uint8_t prefix[] = "./";
    KuProjectionString path_values[12];
    for (size_t i = 0; i < 12; ++i) {
        path_values[i] = (KuProjectionString){&names[i], 1, 0, KU_PROJECTION_STRING_STATIC};
    }
    KuGuardStringArray paths = {12, path_values, 12};
    KuProjectionString base = {prefix, 2, 0, KU_PROJECTION_STRING_STATIC};
    if (argc == 2) {
        assert(atexit(after_growth_failure) == 0);
        fail_after = 10;
        (void)__ENTRY__(&paths, &base);
        return 83;
    }
    __ENTRY_RESULT__ recovered = __ENTRY__(&paths, &base);
    assert(recovered.ok && recovered.value.len == 12 && called == 12);
    assert(outstanding() == 13 && attempts == 14);
    for (size_t i = 0; i < 12; ++i) {
        assert(recovered.value.data[i].f0.f0.data[0].ptr == &names[i]);
    }
    __DROP__(&recovered.value);
    assert(!outstanding() && paths.data == path_values && paths.len == 12);
    names[2] = '!';
    __ENTRY_RESULT__ failed = __ENTRY__(&paths, &base);
    assert(!failed.ok && failed.error.message.ptr == &names[2] && called == 15);
    assert(!outstanding());
    paths.len = 0;
    recovered = __ENTRY__(&paths, &base);
    assert(recovered.ok && !recovered.value.len && !recovered.value.data && called == 15);
    return 0;
}
"#
    .replace("__ENTRY__", &entry)
    .replace("__ENTRY_RESULT__", &entry_result)
    .replace("__RESULT__", &carrier)
    .replace("__DEPENDENCY__", &dependency)
    .replace("__DROP__", &drop);
    let c = cwd.join("nested-record-append.c");
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    let exe = cwd.join(format!(
        "nested-record-append{}",
        std::env::consts::EXE_SUFFIX
    ));
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    let exhausted = execute(Command::new(&exe).arg("grow"), cwd, 15);
    assert_eq!(exhausted.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&exhausted.stderr).trim(),
        "array allocation failed"
    );
}

pub(super) fn assert_short_circuit_reads(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("logic-provider.ku"),
        "fn Mark(): null! { return ok(null) }\n",
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "logic-entry.ku\nlogic-provider.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Observe").unwrap();
    for (operator, skip, expected) in [("||", "chosen", 1), ("&&", "absent", 0)] {
        fs::write(
            cwd.join("logic-entry.ku"),
            format!(
                "import {{ Mark }} from \"./logic-provider.ku\"\n\
                 fn Observe(&command: str, &values: [str]): null! {{\n\
                     if (command == \"chosen\" {operator} values[1] == \"ready\") {{ Mark()? }}\n\
                     return ok(null)\n\
                 }}\n"
            ),
        )
        .unwrap();
        fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
        let output = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&output);
        let Some(Value::Result { ok: true, value }) = json::eval(
            "decode",
            &[Value::String(String::from_utf8(output.stdout).unwrap())],
            Span::default(),
        )
        .unwrap() else {
            panic!("expected checked logical C plan");
        };
        let Value::Object(plan) = *value else {
            panic!("expected object")
        };
        let Value::Object(export) = &plan["export"] else {
            panic!("expected export")
        };
        let Value::Array(links) = &plan["dependencies"] else {
            panic!("expected links")
        };
        let [Value::Object(link)] = links.as_slice() else {
            panic!("expected one checked call")
        };
        let (Value::String(entry), Value::String(callee)) = (&export["symbol"], &link["symbol"])
        else {
            panic!("expected C symbols");
        };
        fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
        let output = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&output);
        let program = String::from_utf8(output.stdout).unwrap();
        let c = cwd.join("logic.c");
        let exe = cwd.join(format!("logic{}", std::env::consts::EXE_SUFFIX));
        let witness = r#"
#include <assert.h>
static int called;
KuBootstrapResultNull CALLEE(void) {
    ++called;
    return (KuBootstrapResultNull){.ok = true};
}
int main(int argc, char** argv) {
    (void)argv;
    uint8_t skip[] = "SKIP";
    uint8_t chosen[] = "chosen", absent[] = "absent", ready[] = "ready";
    KuProjectionString command = {skip, sizeof(skip) - 1, 0, KU_PROJECTION_STRING_STATIC};
    KuProjectionString active = {ACTIVE, sizeof(ACTIVE) - 1, 0, KU_PROJECTION_STRING_STATIC};
    KuProjectionString words[] = {{0}, {ready, 5, 0, KU_PROJECTION_STRING_STATIC}};
    KuGuardStringArray empty = {0}, values = {2, words, 2};
    (void)chosen;
    (void)absent;
    if (argc > 1) {
        (void)ENTRY(&active, &empty);
        return 42;
    }
    assert(ENTRY(&command, &empty).ok && called == CALLS);
    assert(ENTRY(&active, &values).ok && called == CALLS + 1);
    return 0;
}
"#
        .replace("CALLEE", callee)
        .replace("ENTRY", entry)
        .replace("SKIP", skip)
        .replace("ACTIVE", if operator == "||" { "absent" } else { "chosen" })
        .replace("CALLS", &expected.to_string());
        fs::write(&c, format!("{program}{witness}")).unwrap();
        compile_projection(&c, &exe, cwd);
        assert_ok(&execute(&mut Command::new(&exe), cwd, 10));
        let failure = execute(Command::new(&exe).arg("selected-empty"), cwd, 10);
        assert_eq!(failure.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&failure.stderr).contains("index 1 out of bounds for length 0")
        );
    }
}
