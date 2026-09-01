//! Imported nominal structs must remain valid early array elements in native C.
//! C emission is mandatory. Compilation/runtime checks skip only when no host C
//! compiler exists; every spawned process has a timeout and output ceiling.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::{fs, process::Command};

use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

const MODEL: &str = r#"
struct Node { label: str }
struct Arena { nodes: [Node], nested: [[Node]] }

fn Make(): Arena {
    seed = Node { label: "root!" }
    return Arena {
        nodes: [seed.clone()],
        nested: [[seed]]
    }
}
"#;

const ENTRY: &str = r#"
import { Node, Make } from "./model.ku"

fn main(): null! {
    original = Make()
    copy = original.clone()
    copy.nodes[0].label = "copy" + "!"
    copy.nested[0][0].label = "nested" + "!"
    copy.nodes = copy.nodes.push(Node { label: "extra" + "!" })
    println(original.nodes[0].label)
    println(original.nested[0][0].label)
    println(copy.nodes[0].label)
    println(copy.nested[0][0].label)
    println(copy.nodes.len())
    return ok(null)
}
"#;

const ALLOCATION_HOOKS: &str = r#"
static size_t test_live_allocations = 0;
static int test_fail_allocations = 0;
static void* test_malloc(size_t size) {
  if (test_fail_allocations) return NULL;
  void* pointer = malloc(size);
  if (pointer) test_live_allocations++;
  return pointer;
}
static void* test_calloc(size_t count, size_t size) {
  if (test_fail_allocations) return NULL;
  void* pointer = calloc(count, size);
  if (pointer) test_live_allocations++;
  return pointer;
}
static void* test_realloc(void* pointer, size_t size) {
  if (test_fail_allocations) return NULL;
  int was_null = pointer == NULL;
  void* replacement = realloc(pointer, size);
  if (replacement && was_null) test_live_allocations++;
  return replacement;
}
static void test_free(void* pointer) {
  if (pointer) {
    if (test_live_allocations == 0) {
      fputs("imported array allocation ledger underflow\n", stderr);
      exit(99);
    }
    test_live_allocations--;
  }
  free(pointer);
}
#define malloc test_malloc
#define calloc test_calloc
#define realloc test_realloc
#define free test_free
#define main ku_imported_array_generated_main
"#;

const HARNESS_MAIN: &str = r#"
#undef main
int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "oom") == 0) {
    test_fail_allocations = 1;
    return ku_imported_array_generated_main();
  }
  int code = ku_imported_array_generated_main();
  if (code != 0) return code;
  if (test_live_allocations != 0) {
    fprintf(stderr, "imported array allocations remained live: %zu\n", test_live_allocations);
    return 98;
  }
  return 0;
}
"#;

#[test]
fn native_imported_struct_arrays_clone_drop_and_fail_oom_closed() {
    let temp = TempDir::new("imported-struct-array");
    let source_dir = temp.path().join("src");
    fs::create_dir_all(&source_dir).expect("create imported array source directory");
    fs::write(source_dir.join("model.ku"), MODEL).expect("write imported array model");
    let generated = native_harness::emit_c(&source_dir, ENTRY);
    assert!(!generated.contains("run_source"));
    assert!(!generated.contains("const SOURCE"));
    assert!(generated.contains("ku_array_clone_struct___ku_import"));
    assert!(generated.contains("ku_array_drop_struct___ku_import"));
    assert!(generated.contains("ku_array_clone_array_struct___ku_import"));

    let instrumented = generated.replacen(
        "typedef struct KuString",
        &format!("{ALLOCATION_HOOKS}\ntypedef struct KuString"),
        1,
    );
    assert_ne!(
        instrumented, generated,
        "native prelude insertion point moved"
    );
    let harness = temp.path().join("imported_array.c");
    fs::write(&harness, format!("{instrumented}\n{HARNESS_MAIN}"))
        .expect("write imported array ownership harness");
    let Some(executable) = compile_harness(temp.path(), &harness, "imported-array") else {
        return;
    };

    // The compiled artifact must be independent from both Ku source files.
    fs::remove_dir_all(&source_dir).expect("remove imported array source graph");
    let output = run_bounded(
        Command::new(&executable).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("imported array native executable must remain bounded");
    assert!(
        output.status.success(),
        "imported array clone/drop failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "root!\nroot!\ncopy!\nnested!\n2\n"
    );
    assert!(output.stderr.is_empty());

    // Array allocation remains a fatal native ABI today. Verify the contract is
    // bounded and fail-closed; do not misrepresent it as recoverable unwinding.
    let oom = run_bounded(
        Command::new(&executable)
            .current_dir(temp.path())
            .arg("oom"),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("imported array OOM process must remain bounded");
    assert_eq!(oom.status.code(), Some(1));
    assert!(oom.stdout.is_empty());
    assert!(String::from_utf8_lossy(&oom.stderr).contains("array allocation failed"));
}
