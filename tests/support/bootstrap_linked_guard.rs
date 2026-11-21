use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

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
        "fn Load(fail_read: bool): str! { return ok(\"payload\") }\n",
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
fn Consume(value: str): null! { return ok(null) }
fn Keep(value: str): str! { return ok(value) }
fn WrongElse(first: bool): str! {
    left = Supply(1)?
    right = Supply(1)?
    if (first) { Consume(left)? }
    else { right = Supply(3)? }
    return ok(right)
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
        assert(outstanding() == 1 && *result.value.ptr == i);
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
    fs::write(cwd.join("function.txt"), "Replace").unwrap();
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
    let (Value::String(entry), Value::String(provider)) = (&export["symbol"], &supply["symbol"])
    else {
        panic!("expected the bound symbols")
    };
    let host = REBIND_HOST
        .replace("__ENTRY__", entry)
        .replace("__SUPPLY__", provider);
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
    for name in ["WrongElse", "WrongMembers"] {
        fs::write(cwd.join("function.txt"), name).unwrap();
        let wrong = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(wrong.status.code(), Some(1));
        assert!(wrong.stdout.is_empty());
        assert!(String::from_utf8_lossy(&wrong.stderr).contains("linked_control_move"));
    }
    fs::write(cwd.join("function.txt"), "CatchMoved").unwrap();
    let moved = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(moved.status.code(), Some(1));
    assert!(moved.stdout.is_empty());
    assert!(String::from_utf8_lossy(&moved.stderr).contains("unavailable_linked_binding"));
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
fn OutsideWrite(start: int): null! {
    cursor = start
    remaining = start
    while (remaining > 0) { remaining = remaining - 1 }
    cursor = cursor + 1
    return ok(null)
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
    for (function, diagnostic) in [
        ("WrongStep", "unsupported_linked_countdown"),
        ("NestedWrite", "linked_loop_cursor_write"),
        ("OutsideWrite", "linked_rebind_type"),
        ("NestedIncrease", "linked_rebind_type"),
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
    assert(available.data == data && available.len == 3);
    return 0;
}
"#;
    let c = cwd.join("counted-int-bound.c");
    let app = cwd.join(format!("counted-int-bound{}", std::env::consts::EXE_SUFFIX));
    for function in ["Count", "CountSeparated"] {
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
    ] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        let rejected = execute(&mut Command::new(seed), cwd, 30);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
    }
}
