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
        first = Supply(1)?
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
    fs::write(cwd.join("function.txt"), "RejectMoved").unwrap();
    let refused = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unavailable_linked_binding"));
}
