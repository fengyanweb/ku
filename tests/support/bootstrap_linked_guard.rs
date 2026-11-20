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
