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

pub(super) fn assert_path_byte_guard(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("path-guard.ku"),
        r#"fn Check(&path: str): null! {
    if (path.byte_len() == 0 || path.byte_len() > 512 || !path.ends_with(".ku")) {
        fail { domain: "fixture", code: "project_path", message: "bad path" }
    }
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "path-guard.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Check\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null\n").unwrap();
    let output = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&output);
    let program = String::from_utf8(output.stdout).unwrap();
    let Some(entry) = program.lines().find_map(|line| {
        line.strip_prefix("KuBootstrapResultNull ku_linked_null_")
            .and_then(|declaration| declaration.split_once('('))
            .map(|(node, _)| format!("ku_linked_null_{node}"))
    }) else {
        panic!("typed path predicate export missing");
    };
    let caller = r#"
#include <assert.h>
static void verify(const uint8_t* bytes, size_t length, bool ok) {
    const KuProjectionString path = {(uint8_t*)bytes, length, 0u, KU_PROJECTION_STRING_STATIC};
    KuBootstrapResultNull result = __ENTRY__(&path);
    assert(result.ok == ok);
    if (!ok) {
        assert(result.error.code.len == 12u);
        assert(result.error.code.storage == KU_PROJECTION_STRING_STATIC);
        assert(memcmp(result.error.code.ptr, "project_path", 12u) == 0);
    }
}
int main(void) {
    uint8_t bytes[513];
    for (size_t i = 0; i < 170u; ++i) memcpy(bytes + i * 3u, "\345\220\215", 3u);
    memcpy(bytes + 510u, ".ku", 3u);
    verify(bytes, 513u, false);
    memcpy(bytes + 507u, ".ku", 3u);
    verify(bytes, 510u, true);
    memset(bytes, 'a', sizeof(bytes));
    memcpy(bytes + 509u, ".ku", 3u);
    verify(bytes, 512u, true);
    verify((const uint8_t*)".KU", 3u, false);
    verify(NULL, 0u, false);
    return 0;
}
"#;
    let source = cwd.join("path-guard.c");
    let executable = cwd.join(format!("path-guard{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!("{program}{}", caller.replace("__ENTRY__", &entry)),
    )
    .unwrap();
    compile_projection(&source, &executable, cwd);
    assert_ok(&execute(&mut Command::new(executable), cwd, 10));
}

pub(super) fn assert_owned_binary_join(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("join.ku"),
        "fn Join(&left: str, right: str): str! { return ok(left + \"/\" + right) }\n",
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "join.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Join\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null\n").unwrap();
    let output = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&output);
    let program = String::from_utf8(output.stdout).unwrap();
    let Some(entry) = program.lines().find_map(|line| {
        line.strip_prefix("KuBootstrapResult_KuProjectionString ku_linked_null_")
            .and_then(|declaration| declaration.split_once('('))
            .map(|(node, _)| format!("ku_linked_null_{node}"))
    }) else {
        panic!("typed str! export missing");
    };
    let caller = r#"
static void after_join_failure(void) {
    if (outstanding() || attempts != 3) _Exit(82);
}
int main(int argc, char** argv) {
    (void)test_realloc;
    (void)argv;
    const KuProjectionString left = {(uint8_t*)"\345\220\215", 3, 0, KU_PROJECTION_STRING_STATIC};
    KuProjectionString right = {malloc(5), 5, 5, KU_PROJECTION_STRING_OWNED};
    memcpy(right.ptr, "\000\360\237\231\202", 5);
    if (argc == 2) {
        assert(atexit(after_join_failure) == 0);
        fail_after = 1;
        (void)__ENTRY__(&left, right);
        return 83;
    }
    KuBootstrapResult_KuProjectionString joined = __ENTRY__(&left, right);
    assert(joined.ok && joined.value.len == 9);
    assert(!memcmp(joined.value.ptr, "\345\220\215/\000\360\237\231\202", 9));
    assert(left.storage == KU_PROJECTION_STRING_STATIC && left.len == 3);
    assert(attempts == 3 && outstanding() == 1);
    ku_projection_string_drop(&joined.value);
    assert(!outstanding());
    return 0;
}
"#;
    let source = cwd.join("binary-join.c");
    let executable = cwd.join(format!("binary-join{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{program}{}",
            caller.replace("__ENTRY__", &entry)
        ),
    )
    .unwrap();
    compile_projection(&source, &executable, cwd);
    assert_ok(&execute(&mut Command::new(&executable), cwd, 10));
    let failed = execute(Command::new(executable).arg("inject-second-join"), cwd, 10);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(failed.stderr).unwrap().trim(),
        "out of memory"
    );
}

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
                     if (command == \"chosen\" {operator} (values[1].len() == 2 && values[1] == \"中🙂\")) {{ Mark()? }}\n\
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
    uint8_t chosen[] = "chosen", absent[] = "absent";
    uint8_t ready[] = {0xe4, 0xb8, 0xad, 0xf0, 0x9f, 0x99, 0x82};
    KuProjectionString command = {skip, sizeof(skip) - 1, 0, KU_PROJECTION_STRING_STATIC};
    KuProjectionString active = {ACTIVE, sizeof(ACTIVE) - 1, 0, KU_PROJECTION_STRING_STATIC};
    KuProjectionString words[] = {{0}, {ready, sizeof(ready), 0, KU_PROJECTION_STRING_STATIC}};
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

pub(super) fn assert_checked_string_array_borrow(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("paths-provider.ku"),
        r#"fn ReadPaths(mode: int): [str]! { return ok([]) }
fn Observe(&paths: [str], mode: int): null! { return ok(null) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("paths-consumer.ku"),
        r#"import { ReadPaths, Observe } from "./paths-provider.ku"
fn TakePaths(mode: int): null! {
    paths = ReadPaths(mode)?
    Observe(paths, mode)?
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "paths-consumer.ku\npaths-provider.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "TakePaths").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let symbol = |prefix| {
        code.lines()
            .find_map(|line| Some(line.strip_prefix(prefix)?.split_once('(')?.0))
            .expect("typed C symbol")
    };
    let witness = r#"
static uint8_t *saved;
static int visits;
KuBootstrapResultStringArray __READ__(int64_t mode) {
    if (mode == 2) return (KuBootstrapResultStringArray){0};
    KuProjectionString *data = malloc(sizeof(*data));
    saved = malloc(1);
    *saved = 'x';
    *data = (KuProjectionString){saved, 1, 1, KU_PROJECTION_STRING_OWNED};
    return (KuBootstrapResultStringArray){.ok = true, .value = {1, data, 1}};
}
KuBootstrapResultNull __OBSERVE__(const KuGuardStringArray *paths, int64_t mode) {
    assert(paths->len == 1 && paths->data[0].ptr == saved && *saved == 'x');
    assert(outstanding() == 2);
    ++visits;
    return (KuBootstrapResultNull){.ok = (mode == 0)};
}
int main(void) {
    (void)test_realloc;
    assert(__ENTRY__(0).ok && !outstanding() && visits == 1);
    assert(!__ENTRY__(1).ok && !outstanding() && visits == 2);
    assert(!__ENTRY__(2).ok && !outstanding() && visits == 2);
    return 0;
}
"#
    .replace("__READ__", symbol("extern KuBootstrapResultStringArray "))
    .replace("__OBSERVE__", symbol("extern KuBootstrapResultNull "))
    .replace("__ENTRY__", symbol("KuBootstrapResultNull "));
    let c = cwd.join("paths-consumer.c");
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{witness}")).unwrap();
    let executable = cwd.join(format!("paths-consumer{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&c, &executable, cwd);
    assert_ok(&execute(&mut Command::new(&executable), cwd, 15));
}

pub(super) fn assert_named_record_clone(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("named-clone.ku"),
        r#"struct Token { kind: str lexeme: str line: int }
fn Copy(token: Token): Token! { return ok(token.clone()) }
fn Refuse(token: Token): Token! {
    moved = token.kind
    return ok(token.clone())
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "named-clone.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Copy").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let body = code.split_once(" a0) {").unwrap().1;
    assert!(body.contains("_clone_element(&a0, &t"));
    assert!(body.contains("result.value = t"));
    let cleanup = body.split_once("cleanup:\n").unwrap().1;
    assert!(cleanup.contains("ku_projection_string_drop(&a0.f0)"));
    assert!(cleanup.contains("ku_projection_string_drop(&a0.f1)"));

    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_clone_moved"));
}

pub(super) fn assert_owned_record_destructure(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("pattern-provider.ku"),
        r#"struct Item { id: int label: str }
struct Packet { entries: [Item] tags: [int] title: str }
struct Reply { packet: Packet stamp: int }
fn Prepare(input: Packet, reject: bool): Reply! {
    if (reject) { fail { domain: "pattern", code: "rejected", message: input.title } }
    return ok(Reply { packet: input, stamp: 27 })
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("pattern-consumer.ku"),
        r#"import { Packet, Prepare } from "./pattern-provider.ku"
fn Store(input: Packet, previous: Packet, reject: bool, grow: bool): Packet! {
    packet = previous
    { stamp: marker, packet } = Prepare(input, reject)?
    { title: title, entries, tags } = packet
    if (grow) { tags = tags.push(marker) }
    return ok(Packet { title: title, tags: tags, entries: entries })
}
fn Borrowed(&packet: Packet): null! {
    { title, entries, tags } = packet
    return ok(null)
}
fn Repeated(packet: Packet): null! {
    { title: field, entries, tags: field } = packet
    return ok(null)
}
fn AfterMove(packet: Packet): Packet! {
    { title, entries, tags } = packet
    return ok(packet)
}
fn Defaulted(packet: Packet, backup: str): null! {
    { title = backup, entries, tags } = packet
    return ok(null)
}
fn Rest(packet: Packet): null! {
    { title, ...remaining } = packet
    return ok(null)
}
struct Counter { number: int }
struct CounterBox { value: Counter }
fn CursorChange(): null! {
    bound = 2
    cursor = 0
    while (cursor < bound) {
        replacement = Counter { number: 0 }
        { number: cursor } = replacement
        cursor = cursor + 1
    }
    return ok(null)
}
fn BoundChange(): null! {
    bound = 2
    cursor = 0
    while (cursor < bound) {
        replacement = Counter { number: 0 }
        { number: bound } = replacement
        cursor = cursor + 1
    }
    return ok(null)
}
fn RecordBoundChange(): null! {
    bound = Counter { number: 2 }
    cursor = 0
    while (cursor < bound.number) {
        replacement = CounterBox { value: Counter { number: 0 } }
        { value: bound } = replacement
        cursor = cursor + 1
    }
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "pattern-consumer.ku\npattern-provider.ku\n",
    )
    .unwrap();
    let NestedPlan {
        entry,
        entry_result,
        dependency,
        dependency_result,
        code,
    } = nested_plan(seed, cwd, "Store", "Struct");
    let payload = entry_result.strip_prefix("KuBootstrapResult_").unwrap();
    let host = r#"
static int calls;
static KuProjectionString string_owner(char ch) {
    uint8_t *bytes = malloc(1);
    bytes[0] = (uint8_t)ch;
    return (KuProjectionString){bytes, 1, 1, KU_PROJECTION_STRING_OWNED};
}
static __PAYLOAD__ packet(char ch) {
    __PAYLOAD__ value = {0};
    value.f0.data = malloc(sizeof(*value.f0.data));
    value.f0.len = value.f0.capacity = 1;
    value.f0.data[0].f0 = 12;
    value.f0.data[0].f1 = string_owner(ch);
    value.f1.data = malloc(sizeof(*value.f1.data));
    value.f1.len = value.f1.capacity = 1;
    value.f1.data[0] = 41;
    value.f2 = string_owner(ch);
    return value;
}
__DEPENDENCY_RESULT__ __DEPENDENCY__(__PAYLOAD__ input, bool reject) {
    ++calls;
    __DEPENDENCY_RESULT__ transferred = {0};
    if (reject) {
        ku_projection_string_drop(&input.f0.data[0].f1);
        free(input.f0.data);
        free(input.f1.data);
        transferred.error.message = input.f2;
    } else {
        transferred.ok = true;
        transferred.value.f0 = input;
        transferred.value.f1 = 27;
    }
    return transferred;
}
static void check_failed_allocation(void) {
    if (outstanding() || calls != 1 || attempts != 9) _Exit(81);
}
int main(int argc, char **argv) {
    (void)argv;
    __PAYLOAD__ input = packet('x');
    __PAYLOAD__ previous = packet('y');
    if (argc == 2) {
        assert(atexit(check_failed_allocation) == 0);
        fail_after = 0;
        (void)__ENTRY__(input, previous, false, true);
        return 82;
    }
    __RESULT__ result = __ENTRY__(input, previous, false, false);
    assert(result.ok && calls == 1 && attempts == 8 && outstanding() == 4);
    assert(result.value.f0.data == input.f0.data && result.value.f0.data[0].f0 == 12);
    assert(result.value.f0.data[0].f1.ptr == input.f0.data[0].f1.ptr);
    assert(result.value.f1.data == input.f1.data && result.value.f1.data[0] == 41);
    assert(result.value.f2.ptr == input.f2.ptr && *result.value.f2.ptr == 'x');
    __DROP__(&result.value);
    assert(outstanding() == 0);
    input = packet('z');
    previous = packet('w');
    result = __ENTRY__(input, previous, true, true);
    assert(!result.ok && result.error.message.ptr == input.f2.ptr);
    assert(calls == 2 && attempts == 16 && outstanding() == 1);
    ku_projection_string_drop(&result.error.message);
    assert(outstanding() == 0);
    return 0;
}
"#
    .replace("__ENTRY__", &entry)
    .replace("__RESULT__", &entry_result)
    .replace("__DEPENDENCY_RESULT__", &dependency_result)
    .replace("__DEPENDENCY__", &dependency)
    .replace("__PAYLOAD__", payload)
    .replace("__DROP__", &format!("ku_drop_{payload}"));
    let c = cwd.join("owned-record-pattern.c");
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    let exe = cwd.join(format!(
        "owned-record-pattern{}",
        std::env::consts::EXE_SUFFIX
    ));
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 10));
    let trapped = execute(Command::new(&exe).arg("allocation-failure"), cwd, 10);
    assert_eq!(trapped.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&trapped.stderr).trim(),
        "array allocation failed"
    );
    for (function, diagnostic) in [
        ("Borrowed", "linked_destructure_source"),
        ("Repeated", "linked_destructure_field"),
        ("AfterMove", "unavailable_linked_binding"),
        ("Defaulted", "linked_destructure_shape"),
        ("Rest", "linked_destructure_shape"),
        ("CursorChange", "linked_loop_cursor_write"),
        ("BoundChange", "linked_loop_bound_write"),
        ("RecordBoundChange", "linked_loop_bound_write"),
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        let error = String::from_utf8_lossy(&rejected.stderr);
        assert!(error.contains(diagnostic), "{diagnostic}, got {error}");
    }
}

pub(super) fn assert_local_array_reader(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("local-reader.ku"),
        r#"struct Item { label: str value: int }
fn Inspect(first: Item, second: Item): Item! {
    values: [Item] = []
    values = values.push(first)
    fn Read(index: int): Item { return values[index - 1].clone() }
    saved = Read(1)
    values = values.push(second)
    current = Read(2)
    if (saved.value != 11) { fail { domain: "test", code: "old", message: "old element changed" } }
    return ok(current)
}
fn Escape(first: Item): Item! {
    values: [Item] = []
    values = values.push(first)
    fn Read(index: int): Item { return values[index].clone() }
    escaped = Read
    return ok(escaped(0))
}
fn Moved(first: Item): Item! {
    values: [Item] = []
    values = values.push(first)
    fn Read(index: int): Item { return values[index].clone() }
    moved = values
    return ok(Read(0))
}
fn MovedAfterRead(first: Item): Item! {
    values: [Item] = []
    values = values.push(first)
    fn Read(index: int): Item { return values[index].clone() }
    result = Read(0)
    moved = values
    return ok(result)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "local-reader.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Inspect").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, args) = signature.split_once('(').unwrap();
    let (carrier, _) = args.split_once(" a0").unwrap();
    assert!(!code.contains("extern "));
    assert_eq!(code.matches("int64_t local_index").count(), 2);
    let witness = r#"
#include <assert.h>
int main(void) {
    uint8_t first[] = "old", second[] = "new";
    RESULT result = ENTRY(
        (ITEM){{first,3,0,KU_PROJECTION_STRING_STATIC},11},
        (ITEM){{second,3,0,KU_PROJECTION_STRING_STATIC},22});
    assert(result.ok && result.value.f1 == 22 && !memcmp(result.value.f0.ptr, second, 3));
    ku_projection_string_drop(&result.value.f0);
    return 0;
}
"#
    .replace("RESULT", result)
    .replace("ENTRY", &format!("ku_linked_null_{entry}"))
    .replace("ITEM", carrier);
    let c = cwd.join("local-reader.c");
    let exe = cwd.join(format!("local-reader{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{code}{witness}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));

    for (name, error) in [
        ("Escape", "linked_local_callable_escape"),
        ("Moved", "linked_local_reader_capture_move"),
        ("MovedAfterRead", "linked_local_reader_capture_move"),
    ] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(error));
    }
}

pub(super) fn assert_linked_fs_read(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("module-reader.ku"),
        r#"import { fs } from "std"
fn ReadModuleText(&path: str): str! {
    source = fs.read(path)?
    return ok(source)
}
fn Shadow(&fs: str, &path: str): str! { return fs.read(path) }
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "module-reader.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "ReadModuleText").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null-plan").unwrap();
    let projected = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&projected);
    let metadata = cwd.join("module-reader.json");
    fs::write(&metadata, &projected.stdout).unwrap();
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(projected.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected checked filesystem link plan");
    };
    let Value::Object(plan) = *value else {
        panic!("expected plan object");
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export");
    };
    let (Value::String(entry), Value::Int(node)) = (&export["symbol"], &export["node"]) else {
        panic!("expected bound entry");
    };
    let provider_plan = cwd.join("fs-provider.json");
    fs::write(
        &provider_plan,
        r#"{"schema":"ku-bootstrap-link-plan-v1","mode":"provider","export":{
    "source_path":"std/fs.ku","node":1,"source_name":"read","symbol":"KuBootstrapFsRead",
    "signature":{"parameters":[{"kind":"Builtin","source_name":"str","source_path":null,"node":0,
      "c_type":"const KuString*","passing":"view"}],
      "result":{"kind":"Builtin","source_name":"str","source_path":null,"node":0,
      "c_type":"KuResult_str","passing":"value"},"returns_result":true}},"dependencies":[]}"#,
    )
    .unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let projected = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&projected);
    let c = cwd.join("read-module.c");
    fs::write(&c, projected.stdout).unwrap();
    fs::write(cwd.join("source-bytes.ku"), "清单🙂\0\r\n".as_bytes()).unwrap();
    let host = cwd.join("read-module-host.c");
    fs::write(
        &host,
        r#"
#include <assert.h>
#include <string.h>
int main(void) {
    static const char expected[] = "\346\270\205\345\215\225\360\237\231\202\000\015\012";
    KuProjectionString path = {(uint8_t*)"source-bytes.ku", 15, 0, KU_PROJECTION_STRING_STATIC};
    KuBootstrapResult_KuProjectionString result = __ENTRY__(&path);
    assert(result.ok && result.value.storage == KU_PROJECTION_STRING_OWNED);
    assert(result.value.len == sizeof(expected)-1 && !memcmp(result.value.ptr,expected,result.value.len));
    assert(path.len == 15 && !memcmp(path.ptr,"source-bytes.ku",path.len));
    ku_projection_string_drop(&result.value);
    path = (KuProjectionString){(uint8_t*)"missing.ku", 10, 0, KU_PROJECTION_STRING_STATIC};
    result = __ENTRY__(&path);
    assert(!result.ok && !result.value.ptr && result.error.code.len == 11);
    assert(!memcmp(result.error.code.ptr,"read_failed",11));
    ku_projection_string_drop(&result.error.domain);
    ku_projection_string_drop(&result.error.code);
    ku_projection_string_drop(&result.error.message);
    return 0;
}
"#
        .replace("__ENTRY__", entry),
    )
    .unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tool = repo.join("bootstrap/tools");
    let provider = tool.join("compiler_driver");
    let input = repo.join("src/backend");
    let cc =
        std::env::var("CC").unwrap_or_else(|_| if cfg!(windows) { "clang" } else { "cc" }.into());
    let stem = Path::new(&cc)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let include = if stem.eq_ignore_ascii_case("cl") || stem.eq_ignore_ascii_case("clang-cl") {
        "/I"
    } else {
        "-I"
    };
    let exe = cwd.join(format!("read-module{}", std::env::consts::EXE_SUFFIX));
    let linked = execute(
        Command::new(if cfg!(windows) { "python" } else { "python3" })
            .arg("-B")
            .arg(tool.join("link_c.py"))
            .args(["--cc", &cc])
            .arg("--unit")
            .args([&metadata, &c])
            .arg("--unit")
            .arg(provider_plan)
            .arg(provider.join("fs_read.c"))
            .args(["--entry", "module-reader.ku", &node.to_string(), "--caller"])
            .arg(host)
            .arg(format!("--cflag={include}{}", provider.display()))
            .arg(format!("--cflag={include}{}", input.display()))
            .arg("-o")
            .arg(&exe),
        cwd,
        120,
    );
    assert_ok(&linked);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
    fs::write(cwd.join("function.txt"), "Shadow").unwrap();
    let shadow = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(shadow.status.code(), Some(1));
    assert!(shadow.stdout.is_empty());
    assert!(String::from_utf8_lossy(&shadow.stderr).contains("shadowed_linked_dependency"));
    for name in ["module-reader.ku", "source-bytes.ku", "modules.txt"] {
        fs::remove_file(cwd.join(name)).unwrap();
    }
}
