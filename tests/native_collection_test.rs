//! Native collection regressions share the existing bounded C harness.
//! C emission is mandatory; executable checks only skip when no compiler exists.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::{fs, process::Command};

use ku::{backend::c, checker::Checker, cli::run_source, ir, lexer::Lexer, parser::Parser};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

const RECEIVER_ORDER_SOURCE: &str = r#"
struct Container { values: [int], names: [str] }
struct Buckets { rows: [Container] }
fn Missing(): int! { fail "missing" }
fn MissingText(): str! { fail "missing text" }

fn main(): null! {
    container = Container { values: [1, 2], names: ["owned" + "!"] }
    slot = 0
    container.values[slot] = 7
    container.names[slot] = "changed"
    if (container.values[0] != 7 || container.names[0] != "changed") {
        panic("struct field index assignment wrote a temporary")
    }
    container.names = []
    if (container.names.len() != 0) panic("empty array did not replace the field")
    buckets = Buckets { rows: [Container { values: [1], names: ["initial"] }] }
    trace = 0
    next_word = () => {
        trace = trace * 10 + 1
        return "owned" + "界"
    }
    which_row = () => {
        trace = trace * 10 + 2
        return 0
    }
    buckets.rows[which_row()].names = [next_word()]
    if (trace != 12 || buckets.rows[0].names[0] != "owned界") {
        panic("array RHS must complete before evaluating the destination index once")
    }
    try {
        buckets.rows[which_row()].names = [MissingText()?]
    } catch (err) {
        if (trace != 12 || buckets.rows[0].names[0] != "owned界") {
            panic("failed array RHS evaluated or modified its destination")
        }
    }
    buckets.rows[which_row()].names = []
    if (trace != 122 || buckets.rows[0].names.len() != 0) {
        panic("empty array store did not preserve evaluation order")
    }
    values = [1]
    make_piece = () => {
        values = [9]
        return 2
    }
    values = values.push(make_piece())
    if (values.len() != 2 || values[0] != 1 || values[1] != 2) {
        panic("push must snapshot receiver before its callback argument")
    }
    observe = () => { return values.len() }
    piece = 3
    values = values.push(piece)
    if (observe() != 3 || values[2] != 3) panic("captured push binding became stale")

    choices = [10, 20]
    choose = () => {
        choices = [70, 80]
        return 1
    }
    selected = choices[choose()]
    if (selected != 20 || choices[1] != 80) panic("index lost its receiver snapshot")

    text = "before" + "!"
    pattern = () => {
        text = "after"
        return "before!"
    }
    snapshot = text.clone()
    matched = snapshot.contains(pattern())
    if (!matched || text != "after") panic("string method evaluated arguments first")

    needle = "before"
    replacement = () => {
        needle = "after"
        return "!"
    }
    replaced = "before after".replace(needle, replacement())
    if (replaced != "! after" || needle != "after") {
        panic("later argument changed an earlier owned argument snapshot")
    }
    start = 0
    end = () => {
        start = 4
        return 6
    }
    sliced = "abcdef".slice(start, end())?
    if (sliced != "abcdef" || start != 4) {
        panic("later argument changed an earlier Copy argument")
    }

    change = () => {
        values = [33]
        return 0
    }
    caught = false
    finalized = false
    try {
        values = values.push(change() + Missing()?)
    } catch (err) {
        caught = true
    } finally {
        finalized = true
    }
    if (!caught || !finalized || values.len() != 1 || values[0] != 33) {
        panic("failed argument discarded side effects or skipped finally")
    }

    plain = "界"
    plain += plain.clone()
    try {
        plain += MissingText()?
    } catch (err) {
        if (plain != "界界") panic("failed compound RHS changed lhs")
    } finally {
        if (plain != "界界") panic("finally observed a moved compound lhs")
    }
    text += pattern()
    if (text != "afterbefore!") panic("compound assignment must evaluate RHS first")
    println("collection-order-ok")
    return ok(null)
}
"#;

fn checked_ir(source: &str) -> ir::IrProgram {
    let tokens = Lexer::new(source)
        .tokenize()
        .expect("lex collection fixture");
    let ast = Parser::new(tokens)
        .parse_program()
        .expect("parse collection fixture");
    Checker::new()
        .check(&ast)
        .expect("check collection fixture");
    ir::lower_program(&ast).expect("lower collection fixture")
}

fn native_stdout(label: &str, source: &str) -> Option<String> {
    let program = ir::optimize_program(&checked_ir(source));
    let generated = c::generate_c_source(&program).expect("emit collection C artifact");
    native_c_stdout(label, generated)
}

fn native_c_stdout(label: &str, generated: String) -> Option<String> {
    assert!(!generated.contains("run_source"));
    assert!(!generated.contains("const SOURCE"));
    let temp = TempDir::new(label);
    let path = temp.path().join("collection.c");
    fs::write(&path, generated).expect("write collection C artifact");
    let executable = compile_harness(temp.path(), &path, label)?;
    let output = run_bounded(
        Command::new(executable).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap_or_else(|error| panic!("collection executable did not finish safely: {error}"));
    assert!(
        output.status.success(),
        "collection executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).replace('\r', ""))
}

#[test]
fn native_collection_receiver_snapshot_and_fallible_rhs_match_interpreter() {
    run_source("native-collection-order.ku", RECEIVER_ORDER_SOURCE)
        .expect("interpreter collection evaluation order");
    let Some(stdout) = native_stdout("collection-order", RECEIVER_ORDER_SOURCE) else {
        return;
    };
    assert_eq!(stdout, "collection-order-ok\n");
}

#[test]
fn native_collection_call_arguments_keep_left_to_right_values() {
    let source = r#"
fn First(value: int, ignored: int): int { return value }
fn Invoke(operation: fn(): int, ignored: int): int { return operation() }
fn main() {
    value = 1
    change = () => { value = 2; return 0 }
    println(First(value, change()))
    seed = 10
    operation = () => { return seed }
    replace = () => { operation = () => { return 20 }; return 0 }
    println(Invoke(operation.clone(), replace()))
    println(operation())
}
"#;
    run_source("native-collection-arguments.ku", source).expect("interpreter argument order");
    let Some(stdout) = native_stdout("collection-arguments", source) else {
        return;
    };
    assert_eq!(stdout, "1\n10\n20\n");
}

#[test]
fn native_collection_binary_operands_keep_left_to_right_values() {
    let source = r#"
fn main() {
    value = 1
    change = () => { value = 2; return 3 }
    println(value + change())
    text = "before" + "!"
    replace = () => { text = "after" + "!"; return "suffix" }
    println(text + replace())
    println(text)
}
"#;
    run_source("native-collection-operands.ku", source).expect("interpreter operand order");
    let Some(stdout) = native_stdout("collection-operands", source) else {
        return;
    };
    assert_eq!(stdout, "4\nbefore!suffix\nafter!\n");
}

#[test]
fn native_collection_shared_http_identity_is_not_an_owned_clone() {
    let source = r#"
import http from "std.http"
fn main() {
    app = http.service()
    println(app == http.service())
    println(app == app)
}
"#;
    // Keep the native ABI's existing shared-instance identity. This is a guard
    // against automatically cloning a server as if it were an owned struct,
    // not a claim about interpreter configuration-object equality.
    let source_directory = TempDir::new("collection-shared-identity-source");
    let generated = native_harness::emit_c(source_directory.path(), source);
    let Some(stdout) = native_c_stdout("collection-shared-identity", generated) else {
        return;
    };
    assert_eq!(stdout, "false\ntrue\n");
}

#[test]
fn native_collection_copy_return_is_read_before_its_cell_is_released() {
    let source = r#"
fn ReturnBoxedCopy(): int {
    value = 91
    observe = () => { return value }
    if (observe() != 91) panic("boxed value changed")
    return value
}
fn main() {
    for iteration in 128 {
        if (ReturnBoxedCopy() != 91) panic("return read a released cell")
    }
    println("copy-return-ok")
}
"#;
    run_source("native-collection-copy-return.ku", source).expect("interpreter boxed Copy return");
    let generated = c::generate_c_source(&ir::optimize_program(&checked_ir(source)))
        .expect("emit boxed Copy return");
    let body = generated
        .split_once(" ReturnBoxedCopy(void) {\n")
        .expect("boxed Copy return function")
        .1
        .split_once("\n}\n")
        .expect("boxed Copy return function end")
        .0;
    assert!(body.contains("ku_cell_int_release("), "{body}");
    assert!(
        !body.contains("return (value)->value;"),
        "Copy return must be evaluated before owned cleanup releases its cell"
    );
    let Some(stdout) = native_c_stdout("collection-copy-return", generated) else {
        return;
    };
    assert_eq!(stdout, "copy-return-ok\n");
}

const ORDER_CLEANUP_FUNCTIONS: &str = r#"
fn OrderMissing(): int! { fail "missing number" }
fn OrderMissingText(): str! { fail "missing text" }
fn OrderInvoke(operation: fn(): int, ignored: int): int { return operation() }
fn OrderSnapshotCleanup(): int {
    calls = 0
    operation = () => { calls += 1; return calls }
    caught = 0
    finalized = 0
    try {
        ignored = OrderInvoke(operation.clone(), OrderMissing()?)
    } catch (err) {
        caught += 1
    } finally {
        finalized += 1
    }
    if (calls != 0 || operation() != 1) panic("failed argument invoked or consumed the original closure")
    text = "owned" + " value"
    try {
        ignored = text + OrderMissingText()?
    } catch (err) {
        caught += 1
    } finally {
        if (text != "owned value") panic("failed operand consumed its borrowed lhs")
        finalized += 1
    }
    if (caught != 2 || finalized != 2 || operation() != 2) {
        panic("snapshot cleanup skipped an error, finally, or shared capture")
    }
    return 42
}
"#;

#[test]
fn native_collection_snapshots_drop_on_failed_later_expressions() {
    let source =
        format!("{ORDER_CLEANUP_FUNCTIONS}\nfn main() {{ println(OrderSnapshotCleanup()) }}\n");
    run_source("native-collection-order-cleanup.ku", &source)
        .expect("interpreter cleanup after a later failure");
    let Some(stdout) = native_stdout("collection-order-cleanup", &source) else {
        return;
    };
    assert_eq!(stdout, "42\n");
}

#[test]
fn native_collection_array_field_assignments_keep_the_original_place() {
    let source = r#"
struct Inner { words: [str], values: [int] }
struct Outer { inner: Inner }
fn Missing(): str! { fail "missing" }
fn main(): null! {
    outer = Outer { inner: Inner { words: ["one" + "!", "two" + "!"], values: [1, 2] } }
    outer.inner.words[0] = "changed"
    outer.inner.values[0] = 7
    plain = [1, 2]
    plain[0] = 3
    if (outer.inner.words[0] != "changed" || outer.inner.values[0] != 7 || plain[0] != 3) {
        panic("nested field or plain array write did not update its original place")
    }
    indexes = 0
    order = 0
    right = () => {
        order = order * 10 + 1
        return "owned" + "!"
    }
    index = () => {
        indexes += 1
        order = order * 10 + 2
        return 1
    }
    outer.inner.words[index()] = right()
    if (indexes != 1 || order != 12 || outer.inner.words[1] != "owned!") {
        panic("field index assignment repeated its index or evaluated lhs first")
    }
    caught = false
    try {
        outer.inner.words[index()] = Missing()?
    } catch (err) {
        caught = true
    }
    if (!caught || indexes != 1 || outer.inner.words[1] != "owned!") {
        panic("failed rhs evaluated an index or replaced the previous owner")
    }
    println("collection-places-ok")
    return ok(null)
}
"#;
    run_source("native-collection-places.ku", source)
        .expect("interpreter writable collection places");
    let Some(stdout) = native_stdout("collection-places", source) else {
        return;
    };
    assert_eq!(stdout, "collection-places-ok\n");
}

const MAP_FUNCTIONS: &str = r#"
fn IdentityForMap(value: int): int { return value }
fn MapSnapshots(): int {
    values = [1, 2, 3]
    mapped = values.map((value: int) => {
        values = [9, 9, 9]
        values = [8, 8, 8]
        return value
    })
    if (mapped.len() != 3 || mapped[0] != 1 || mapped[1] != 2 || mapped[2] != 3) {
        panic("map callback replaced its live input buffer")
    }
    if (values[0] != 8) panic("map discarded callback side effects")

    words = ["a" + str(1), "b" + str(2), "c" + str(3)]
    copied = words.map((word: str) => {
        words = ["new" + str(4), "new" + str(5), "new" + str(6)]
        words = ["last" + str(7), "last" + str(8), "last" + str(9)]
        return word
    })
    if (copied.len() != 3 || copied[0] != "a1" || copied[1] != "b2" || copied[2] != "c3") {
        panic("map callback invalidated owned elements")
    }
    if (words[0] != "last7") panic("owned map discarded callback side effects")

    pending = [4, 5]
    factory = () => {
        pending = [8]
        return IdentityForMap
    }
    made = pending.map(factory())
    if (made.len() != 2 || made[0] != 4 || made[1] != 5 || pending[0] != 8) {
        panic("mapper factory ran before the receiver snapshot")
    }
    ordinary = [6, 7]
    doubled = ordinary.map(value => value * 2)
    if (ordinary.len() != 2 || ordinary[0] != 6 || doubled[1] != 14) {
        panic("ordinary map consumed its borrowed receiver")
    }
    return 42
}
"#;

#[test]
fn native_collection_map_keeps_its_snapshot_through_mapper_callbacks() {
    let source = format!("{MAP_FUNCTIONS}\nfn main() {{ println(MapSnapshots()) }}\n");
    run_source("native-collection-map.ku", &source).expect("interpreter map snapshots");
    let Some(stdout) = native_stdout("collection-map", &source) else {
        return;
    };
    assert_eq!(stdout, "42\n");
}

fn intrinsic_count(program: &ir::IrProgram, function: &str, intrinsic: &str) -> usize {
    program
        .functions
        .iter()
        .find(|candidate| candidate.name == function)
        .expect("collection fixture function")
        .blocks
        .iter()
        .flat_map(|block| &block.instructions)
        .filter(|instruction| {
            matches!(
                instruction,
                ir::IrInst::Temp {
                    value: ir::IrExpr {
                        kind: ir::IrExprKind::Call {
                            kind: ir::IrCallKind::Intrinsic(name),
                            ..
                        },
                        ..
                    },
                    ..
                } if name == intrinsic
            )
        })
        .count()
}

#[test]
fn native_collection_reuse_is_limited_to_safe_local_assignments() {
    let source = r#"
struct Row { label: str }
fn One(): int { return 1 }
fn Pure(): int {
    values:[int] = []
    piece = 2
    values = values.push(piece + 1)
    return values.len()
}
fn DifferentLocal(): int {
    values = [1]
    other = values.push(2)
    return values.len() + other.len()
}
fn SelfReference(): int {
    values = [1]
    values = values.push(values[0])
    return values.len()
}
fn CallArgument(): int {
    values = [1]
    values = values.push(One())
    return values.len()
}
fn CloneArgument(): int {
    values:[str] = []
    piece = "owned" + " string"
    values = values.push(piece.clone())
    return values.len()
}
fn AggregateArgument(): int {
    values:[Row] = []
    values = values.push(Row { label: "owned" + " string" })
    return values.len()
}
fn Captured(): int {
    values = [1]
    observe = () => { return values.len() }
    values = values.push(2)
    return observe()
}
fn PlainString(): str {
    text = "start"
    text = text + "!"
    return text
}
fn CompoundString(): str {
    text = "start"
    text += "!"
    return text
}
fn CapturedString(): int {
    text = "start"
    observe = () => { return text.len() }
    text += "!"
    return observe()
}
fn main() {}
"#;
    let program = checked_ir(source);
    assert_eq!(
        intrinsic_count(&program, "Pure", "__ku_array_push_reuse"),
        1
    );
    assert_eq!(
        intrinsic_count(&program, "Captured", "__ku_array_push_reuse"),
        1
    );
    for function in [
        "DifferentLocal",
        "SelfReference",
        "CallArgument",
        "CloneArgument",
        "AggregateArgument",
    ] {
        assert_eq!(
            intrinsic_count(&program, function, "__ku_array_push_reuse"),
            0,
            "{function} must retain pure array.push lowering"
        );
        assert_eq!(intrinsic_count(&program, function, "array.push"), 1);
    }
    assert_eq!(
        intrinsic_count(&program, "CompoundString", "__ku_string_concat_reuse"),
        1
    );
    for function in ["PlainString", "CapturedString"] {
        assert_eq!(
            intrinsic_count(&program, function, "__ku_string_concat_reuse"),
            0
        );
    }
    let generated = c::generate_c_source(&program).expect("emit narrow reuse C artifact");
    assert!(generated.contains("int64_t* data; size_t capacity; } KuArray_int;"));
    assert!(generated.contains("ku_array_push_reuse_int(&values,"));
    assert!(generated.contains("ku_array_push_reuse_int(&(values)->value,"));
    assert!(generated.contains("ku_string_concat_reuse(&text,"));
}

const GROWTH_SOURCE: &str = r#"
struct Entry { labels: [str], text: str }
fn GrowthArray(): int {
    values:[int] = []
    for index in 4096 {
        values = values.push(index)
    }
    if (values.len() != 4096 || values[0] != 0 || values[4095] != 4095) {
        panic("array growth lost values")
    }
    return values.len()
}
fn GrowthString(): int {
    text = "界"
    for index in 4096 {
        text += "x"
    }
    if (text.len() != 4097 || !text.starts_with("界x")) panic("string growth lost bytes")
    return text.len()
}
fn OwnedCopies(): int {
    rows:[[str]] = []
    seed = ["owned" + " string"]
    rows = rows.push(seed)
    rows = rows.push(seed)
    seed[0] = "changed"
    rows[0][0] = "first"
    if (rows[1][0] != "owned string" || seed[0] != "changed") panic("nested push did not deep clone")

    entry = Entry { labels: ["tag" + "!"], text: "name" + "!" }
    entries:[Entry] = []
    entries = entries.push(entry)
    copy = entries.clone()
    entries = entries.push(entry)
    entry.labels[0] = "changed"
    entries[0].text = "first"
    if (copy.len() != 1 || entries.len() != 2 || copy[0].text != "name!") {
        panic("capacity or clone ownership was lost")
    }
    if (entry.labels[0] != "changed") panic("struct field index assignment lost its write")
    if (entries[1].labels[0] != "tag!") {
        panic("pushed struct retained a mutable alias")
    }
    more = entries.push(entry)
    if (entries.len() != 2 || more.len() != 3) panic("ordinary push changed source")
    return 42
}
fn main() {
    GrowthArray()
    GrowthString()
    OwnedCopies()
}
"#;

// Hooks are inserted after the generated standard headers. They count actual
// generated-runtime allocations; libc's own buffering is outside this counter.
const ALLOCATION_HOOKS: &str = r#"
static size_t test_allocations = 0, test_requested_bytes = 0, test_live = 0;
static int test_fail_allocations = 0;
static size_t test_attempts = 0, test_fail_at = 0;
static int test_should_fail(void) {
  test_attempts++;
  return test_fail_allocations || (test_fail_at && test_attempts == test_fail_at);
}
static void* test_malloc(size_t size) {
  if (test_should_fail()) return NULL;
  void* result = malloc(size);
  if (result) { test_allocations++; test_requested_bytes += size; test_live++; }
  return result;
}
static void* test_calloc(size_t count, size_t size) {
  if (test_should_fail()) return NULL;
  void* result = calloc(count, size);
  if (result) { test_allocations++; test_requested_bytes += count * size; test_live++; }
  return result;
}
static void* test_realloc(void* pointer, size_t size) {
  if (test_should_fail()) return NULL;
  int was_null = pointer == NULL;
  void* result = realloc(pointer, size);
  if (result) {
    test_allocations++; test_requested_bytes += size;
    if (was_null) test_live++;
  }
  return result;
}
static void test_free(void* pointer) {
  if (pointer) {
    if (!test_live) { fputs("unbalanced collection free\n", stderr); exit(99); }
    test_live--;
  }
  free(pointer);
}
#define malloc test_malloc
#define calloc test_calloc
#define realloc test_realloc
#define free test_free
"#;

const ALLOCATION_MAIN: &str = r#"
#undef main
static void test_require(int condition, const char* message) {
  if (!condition) { fputs(message, stderr); fputc('\n', stderr); exit(98); }
}
static void test_reset(void) {
  test_require(test_live == 0, "collection allocations remained live");
  test_allocations = 0;
  test_requested_bytes = 0;
}
int main(int argc, char** argv) {
  if (argc == 2) {
    if (strcmp(argv[1], "array-overflow") == 0) {
      KuArray_int invalid = { SIZE_MAX, NULL, 0 };
      ku_array_push_reuse_int(&invalid, 1);
    } else if (strcmp(argv[1], "element-overflow") == 0) {
      KuArray_int invalid = { SIZE_MAX / sizeof(int64_t), NULL, 0 };
      ku_array_push_reuse_int(&invalid, 1);
    } else if (strcmp(argv[1], "string-overflow") == 0) {
      KuString invalid = { NULL, SIZE_MAX, 0, KU_STRING_STATIC };
      ku_string_concat_reuse(&invalid, ku_string_static((const uint8_t*)"x", 1));
    } else if (strcmp(argv[1], "array-oom") == 0) {
      KuArray_int empty = {0};
      test_fail_allocations = 1;
      ku_array_push_reuse_int(&empty, 1);
    } else if (strcmp(argv[1], "string-oom") == 0) {
      KuString empty = {0};
      test_fail_allocations = 1;
      ku_string_concat_reuse(&empty, ku_string_static((const uint8_t*)"x", 1));
    } else if (strcmp(argv[1], "array-realloc-oom") == 0) {
      const int64_t data[] = { 0, 1, 2, 3, 4, 5, 6, 7 };
      KuArray_int full = ku_array_make_int(8, data);
      test_fail_allocations = 1;
      ku_array_push_reuse_int(&full, 8);
    } else if (strcmp(argv[1], "string-realloc-oom") == 0) {
      KuString full = ku_string_concat(ku_string_static((const uint8_t*)"a", 1),
          ku_string_static((const uint8_t*)"b", 1));
      test_fail_allocations = 1;
      ku_string_concat_reuse(&full, ku_string_static((const uint8_t*)"c", 1));
    }
    return 97;
  }
  test_require(GrowthArray() == 4096, "array growth result");
  test_require(test_allocations > 0 && test_allocations <= 16, "array growth was not geometric");
  test_require(test_requested_bytes <= 3 * 4096 * sizeof(int64_t), "array allocation volume was not linear");
  test_reset();

  test_require(GrowthString() == 4097, "string growth result");
  test_require(test_allocations > 0 && test_allocations <= 16, "string growth was not geometric");
  test_require(test_requested_bytes <= 4 * 4099, "string allocation volume was not linear");
  test_reset();

  test_require(OwnedCopies() == 42, "owned copies result");
  test_reset();
  test_require(MapSnapshots() == 42, "map snapshot result");
  test_reset();
  test_require(OrderSnapshotCleanup() == 42, "failed expression snapshot cleanup result");
  test_reset();

  const uint8_t bytes[] = { 'A', 0, 0xe7, 0x95, 0x8c };
  KuString literal = ku_string_static(bytes, sizeof(bytes));
  KuString literal_clone = ku_string_clone(literal);
  test_require(test_allocations == 0, "static clone allocated");
  ku_string_drop(&literal_clone);
  KuString appended = ku_string_concat_reuse(&literal, ku_string_static((const uint8_t*)"!", 1));
  test_require(literal.ptr == NULL && literal.len == 0 && literal.capacity == 0, "string move left source live");
  test_require(appended.storage == KU_STRING_OWNED && appended.ptr != bytes, "static string was not copied before write");
  test_require(appended.len == sizeof(bytes) + 1 && memcmp(appended.ptr, bytes, sizeof(bytes)) == 0
      && appended.ptr[sizeof(bytes)] == '!', "append lost UTF-8 or embedded NUL");
  KuString cloned = ku_string_clone(appended);
  appended = ku_string_concat_reuse(&appended, ku_string_static((const uint8_t*)"x", 1));
  test_require(cloned.len == sizeof(bytes) + 1 && cloned.ptr != appended.ptr, "owned clone shared append storage");
  size_t before_empty_append = test_allocations;
  uint8_t* before_empty_pointer = appended.ptr;
  KuString empty_moved = ku_string_concat_reuse(&appended, ku_string_static((const uint8_t*)"", 0));
  test_require(test_allocations == before_empty_append && empty_moved.ptr == before_empty_pointer,
      "empty RHS append allocated or discarded the existing buffer");
  test_require(appended.ptr == NULL && appended.len == 0 && appended.capacity == 0,
      "empty RHS append did not move-clear its source");
  ku_string_drop(&empty_moved);
  ku_string_drop(&cloned);
  test_reset();

  int64_t* data = (int64_t*)malloc(8 * sizeof(int64_t));
  test_require(data != NULL, "legacy array allocation");
  for (size_t index = 0; index < 8; index++) data[index] = (int64_t)index;
  KuArray_int legacy = { 8, data };
  KuArray_int grown = ku_array_push_reuse_int(&legacy, 8);
  test_require(legacy.len == 0 && legacy.data == NULL && legacy.capacity == 0, "array move left source live");
  test_require(grown.len == 9 && grown.capacity >= 9 && grown.data[8] == 8, "legacy array capacity boundary");
  KuArray_int copy = ku_array_clone_int(grown);
  KuArray_int moved = ku_array_move_int(&copy);
  test_require(copy.len == 0 && copy.data == NULL && copy.capacity == 0, "clone move did not clear capacity");
  ku_array_drop_int(&grown);
  ku_array_drop_int(&moved);
  test_require(grown.capacity == 0 && moved.capacity == 0, "array drop did not clear capacity");
  test_reset();
  test_require(ku_collection_capacity(SIZE_MAX / 2, SIZE_MAX, 1, "test") == SIZE_MAX,
      "capacity saturation overflowed or failed to terminate");
  puts("collection-allocation-ok");
  return 0;
}
"#;

#[test]
fn native_collection_growth_is_geometric_and_ownership_balanced() {
    run_source("native-collection-growth.ku", GROWTH_SOURCE)
        .expect("interpreter checks collection growth fixture");
    let program = ir::optimize_program(&checked_ir(&format!(
        "{GROWTH_SOURCE}\n{MAP_FUNCTIONS}\n{ORDER_CLEANUP_FUNCTIONS}"
    )));
    let generated = c::generate_c_source(&program).expect("emit allocation-counting C artifact");
    let generated = generated.replacen(
        "typedef struct KuString",
        &format!("{ALLOCATION_HOOKS}\n#define main ku_collection_generated_main\ntypedef struct KuString"),
        1,
    );
    assert!(generated.contains("ku_array_push_reuse_int(&values,"));
    assert!(generated.contains("ku_string_concat_reuse(&text,"));
    let temp = TempDir::new("collection-allocations");
    let path = temp.path().join("collection_allocations.c");
    fs::write(&path, format!("{generated}\n{ALLOCATION_MAIN}"))
        .expect("write allocation-counting C harness");
    let Some(executable) = compile_harness(temp.path(), &path, "collection-allocations") else {
        return;
    };
    let output = run_bounded(
        Command::new(&executable).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("bounded allocation-counting executable");
    assert!(
        output.status.success(),
        "allocation or ownership regression:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "collection-allocation-ok\n"
    );
    for (mode, diagnostic) in [
        ("array-overflow", "array allocation is too large"),
        ("element-overflow", "array allocation is too large"),
        ("string-overflow", "string allocation is too large"),
        ("array-oom", "array allocation failed"),
        ("string-oom", "out of memory"),
        ("array-realloc-oom", "array allocation failed"),
        ("string-realloc-oom", "out of memory"),
    ] {
        let output = run_bounded(
            Command::new(&executable).current_dir(temp.path()).arg(mode),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .unwrap_or_else(|error| panic!("{mode} did not terminate safely: {error}"));
        assert_eq!(output.status.code(), Some(1), "{mode} must fail closed");
        assert!(output.stdout.is_empty(), "{mode} must not report success");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{mode} emitted the wrong failure"
        );
    }
}

const CONCAT_OPTIONAL_SOURCE: &str = r#"
struct Piece { text: str, words: [str] }
fn Edge(values: [str]): str | null { return array.first(values) }
fn OptionalLength(value: str | null): int {
    if (value == null) { return -1 }
    return value.len()
}
fn main() {
    empty: [int] = []
    ints = [0, 2]
    joined = array.concat(ints, [3])
    if (joined.len() != 3 || joined[0] != 0 || joined[1] != 2 || joined[2] != 3) panic("concat order")
    joined[0] = 9
    if (ints[0] != 0 || ints.len() != 2) panic("concat changed input")
    if (array.concat(empty, empty).len() != 0) panic("empty concat")

    original = ["before" + "!"]
    change = () => { original = ["after"]; return ["right" + "!"] }
    snapshot = array.concat(original, change())
    if (snapshot[0] != "before!" || snapshot[1] != "right!" || original[0] != "after") panic("concat receiver order")
    words = ["owned" + "!", "last" + "!"]
    copies = array.concat(words, [""])
    first = array.first(words)
    last = array.last(words)
    words = ["replacement"]
    if (copies[0] != "owned!" || copies[1] != "last!") panic("concat result lifetime")
    if (first == null || last == null) panic("nonempty null")
    if (first != null && first != "owned!") panic("first lifetime")
    if (last != null && last != "last!") panic("last lifetime")
    first = null
    if (first != null) panic("nullable assignment")

    zero = array.first([0])
    flag = array.last([false])
    blank = array.first([""])
    if (zero == null || flag == null || blank == null) panic("zero sentinel")
    if (zero != null && zero != 0) panic("zero value")
    if (flag != null && flag != false) panic("false value")
    if (blank != null && blank.len() != 0) panic("empty string value")
    if (array.first(empty) != null || array.last(empty) != null) panic("empty edge")
    if (array.first([0]) != 0 || array.last([false]) != false) panic("nullable payload equality")
    empty_text: [str] = []
    if (OptionalLength(Edge(empty_text)) != -1) panic("empty function return")
    returned = Edge(["returned" + "!"])
    if (OptionalLength(returned) != 9) panic("owned nullable parameter")

    pieces = [Piece { text: "part" + "!", words: ["nested" + "!"] }]
    combined = array.concat(pieces, pieces)
    piece = array.first(pieces)
    pieces = []
    if (combined.len() != 2 || combined[1].text != "part!") panic("struct concat")
    if (piece != null && piece.words[0] != "nested!") panic("struct edge lifetime")
    if (piece == null) panic("struct edge null")
    nested = [["deep" + "!"], ["tail" + "!"]]
    deep = array.first(nested)
    nested_copy = array.concat(nested, nested)
    nested = []
    if (deep != null && deep[0] != "deep!") panic("nested edge lifetime")
    if (nested_copy[3][0] != "tail!") panic("nested concat lifetime")
    singleton: [str] = ["single" + "!"]
    singleton_first = array.first(singleton)
    singleton_last = array.last(singleton)
    if (singleton_first != null && singleton_first != "single!") panic("single first")
    if (singleton_last != null && singleton_last != "single!") panic("single last")
    println("array-optional-ok")
}
"#;

#[test]
fn native_collection_concat_and_nullable_edges_preserve_values_and_owners() {
    run_source("native-array-optional.ku", CONCAT_OPTIONAL_SOURCE)
        .expect("interpreter checks independent array and nullable results");
    let Some(stdout) = native_stdout("array-optional", CONCAT_OPTIONAL_SOURCE) else {
        return;
    };
    assert_eq!(stdout, "array-optional-ok\n");
}

#[test]
fn native_named_patterns_move_owned_fields_and_skip_present_defaults() {
    let source = r#"
struct Details { label: str }
struct Reply { body: str, details: Details, optional: int | null, copied: int }
struct Envelope { reply: Reply, retained: str }

fn Default(): int {
    println("default")
    return 17
}

fn main() {
    envelope = Envelope {
        reply: Reply { body: "owned" + " buffer", details: Details { label: "nested" + " field" }, optional: null, copied: 0 },
        retained: "retained" + " sibling"
    }
    { body: payload, details, optional = Default(), missing = Default(), ...rest } = envelope.reply
    if (payload != "owned buffer" || details.label != "nested field") panic("owned field lost")
    if (optional != null || missing != 17 || rest?.copied != 0) panic("pattern failed")
    if (envelope.retained != "retained sibling") panic("pattern consumed a sibling")
    println("named-pattern-ok")
}
"#;
    let Some(stdout) = native_stdout("named-pattern", source) else {
        return;
    };
    assert_eq!(stdout, "default\nnamed-pattern-ok\n");
}

#[test]
fn native_optional_fields_evaluate_once_and_keep_null_and_zero_distinct() {
    let source = r#"
struct Child { text: str }
struct Parent { count: int, child: Child | null }

fn receiver(present: bool): Parent | null {
    println("receiver")
    if (present) {
        return Parent { count: 0, child: Child { text: "owned" + "!" } }
    }
    return null
}

fn check_view(&parent: Parent | null): bool {
    return parent?.child?.text == "owned!"
}

fn main() {
    present = receiver(true)
    count = present?.count
    if (count == null || count != 0) panic("zero lost its present tag")
    if (!check_view(present)) panic("borrowed nested field")
    text = present?.child?.text.clone()
    present = null
    if (text == null || text != "owned!") panic("owned projection lost its source lifetime")
    if (receiver(false)?.child?.text != null) panic("null receiver selected a field")
    object = { child: { count: 0 }, none: null }
    next = object?.child?.count + 1
    if (next != 1) panic("static object field lost its checked type")
    if (object?.none != null || object?.missing != null) panic("missing object field")
    println("optional-field-ok")
}
"#;
    run_source("optional-field.ku", source).expect("interpreter optional-field contract");
    let temp = TempDir::new("native-optional-field");
    fs::write(temp.path().join("main.ku"), source).expect("write optional-field program");
    let name = if cfg!(windows) { "app.exe" } else { "app" };
    let mut build = Command::new(env!("CARGO_BIN_EXE_ku"));
    build.current_dir(temp.path()).args([
        "build",
        "--backend",
        "c",
        "--release",
        "main.ku",
        "-o",
        name,
    ]);
    let built = run_bounded(&mut build, RUN_TIMEOUT, RUN_LIMITS).expect("bounded optional build");
    let error = String::from_utf8_lossy(&built.stderr);
    if !built.status.success() && error.contains("C compiler not found") {
        return;
    }
    assert!(
        built.status.success(),
        "optional field native build failed: {error}"
    );
    let mut run = Command::new(temp.path().join(name));
    run.current_dir(temp.path());
    let result = run_bounded(&mut run, RUN_TIMEOUT, RUN_LIMITS).expect("bounded optional run");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).replace('\r', ""),
        "receiver\nreceiver\noptional-field-ok\n"
    );
}

#[test]
fn native_collection_nullable_containers_print_and_encode_without_boxing_scalars() {
    let source = r#"
import json from "std.json"
fn main(): null! {
    empty: [int] = []
    println(array.first(empty))
    println(array.first([0]))
    println(array.first([false]))
    first = array.first(["owned" + "!"])
    none: [str] = []
    nested = array.first([["nested" + "!"]])
    println(json.encode(first)?)
    println(json.encode(nested)?)
    println(json.encode({a:array.first(none),b:first.clone(),c:nested.clone()})?)
    if (first != null && first != "owned!") panic("JSON consumed a borrowed nullable root")
    operations = [() => { return 7 }]
    operation = array.first(operations)
    operations = []
    if (operation == null) panic("closure edge")
    if (operation != null && operation() != 7) panic("closure edge owner")
    results = [ok("result" + "!")]
    result = array.last(results)
    results = []
    if (result != null) println(result?)
    dynamic_values = [json.decode("null")?,json.decode("0")?,json.decode("\"owned\"")?]
    dynamic_null = array.first(dynamic_values)
    if (dynamic_null != null) panic("dynamic null became present")
    dynamic_some = array.last(dynamic_values)
    if (dynamic_some == null) panic("dynamic string became null")
    println(json.encode(dynamic_some)?)
    dynamic_some = json.decode("null")?
    if (dynamic_some != null) panic("dynamic null assignment became present")
    println(json.encode(dynamic_values)?)
    return ok(null)
}
"#;
    let mut ast = Parser::new(
        Lexer::new(source)
            .tokenize()
            .expect("lex nullable containers"),
    )
    .parse_program()
    .expect("parse nullable containers");
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:json".into(),
            span: Default::default(),
        }),
    );
    Checker::new()
        .check(&ast)
        .expect("check nullable containers");
    let program =
        ir::optimize_program(&ir::lower_program(&ast).expect("lower nullable containers"));
    let generated = c::generate_c_source(&program).expect("emit flat nullable containers");
    assert!(generated.contains("struct __ku_nullable_type_int { bool is_null; int64_t value; }"));
    let Some(stdout) = native_c_stdout("nullable-containers", generated) else {
        return;
    };
    assert_eq!(stdout, "null\n0\nfalse\n\"owned!\"\n[\"nested!\"]\n{\"a\":null,\"b\":\"owned!\",\"c\":[\"nested!\"]}\nresult!\n\"owned\"\n[null,0,\"owned\"]\n");
}

const OPTIONAL_FAILURE_MAIN: &str = r#"
#undef main
static void optional_require(int condition, const char* message) {
  if (!condition) { fputs(message, stderr); fputc('\n', stderr); exit(98); }
}
static void optional_fault(size_t ordinal) {
  test_attempts = 0; test_fail_at = ordinal; __ku_sync_reset();
}
int main(void) {
  const uint8_t bytes[] = { 'A', 0, 0xe7, 0x95, 0x8c };
  KuString a = ku_string_concat(ku_string_static(bytes, sizeof(bytes)), ku_string_static((const uint8_t*)"!", 1));
  KuString b = ku_string_concat(ku_string_static((const uint8_t*)"B", 1), ku_string_static((const uint8_t*)"!", 1));
  KuString values[] = { a, b };
  KuArray_str source = ku_array_make_str(2, values);
  const size_t baseline = test_live;
  size_t failures = 0, successes = 0;
  /* The output array and all four element allocations are failed in turn. */
  for (size_t ordinal = 1; ordinal <= 6; ordinal++) {
    optional_fault(ordinal);
    KuArray_str result = __ku_array_concat_str(source, source);
    test_fail_at = 0;
    KuSyncExitSignal signal = __ku_sync_take();
    if (signal.kind != KU_SYNC_EXIT_NONE) {
      failures++;
      optional_require(result.len == 0 && !result.data, "partial concat returned live data");
    } else {
      successes++;
      optional_require(result.len == 4 && result.data[0].ptr != source.data[0].ptr,
          "concat did not independently own elements");
      optional_require(result.data[0].len == sizeof(bytes) + 1
          && memcmp(result.data[0].ptr, bytes, sizeof(bytes)) == 0, "concat lost embedded NUL");
    }
    ku_array_drop_str(&result);
    optional_require(test_live == baseline, "concat failure leaked partial elements");
    optional_require(source.len == 2 && source.data[0].len == sizeof(bytes) + 1,
        "concat failure changed the source");
  }
  optional_require(failures == 5 && successes == 1, "concat fault plan did not cover every allocation");
  for (size_t last = 0; last <= 1; last++) {
    optional_fault(1);
    __ku_nullable_type_str result = last ? __ku_array_last_str(source) : __ku_array_first_str(source);
    test_fail_at = 0;
    optional_require(__ku_sync_take().kind != KU_SYNC_EXIT_NONE && result.is_null,
        "edge allocation failure was not reported");
    __ku_nullable_drop_str(&result);
    optional_require(test_live == baseline, "edge allocation failure leaked");
    optional_fault(2);
    result = last ? __ku_array_last_str(source) : __ku_array_first_str(source);
    test_fail_at = 0;
    optional_require(__ku_sync_take().kind == KU_SYNC_EXIT_NONE && !result.is_null,
        "edge could not recover after allocation failure");
    optional_require(result.value.ptr != source.data[last].ptr, "edge borrowed owned input storage");
    __ku_nullable_drop_str(&result);
    optional_require(test_live == baseline, "edge successful result leaked");
  }
  KuArray_str empty = {0};
  optional_fault(1);
  __ku_nullable_type_str edge = __ku_array_first_str(empty);
  optional_require(edge.is_null && test_attempts == 0, "empty edge allocated or accessed data");
  test_fail_at = 0;
  KuArray_str invalid = {SIZE_MAX, source.data, SIZE_MAX};
  optional_fault(0);
  KuArray_str overflow = __ku_array_concat_str(invalid, source);
  optional_require(__ku_sync_take().kind != KU_SYNC_EXIT_NONE && !overflow.data && test_attempts == 0,
      "concat overflow reached allocation or read");
  ku_array_drop_str(&source);
  optional_require(test_live == 0, "array source remained live");
  KuString text = ku_string_concat(ku_string_static((const uint8_t*)"part",4), ku_string_static((const uint8_t*)"!",1));
  KuString word = ku_string_concat(ku_string_static((const uint8_t*)"nested",6), ku_string_static((const uint8_t*)"!",1));
  KuStruct_Piece piece = {text, ku_array_make_str(1, &word)};
  KuArray_struct_Piece pieces = ku_array_make_struct_Piece(1, &piece);
  const size_t piece_baseline = test_live;
  failures = 0; successes = 0;
  for (size_t ordinal = 1; ordinal <= 8; ordinal++) {
    optional_fault(ordinal);
    KuArray_struct_Piece result = __ku_array_concat_struct_Piece(pieces, pieces);
    test_fail_at = 0;
    if (__ku_sync_take().kind != KU_SYNC_EXIT_NONE) {
      failures++;
      optional_require(!result.data && result.len == 0, "partial struct concat escaped");
    } else {
      successes++;
      optional_require(result.len == 2 && result.data[1].words.data[0].len == 7,
          "deep concat lost a nested field");
    }
    ku_array_drop_struct_Piece(&result);
    optional_require(test_live == piece_baseline, "partial struct clone leaked");
  }
  optional_require(failures == 7 && successes == 1, "nested fault plan did not cover every allocation");
  for (size_t ordinal = 1; ordinal <= 4; ordinal++) {
    optional_fault(ordinal);
    __ku_nullable_type_struct_Piece result = __ku_array_first_struct_Piece(pieces);
    test_fail_at = 0;
    int failed = __ku_sync_take().kind != KU_SYNC_EXIT_NONE;
    optional_require(failed == (ordinal <= 3) && result.is_null == failed,
        "partial struct edge failure contract");
    __ku_nullable_drop_struct_Piece(&result);
    optional_require(test_live == piece_baseline, "partial struct edge leaked");
  }
  ku_array_drop_struct_Piece(&pieces);
  optional_require(test_live == 0, "deep array owners remained live");
  puts("array-optional-faults-ok");
  return 0;
}
"#;

#[test]
fn native_collection_array_results_rollback_every_partial_allocation() {
    let program = ir::optimize_program(&checked_ir(CONCAT_OPTIONAL_SOURCE));
    let generated = c::generate_c_source(&program).expect("emit typed array failure artifact");
    let generated = generated.replacen(
        "typedef struct KuString",
        &format!("{ALLOCATION_HOOKS}\n#define main ku_collection_generated_main\ntypedef struct KuString"),
        1,
    );
    let Some(stdout) = native_c_stdout(
        "array-optional-faults",
        format!("{generated}\n{OPTIONAL_FAILURE_MAIN}"),
    ) else {
        return;
    };
    assert_eq!(stdout, "array-optional-faults-ok\n");
}

#[test]
fn native_collection_task_results_keep_budgeted_owners_across_await() {
    let source = r#"
async fn Tick(): int! { return ok(1) }
async fn Integers(): null! {
    values = [0, 2]
    joined = array.concat(values, [3])
    zero = array.first(values)
    empty: [int] = []
    absent = array.last(empty)
    flag = array.first([false])
    if (absent != null) fail "empty Task array edge"
    println(absent)
    if (zero == null || flag == null) fail "Task sentinel collision"
    if (zero != null) println(zero)
    if (flag != null) println(flag)
    if (values.len() != 2 || joined.len() != 3) fail "Task concat consumed input"
    println(joined.try_get(2)?)
    return ok(null)
}
async fn Size(value: str | null): int! {
    if (value == null) { return ok(-1) }
    return ok(value.len())
}
async fn Text(value: str | null): str! {
    if (value == null) fail "empty"
    return ok(value)
}
async fn Strings(): null! {
    words = ["owned" + "!", "last" + "!"]
    first = array.first(words)
    last = array.last(words)
    joined = array.concat(words, [""])
    marker = Tick()
    marker_value = (await marker)?
    words = []
    if (first == null || last == null) fail "Task string edge null"
    if (first != null) println(first)
    if (last != null) println(last)
    println(joined.try_get(1)?)
    println((await Size(first))?)
    empty: [str] = []
    println((await Size(array.first(empty)))?)
    text_edge = array.first(joined)
    println((await Text(text_edge))?)
    return ok(null)
}
async fn main(): null! {
    done = (await Integers())?
    done = (await Strings())?
    return ok(null)
}
"#;
    let ast = Parser::new(Lexer::new(source).tokenize().expect("lex Task arrays"))
        .parse_program()
        .expect("parse Task arrays");
    Checker::new().check(&ast).expect("check Task arrays");
    let native = ir::task_lower::lower_program(&ast).expect("lower real Task array program");
    let generated = c::generate_native_task_c_source(&native, &Default::default())
        .expect("emit budgeted Task arrays");
    assert!(generated.contains("ku_task_host_allocate_array"));
    assert!(generated.contains("ku_task_host_clone_string"));
    let Some(stdout) = native_c_stdout("task-array-optional", generated) else {
        return;
    };
    assert_eq!(
        stdout,
        "null\n0\nfalse\n3\nowned!\nlast!\nlast!\n6\n-1\nowned!\n"
    );
}
