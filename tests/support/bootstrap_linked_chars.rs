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
    assert!(source.contains("ku_linked_view_str_array_valid(a1)"));
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

    // An owned character array needs its carrier and destructor, but has
    // no imported array header to validate.
    let local = INPUT.replace(", &view: [str]", "");
    fs::write(cwd.join("iterate.ku"), local).unwrap();
    let emitted = generate();
    assert_ok(&emitted);
    let source = String::from_utf8(emitted.stdout).unwrap();
    assert!(source.contains("ku_managed_source_drop"));
    assert!(!source.contains("ku_linked_view_str_array_valid"));

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

const RECORD_SOURCE: &str = r#"struct Item { text: str }
fn Produce(): [Item]! { return ok([]) }
fn Observe(&item: Item): null! { return ok(null) }

fn Drain(): [Item]! {
    result: [Item] = []
    for item in Produce()? {
        Observe(item)?
        result = result.push(item)
    }
    return ok(result)
}

fn Reusable(): null! {
    items = Produce()?
    for item in items { Observe(item)? }
    return ok(null)
}
"#;

const RECORD_DRIVER: &str = r#"
static size_t length, produced, seen;
static bool fail_on_second;
static uint8_t *strings[3];

__RESULT__ __PRODUCE__(void) {
    ++produced;
    __ARRAY__ array = {0};
    if (length) {
        array = (__ARRAY__){length, malloc(length * sizeof(*array.data)), length};
        assert(array.data);
        for (size_t i = 0; i < length; ++i) {
            strings[i] = malloc(1);
            assert(strings[i]);
            strings[i][0] = (uint8_t)('a' + i);
            array.data[i] = (__ELEMENT__){
                (KuProjectionString){strings[i], 1, 1, KU_PROJECTION_STRING_OWNED}
            };
        }
    }
    return (__RESULT__){.ok = true, .value = array};
}

KuBootstrapResultNull __OBSERVE__(const __ELEMENT__* item) {
    assert(seen < length && item->f0.len == 1 && item->f0.ptr == strings[seen]);
    assert(item->f0.ptr[0] == (uint8_t)('a' + seen));
    ++seen;
    if (fail_on_second && seen == 2) {
        KuBootstrapResultNull result = {0};
        result.error.code = (KuProjectionString){
            (uint8_t*)"stop", 4, 0, KU_PROJECTION_STRING_STATIC
        };
        return result;
    }
    return (KuBootstrapResultNull){.ok = true};
}

static void verify_failed_append(void) {
    if (produced != 1 || seen != 1 || attempts != 5 || outstanding()) _Exit(88);
}

int main(int argc, char **argv) {
    (void)argv;
    if (argc > 1) {
        length = 3;
        fail_after = 4;
        assert(atexit(verify_failed_append) == 0);
        (void)__ENTRY__();
        return 89;
    }
    __RESULT__ empty = __ENTRY__();
    assert(empty.ok && empty.value.len == 0 && !empty.value.data);
    assert(produced == 1 && !seen && !attempts && !outstanding());

    length = 3;
    __RESULT__ output = __ENTRY__();
    assert(produced == 2 && seen == 3 && output.ok && output.value.len == 3);
    assert(attempts == 5 && outstanding() == 4);
    for (size_t i = 0; i < length; ++i) {
        assert(output.value.data[i].f0.ptr == strings[i]);
        test_free(output.value.data[i].f0.ptr);
    }
    test_free(output.value.data);
    assert(!outstanding());

    seen = 0;
    fail_on_second = true;
    __RESULT__ stopped = __ENTRY__();
    assert(produced == 3 && seen == 2 && !stopped.ok && !stopped.value.data);
    assert(stopped.error.code.len == 4 && !memcmp(stopped.error.code.ptr, "stop", 4));
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_owned_record_result_iteration(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("record-iteration.ku"), RECORD_SOURCE).unwrap();
    fs::write(cwd.join("modules.txt"), "record-iteration.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Drain\n").unwrap();
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
    let (carrier, _) = source
        .lines()
        .find_map(|line| {
            let declaration = line.strip_prefix("extern ")?;
            let (carrier, tail) = declaration.split_once(' ')?;
            carrier
                .starts_with("KuBootstrapResult_KuArray_")
                .then_some((carrier, tail))
        })
        .expect("owned array Result declaration");
    let array = carrier.strip_prefix("KuBootstrapResult_").unwrap();
    let element = array.strip_prefix("KuArray_").unwrap();
    let driver = RECORD_DRIVER
        .replace("__RESULT__", carrier)
        .replace("__ARRAY__", array)
        .replace("__ELEMENT__", element)
        .replace("__ENTRY__", symbol(&format!("{carrier} ")))
        .replace("__PRODUCE__", symbol(&format!("extern {carrier} ")))
        .replace("__OBSERVE__", symbol("extern KuBootstrapResultNull "));
    let path = cwd.join("owned-record-iteration.c");
    let binary = cwd.join(format!(
        "owned-record-iteration{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{source}{driver}")).unwrap();
    compile_projection(&path, &binary, cwd);
    assert_ok(&execute(&mut Command::new(&binary), cwd, 10));
    let failed = execute(Command::new(binary).arg("inject-append"), cwd, 10);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(failed.stderr).unwrap().trim(),
        "array allocation failed"
    );

    fs::write(cwd.join("function.txt"), "Reusable\n").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("|unsupported_linked_for|"));
}

pub(super) fn assert_fresh_cloned_record_iteration(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("cloned-record-iteration.ku"),
        r#"struct Entry { text: str }
struct Batch { entries: [Entry], label: str }
struct Outcome { entries: [Entry], label: str, bytes: int }
fn Visit(item: Entry, position: int, fail_at: int): int! {
    if (position == fail_at) { fail { domain: "loop", code: "after_first", message: "stop" } }
    return ok(item.text.len())
}
fn Snapshot(batch: Batch, fail_at: int): Outcome! {
    index = 0
    bytes = 0
    for item in batch.entries.clone() {
        index = index + 1
        amount = Visit(item, index, fail_at)?
        bytes = bytes + amount
    }
    retained = batch.entries
    label = batch.label
    return ok(Outcome { entries: retained, label: label, bytes: bytes })
}
fn Named(batch: Batch): null! {
    names = batch.entries.clone()
    for item in names { Visit(item, 0, 0)? }
    return ok(null)
}
fn Produce(batch: Batch): [Entry]! {
    entries = batch.entries.clone()
    return ok(entries)
}
fn Checked(batch: Batch): int! {
    bytes = 0
    for item in Produce(batch)? {
        amount = Visit(item, 0, 1)?
        bytes = bytes + amount
    }
    return ok(bytes)
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "cloned-record-iteration.ku\n").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    fs::write(cwd.join("function.txt"), "Snapshot").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let (result, signature) = code
        .lines()
        .find_map(|line| line.split_once(" ku_linked_null_"))
        .unwrap();
    let (entry, parameters) = signature.split_once('(').unwrap();
    let (batch, _) = parameters.split_once(" a0,").unwrap();
    let outcome = result.strip_prefix("KuBootstrapResult_").unwrap();
    let element = code
        .lines()
        .find_map(|line| line.split_once(" } KuArray_")?.1.strip_suffix(';'))
        .unwrap();
    let visit = code
        .lines()
        .find_map(|line| line.strip_prefix("extern KuBootstrapResult_int64_t "))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let host = r#"
static int calls;
static uint8_t *original_text[3];
static KuProjectionString owned(uint8_t byte, size_t len) {
    uint8_t *data = malloc(len);
    assert(data);
    memset(data, byte, len);
    return (KuProjectionString){data, len, len, KU_PROJECTION_STRING_OWNED};
}
KuBootstrapResult_int64_t VISIT(ELEMENT entry, int64_t position, int64_t fail_at) {
    assert(position == ++calls && position >= 1 && position <= 3);
    assert(entry.f0.ptr != original_text[position - 1] && entry.f0.len == (size_t)position);
    ku_projection_string_drop(&entry.f0);
    if (position == fail_at) return (KuBootstrapResult_int64_t){
        .error.code = {(uint8_t*)"after_first", 11, 0, KU_PROJECTION_STRING_STATIC}};
    return (KuBootstrapResult_int64_t){.ok = true, .value = position};
}
static void check(int64_t fail_at) {
    int start = attempts;
    calls = 0;
    ELEMENT *original = malloc(3 * sizeof(*original));
    assert(original);
    BATCH input = {.f0 = {3, original, 3}, .f1 = owned('z', 1)};
    uint8_t *label = input.f1.ptr;
    for (size_t i = 0; i < 3; ++i) {
        original[i].f0 = owned((uint8_t)('a' + i), i + 1);
        original_text[i] = original[i].f0.ptr;
    }
    RESULT result = ENTRY(input, fail_at);
    assert(attempts - start == 9);
    if (fail_at) {
        assert(!result.ok && calls == 2 && !outstanding());
        assert(result.error.code.len == 11 && !memcmp(result.error.code.ptr, "after_first", 11));
    } else {
        assert(result.ok && calls == 3 && result.value.f2 == 6);
        assert(result.value.f0.data == original && result.value.f1.ptr == label);
        for (size_t i = 0; i < 3; ++i) assert(result.value.f0.data[i].f0.ptr == original_text[i]);
        DROP(&result.value);
        assert(!outstanding());
    }
}
int main(void) {
    (void)test_realloc;
    check(0);
    check(2);
    check(0);
    return 0;
}
"#
    .replace("ENTRY", &format!("ku_linked_null_{entry}"))
    .replace("RESULT", result)
    .replace("BATCH", batch)
    .replace("ELEMENT", element)
    .replace("VISIT", visit)
    .replace("DROP", &format!("ku_drop_{outcome}"));
    let c = cwd.join("cloned-record-iteration.c");
    let exe = cwd.join(format!(
        "cloned-record-iteration{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{host}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 10));

    fs::write(cwd.join("function.txt"), "Named").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("|unsupported_linked_for|"));
}
