//! Contextual Result payload typing and owned-error cleanup, pinned between the
//! interpreter and one source-free native C artifact.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use native_pg_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

const SOURCE: &str = r#"struct Box { text: str }

enum Choice {
    Text(value: str)
    Empty
}

fn Owned(label: str): str {
    return "owned-" + label
}

fn ReturnInt(message: str): int! {
    return err(message)
}

fn ReturnStr(): str! {
    message: str = Owned("str")
    return err(message)
}

fn ReturnArray(): [str]! {
    return err(Owned("array"))
}

fn ReturnStruct(): Box! {
    return err(Owned("struct"))
}

fn ReturnEnum(): Choice! {
    return err(Owned("enum"))
}

fn ReturnTypedLocal(): int! {
    result: int! = err(Owned("local"))
    return result
}

fn PassResult(value: int!): int! {
    return value
}

fn EmptyWords(): [str]! {
    return ok([])
}

fn ForwardError(): str! {
    try {
        return ReturnStr()
    } finally {
        println("finally|forward-error")
    }
    return ok("unreachable")
}

fn ReturnOwnedSuccess(): str! {
    try {
        return ok("owned" + "-success")
    } finally {
        println("finally|owned-return")
    }
    return ok("unreachable")
}

fn ShadowErr(value: str): str {
    return "shadow-err:" + value
}

fn ShadowOk(value: str): str {
    return "shadow-ok:" + value
}

fn VerifyShadowNames(): null {
    err: fn(str): str = ShadowErr
    ok: fn(str): str = ShadowOk
    println(err("value"))
    println(ok("value"))
    return null
}

fn VerifyInt(): null! {
    try {
        ReturnInt(Owned("int"))?
        panic("int error was accepted")
    } catch(err) {
        println("int|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|int")
    }
    return ok(null)
}

fn VerifyStr(): null! {
    try {
        ReturnStr()?
        panic("str error was accepted")
    } catch(err) {
        println("str|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|str")
    }
    return ok(null)
}

fn VerifyArray(): null! {
    try {
        ReturnArray()?
        panic("array error was accepted")
    } catch(err) {
        println("array|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|array")
    }
    return ok(null)
}

fn VerifyStruct(): null! {
    try {
        ReturnStruct()?
        panic("struct error was accepted")
    } catch(err) {
        println("struct|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|struct")
    }
    return ok(null)
}

fn VerifyEnum(): null! {
    try {
        ReturnEnum()?
        panic("enum error was accepted")
    } catch(err) {
        println("enum|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|enum")
    }
    return ok(null)
}

fn VerifyLocal(): null! {
    try {
        ReturnTypedLocal()?
        panic("typed-local error was accepted")
    } catch(err) {
        println("local|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|local")
    }
    return ok(null)
}

fn VerifyResultParameter(): null! {
    try {
        PassResult(err(Owned("parameter")))?
        panic("Result parameter error was accepted")
    } catch(err) {
        println("parameter|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|parameter")
    }
    return ok(null)
}

fn VerifyResultParameterAlias(): null! {
    pass: fn(int!): int! = PassResult
    try {
        pass(err(Owned("alias-parameter")))?
        panic("Result parameter alias error was accepted")
    } catch(err) {
        println("alias-parameter|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|alias-parameter")
    }
    return ok(null)
}

fn main(): null! {
    VerifyInt()?
    VerifyStr()?
    VerifyArray()?
    VerifyStruct()?
    VerifyEnum()?
    VerifyLocal()?
    VerifyResultParameter()?
    VerifyResultParameterAlias()?
    words = EmptyWords()?
    println("empty|" + str(words.len()))
    try {
        ForwardError()?
        panic("forwarded error was accepted")
    } catch(err) {
        println("forward|" + err.domain + "|" + err.code + "|" + err.message)
    } finally {
        println("finally|forward-catch")
    }
    println("success|" + ReturnOwnedSuccess()?)
    VerifyShadowNames()
    return ok(null)
}
"#;

const EXPECTED: &str = "int|ku|err|owned-int\n\
finally|int\n\
str|ku|err|owned-str\n\
finally|str\n\
array|ku|err|owned-array\n\
finally|array\n\
struct|ku|err|owned-struct\n\
finally|struct\n\
enum|ku|err|owned-enum\n\
finally|enum\n\
local|ku|err|owned-local\n\
finally|local\n\
parameter|ku|err|owned-parameter\n\
finally|parameter\n\
alias-parameter|ku|err|owned-alias-parameter\n\
finally|alias-parameter\n\
empty|0\n\
finally|forward-error\n\
forward|ku|err|owned-str\n\
finally|forward-catch\n\
finally|owned-return\n\
success|owned-success\n\
shadow-err:value\n\
shadow-ok:value\n";

#[test]
fn native_result_context_preserves_payload_types_and_owned_error_cleanup() {
    let directory = TempDir::new("result-context");
    fs::write(directory.path().join("main.ku"), SOURCE).expect("write Result context fixture");

    let mut interpreted = Command::new(ku_binary());
    interpreted
        .current_dir(directory.path())
        .args(["run", "main.ku"]);
    let interpreted = run_bounded(&mut interpreted, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("Result context interpreter run was not bounded: {error}"));
    assert!(
        interpreted.status.success(),
        "Result context interpreter failed:\n{}{}",
        String::from_utf8_lossy(&interpreted.stdout),
        String::from_utf8_lossy(&interpreted.stderr)
    );
    let vm_stdout = String::from_utf8_lossy(&interpreted.stdout).replace('\r', "");
    assert_eq!(vm_stdout, EXPECTED);

    let generated = emit_c(directory.path(), SOURCE);
    for forbidden in ["run_source(", "const char* SOURCE", "const char *SOURCE"] {
        assert!(
            !generated.contains(forbidden),
            "native Result artifact embedded its source runner: {forbidden}"
        );
    }
    assert!(
        !generated.contains("KuResult_unknown"),
        "declared Result payloads must not decay to Result<unknown>"
    );
    for expected in [
        "KuResult_int ReturnInt(KuString message)",
        "KuResult_str ReturnStr(void)",
        "KuResult_array_str ReturnArray(void)",
        "KuResult_struct_Box ReturnStruct(void)",
        "KuResult_enum_Choice ReturnEnum(void)",
        "KuResult_int PassResult(KuResult_int value)",
        "KuResult_array_str EmptyWords(void)",
    ] {
        assert!(
            generated.contains(expected),
            "missing concrete Result ABI: {expected}"
        );
    }

    let mut harness = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    harness.push_str(OWNERSHIP_WRAPPER);
    let source = directory.path().join("result-context.c");
    fs::write(&source, harness).expect("write Result context C harness");
    let Some(executable) = compile_harness(directory.path(), &source, "result-context") else {
        return;
    };
    let mut native = Command::new(executable);
    native.current_dir(directory.path());
    let native = run_bounded(&mut native, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("Result context native run was not bounded: {error}"));
    assert!(
        native.status.success(),
        "Result context native harness failed ({:?}):\n{}{}",
        native.status.code(),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&native.stdout).replace('\r', ""),
        vm_stdout
    );
    assert!(
        native.stderr.is_empty(),
        "native ownership ledger reported an error"
    );
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = env::var("KU_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return candidate;
        }
    }
    let executable = if cfg!(windows) { "ku.exe" } else { "ku" };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    [
        target.join("debug").join(executable),
        target.join("release").join(executable),
        root.join("target").join("debug").join(executable),
        root.join("target").join("release").join(executable),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .expect("ku binary not found; set KU_BIN or build it before this test")
}

const ALLOCATION_HOOK: &str = r#"
typedef union KuResultAllocation {
  struct { size_t size; } record;
  long double scalar_alignment;
  int64_t integer_alignment;
  void* pointer_alignment;
} KuResultAllocation;
static size_t ku_result_live_allocations = 0, ku_result_live_bytes = 0;
static int ku_result_accounting_error = 0;
static void* ku_result_allocate(size_t size, int clear) {
  if (size > SIZE_MAX - sizeof(KuResultAllocation)) return 0;
  KuResultAllocation* allocation = (KuResultAllocation*)malloc(sizeof(KuResultAllocation) + size);
  if (!allocation) return 0;
  allocation->record.size = size; ku_result_live_allocations++;
  if (size > SIZE_MAX - ku_result_live_bytes) { ku_result_accounting_error = 1; }
  else ku_result_live_bytes += size;
  void* value = (void*)(allocation + 1);
  if (clear && size) memset(value, 0, size);
  return value;
}
static void* ku_result_malloc(size_t size) { return ku_result_allocate(size, 0); }
static void* ku_result_calloc(size_t count, size_t size) {
  if (count && size > SIZE_MAX / count) return 0;
  return ku_result_allocate(count * size, 1);
}
static void ku_result_free(void* value) {
  if (!value) return;
  KuResultAllocation* allocation = ((KuResultAllocation*)value) - 1;
  if (!ku_result_live_allocations || allocation->record.size > ku_result_live_bytes) {
    ku_result_accounting_error = 1; return;
  }
  ku_result_live_allocations--; ku_result_live_bytes -= allocation->record.size; free(allocation);
}
static void* ku_result_realloc(void* value, size_t size) {
  if (!value) return ku_result_allocate(size, 0);
  if (!size) { ku_result_free(value); return 0; }
  KuResultAllocation* allocation = ((KuResultAllocation*)value) - 1;
  size_t old_size = allocation->record.size;
  if (!ku_result_live_allocations || old_size > ku_result_live_bytes
      || size > SIZE_MAX - sizeof(KuResultAllocation)) {
    ku_result_accounting_error = 1; return 0;
  }
  KuResultAllocation* replacement = (KuResultAllocation*)realloc(
      allocation, sizeof(KuResultAllocation) + size);
  if (!replacement) return 0;
  replacement->record.size = size; ku_result_live_bytes -= old_size;
  if (size > SIZE_MAX - ku_result_live_bytes) ku_result_accounting_error = 1;
  else ku_result_live_bytes += size;
  return (void*)(replacement + 1);
}
#define malloc ku_result_malloc
#define calloc ku_result_calloc
#define realloc ku_result_realloc
#define free ku_result_free
"#;

const OWNERSHIP_WRAPPER: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
int main(void) {
  int result = ku_generated_main();
  if (ku_result_accounting_error || ku_result_live_allocations || ku_result_live_bytes) {
    fprintf(stderr, "Result ownership ledger did not return to zero: allocations=%zu bytes=%zu error=%d\n",
      ku_result_live_allocations, ku_result_live_bytes, ku_result_accounting_error);
    return 2;
  }
  return result;
}
"#;
