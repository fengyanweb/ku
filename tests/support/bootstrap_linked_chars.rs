use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use std::{fs, path::Path, process::Command};

const INPUT: &str = r#"import { string } from "std"
fn Observe(&character: str): null! { return ok(null) }
fn Iterate(&source: str, &view: [str]): null! {
    for item in string.chars(source.clone()) {
        Observe(item)?
    }
    return ok(null)
}
"#;

const EXTRA_ALLOCATOR: &str = r#"
static void* test_calloc(size_t count, size_t size) {
    assert(!size || count <= SIZE_MAX / size);
    const size_t total = count * size;
    void* data = test_malloc(total);
    if (data) memset(data, 0, total);
    return data;
}
#define calloc test_calloc
"#;

const DRIVER: &str = r#"
static unsigned seen;
static bool fail_on_second;
static const uint8_t bytes[] = {'a', 0xe4, 0xb8, 0xad, 0xf0, 0x9f, 0x99, 0x82, 0, '/'};
static const size_t lengths[] = {1, 3, 4, 1, 1};
static size_t offset;

KuBootstrapResultNull __OBSERVE__(const KuProjectionString* value) {
    assert(seen < sizeof(lengths) / sizeof(lengths[0]));
    assert(value->len == lengths[seen]);
    assert(!memcmp(value->ptr, bytes + offset, value->len));
    assert(value->ptr != bytes + offset);
    offset += value->len;
    ++seen;
    if (fail_on_second && seen == 2) {
        KuBootstrapResultNull checked = {0};
        checked.error.code = (KuProjectionString){(uint8_t*)"stop", 4u, 0u, KU_PROJECTION_STRING_STATIC};
        return checked;
    }
    return (KuBootstrapResultNull){.ok = true};
}
static void after_partial_array(void) {
    if (seen || outstanding() || attempts != 3) _Exit(82);
}
int main(int argc, char** argv) {
    (void)argv;
    (void)test_realloc;
    const KuProjectionString value = {(uint8_t*)bytes, sizeof(bytes), 0u, KU_PROJECTION_STRING_STATIC};
    const KuProjectionString empty = {0};
    const KuGuardStringArray empty_view = {0};
    if (argc > 1) {
        assert(atexit(after_partial_array) == 0);
        // The array and the first non-ASCII character succeed. Static ASCII
        // requires no allocation. Fail on the second non-ASCII character.
        fail_after = 2;
        (void)__ENTRY__(&value, &empty_view);
        return 83;
    }
    assert(__ENTRY__(&empty, &empty_view).ok && seen == 0 && !outstanding());
    assert(__ENTRY__(&value, &empty_view).ok && seen == 5 && offset == sizeof(bytes) && !outstanding());
    seen = 0;
    offset = 0;
    fail_on_second = true;
    KuBootstrapResultNull result = __ENTRY__(&value, &empty_view);
    assert(!result.ok && seen == 2 && offset == 4 && !outstanding());
    assert(result.error.code.len == 4 && !memcmp(result.error.code.ptr, "stop", 4));
    assert(value.ptr == bytes);
    return 0;
}
"#;

pub(super) fn assert_owned_characters(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("iterate.ku"), INPUT).unwrap();
    fs::write(cwd.join("modules.txt"), "iterate.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Iterate\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null\n").unwrap();

    let generate = || execute(&mut Command::new(seed), cwd, 30);
    let emitted = generate();
    assert_ok(&emitted);
    let source = String::from_utf8(emitted.stdout).unwrap();
    let symbol = |prefix| {
        source
            .lines()
            .find_map(|line| line.strip_prefix(prefix)?.split_once('('))
            .map(|(name, _)| name)
            .expect("generated typed entry or callback")
    };
    let driver = DRIVER
        .replace("__ENTRY__", symbol("KuBootstrapResultNull "))
        .replace("__OBSERVE__", symbol("extern KuBootstrapResultNull "));
    let path = cwd.join("owned-characters.c");
    let binary = cwd.join(format!("owned-characters{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &path,
        format!("{CHECKED_ARRAY_ALLOCATOR}{EXTRA_ALLOCATOR}{source}{driver}"),
    )
    .unwrap();
    compile_projection(&path, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 10));
    let failed = execute(Command::new(binary).arg("inject-char"), cwd, 10);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(failed.stderr).unwrap().trim(),
        "out of memory"
    );

    // A non-stdlib iterable is not a supported character iterator.
    let changed = INPUT.replace("string.chars(source.clone())", "source");
    fs::write(cwd.join("iterate.ku"), changed).unwrap();
    let rejected = generate();
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8(rejected.stderr)
        .unwrap()
        .contains("|unsupported_linked_for|"));
}

pub(super) fn assert_owned_copy_ints(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("iterate-ints.ku"),
        r#"struct Snapshot { source: [int] visited: [int] }
fn Inspect(input: [int]): Snapshot! {
    source = input
    visited: [int] = []
    for item in source {
        visited = visited.push(item)
        source = source.push(item + 10)
    }
    return ok(Snapshot { source: source, visited: visited })
}
fn Refuse(&view: [int]): null! {
    for item in view { }
    return ok(null)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "iterate-ints.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Inspect").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = text
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, _) = signature.split_once('(').unwrap();
    assert_eq!(text.matches("memcpy(copy,").count(), 1);
    let driver = r#"
static void after_oom(void) {
    if (outstanding() || attempts != 2) _Exit(82);
}
int main(int argc, char **argv) {
    (void)argv;
    RESULT empty = ENTRY((KuArrayInt){0});
    assert(empty.ok && !empty.value.f0.len && !empty.value.f1.len && !attempts);
    int64_t *data = malloc(2 * sizeof(*data));
    data[0] = 2; data[1] = 7;
    if (argc > 1) {
        assert(!atexit(after_oom));
        fail_after = 0;
    }
    RESULT read = ENTRY((KuArrayInt){2, data, 2});
    assert(read.ok && read.value.f0.len == 4 && read.value.f1.len == 2);
    assert(read.value.f0.data[2] == 12 && read.value.f0.data[3] == 17);
    assert(read.value.f1.data[0] == 2 && read.value.f1.data[1] == 7);
    ku_linked_view_int_array_drop(&read.value.f0);
    ku_linked_view_int_array_drop(&read.value.f1);
    assert(!outstanding());
    return 0;
}
"#
    .replace("RESULT", result)
    .replace("ENTRY", &format!("ku_linked_null_{entry}"));
    let path = cwd.join("copy-int-iteration.c");
    let binary = cwd.join(format!(
        "copy-int-iteration{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{text}{driver}")).unwrap();
    compile_projection(&path, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 15));
    let exhausted = execute(Command::new(binary).arg("snapshot-allocation"), cwd, 15);
    assert_eq!(exhausted.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&exhausted.stderr).trim(),
        "out of memory"
    );
    fs::write(cwd.join("function.txt"), "Refuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("linked_int_for_borrowed"));
}
