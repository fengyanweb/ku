//! Exact allocation and lifetime checks for native synchronous borrowing.
//! Timing is deliberately not a pass/fail signal.

#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

const SOURCE: &str = r#"
struct User { name: str, tags: [str] }

fn Read(&text: str): int { return text.byte_len() }
fn Copy(&text: str): str { return text.clone() }
fn MakeUser(&text: str): User { return User { name: text.clone(), tags: ["Ku"] } }
fn MakeArray(&text: str): [str] { return [text.clone()] }
fn Encode(&users: [User]): str! { return json.stringify(users) }
fn AlwaysFail(): int! { return err("expected") }
fn AbortTarget(&text: str, value: int): int { return text.byte_len() + value }
fn CheckAbortStage(stage: int): int { return stage }
fn TimeoutCopy(&text: str): str { return text.clone() }
fn TimeoutArgument(value: int): int { return value }
fn TimeoutTarget(&text: str, value: int): int { return value }
fn CheckTimeoutFinally(stage: int): int { return stage }

fn TimeoutBorrow(&seed: str): int {
    try { return TimeoutTarget(TimeoutCopy(seed), TimeoutArgument(7)) }
    finally { CheckTimeoutFinally(2) }
    return -1
}

fn AbortArgumentBurst(&seed: str, rounds: int): int {
    caught = 0
    finalized = 0
    index = 0
    while (index < rounds) {
        try { AbortTarget(Copy(seed), AlwaysFail()?) }
        catch(error) { CheckAbortStage(1) caught++ }
        finally { CheckAbortStage(2) finalized++ }
        index++
    }
    if (caught != rounds || finalized != rounds) { panic("aborted argument control flow") }
    return caught + finalized
}

fn NestedAbortArgumentBurst(&seed: str, rounds: int): int {
    caught = 0
    finalized = 0
    index = 0
    while (index < rounds) {
        try { AbortTarget(Copy(seed), AbortTarget(Copy(seed), AlwaysFail()?)) }
        catch(error) { CheckAbortStage(1) caught++ }
        finally { CheckAbortStage(2) finalized++ }
        index++
    }
    if (caught != rounds || finalized != rounds) { panic("nested aborted argument control flow") }
    return caught + finalized
}

fn RecoverInternally(): int {
    try { AlwaysFail()? } catch(error) { return 1 }
    return 0
}

fn RetainedArgument(&seed: str): int {
    return Read(Copy(seed)) + AbortTarget(Copy(seed), RecoverInternally())
}

fn Measure(&users: [User]): int {
    total = 0
    index = 0
    while (index < 256) {
        total += Read(users[0].name)
        total += Read(users[0].tags[0])
        index++
    }
    return total
}

fn Burst(&seed: str, rounds: int): int {
    total = 0
    index = 0
    while (index < rounds) {
        total += Read(Copy(seed))
        index++
    }
    return total
}

fn ProjectedBurst(&seed: str, rounds: int): int {
    total = 0
    index = 0
    while (index < rounds) {
        total += Read(MakeUser(seed).name)
        total += Read(MakeArray(seed)[0])
        index++
    }
    return total
}

fn main() {}
"#;

#[test]
fn borrow_native_nested_reads_allocate_nothing_and_temporaries_drop_after_call() {
    let directory = TempDir::new("borrow-allocation");
    let generated = emit_c(directory.path(), SOURCE);
    assert!(generated.contains("typedef struct KuString {"));
    assert!(generated.contains("int64_t Measure(const KuArray_struct_User* users)"));
    assert!(generated.contains("int64_t Burst(const KuString* seed, int64_t rounds)"));
    let checkpoint = "int64_t CheckAbortStage(int64_t stage) {";
    let abort_target = "int64_t AbortTarget(const KuString* text, int64_t value) {";
    assert!(generated.contains(checkpoint) && generated.contains(abort_target));
    let mut harness = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOK}\n{JSON_FAILURE_HOOK}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        )
        .replacen(
            checkpoint,
            &format!("{checkpoint}\n  ku_borrow_check_abort(stage);"),
            1,
        )
        .replacen(
            abort_target,
            &format!("{abort_target}\n  ku_borrow_abort_target_calls++;"),
            1,
        );
    // Freeze only this generated fixture's clock. Cancellation is injected at
    // exact function boundaries, and finite finally cleanup cannot accidentally
    // exhaust its grace budget because the test host was descheduled.
    let clock_signature = "static unsigned long long __ku_handler_now_ms(void) {";
    let clock_start = harness.find(clock_signature).expect("native timeout clock");
    let clock_end = clock_start
        + harness[clock_start..]
            .find("\n}\n")
            .expect("native timeout clock body")
        + "\n}\n".len();
    harness.replace_range(
        clock_start..clock_end,
        &format!("{clock_signature}\n  return 1000ULL;\n}}\n{TIMEOUT_HOOK}\n"),
    );
    for (signature, hook) in [
        (
            "KuString TimeoutCopy(const KuString* text) {",
            "ku_borrow_timeout_copy_calls++; ku_borrow_timeout_mark(1);",
        ),
        (
            "int64_t TimeoutArgument(int64_t value) {",
            "ku_borrow_timeout_argument_calls++; ku_borrow_timeout_mark(2);",
        ),
        (
            "int64_t TimeoutTarget(const KuString* text, int64_t value) {",
            "ku_borrow_timeout_target_calls++; ku_borrow_timeout_validate_text(*text); ku_borrow_timeout_mark(3);",
        ),
        (
            "int64_t CheckTimeoutFinally(int64_t stage) {",
            "ku_borrow_check_timeout_finally(stage);",
        ),
    ] {
        assert!(harness.contains(signature), "missing timeout hook: {signature}");
        harness = harness.replacen(signature, &format!("{signature}\n  {hook}"), 1);
    }
    harness.push_str(WRAPPER);
    let source = directory.path().join("borrow-allocation.c");
    fs::write(&source, harness).expect("write borrow allocation fixture");
    let Some(executable) = compile_harness(directory.path(), &source, "borrow-allocation") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("borrow allocation fixture must finish within its deadline");
    assert!(
        output.status.success(),
        "borrow allocation gate failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    assert!(stdout.contains("borrow bytes=16 reads=512 allocations=0"));
    assert!(stdout.contains("borrow bytes=65536 reads=512 allocations=0"));
    assert!(stdout.contains("borrow aborted temporary bytes=16 rounds=256"));
    assert!(stdout.contains("borrow aborted temporary bytes=65536 rounds=256"));
    for bytes in [16, 65536] {
        for mode in 0..=3 {
            assert!(stdout.contains(&format!("borrow timeout bytes={bytes} mode={mode}")));
        }
    }
    assert!(stdout.ends_with("borrow allocation closed loop\n"));
    assert!(output.stderr.is_empty());
    eprint!("{stdout}");
}

#[test]
fn borrow_native_json_reads_request_storage_without_input_copies() {
    let directory = TempDir::new("borrow-json-request");
    let generated = emit_c(
        directory.path(),
        r#"
fn DecodeBody(&req: HttpRequest): int! { value = json.decode(req.body)? return value.as_int() }
fn ParseBody(&req: HttpRequest): int! { value = json.parse(req.body)? return value.as_int() }
fn TryParseBody(&req: HttpRequest): int! { value = json.try_parse(req.body)? return value.as_int() }
fn DecodeOwnedBody(req: HttpRequest): int! { value = json.decode(req.body)? return value.as_int() }
fn MakeRequest(req: HttpRequest): HttpRequest { return req }
fn DecodeTemporaryBody(req: HttpRequest): int! { value = json.decode(MakeRequest(req).body)? return value.as_int() }
fn ReadRequestFields(&req: HttpRequest): int { return req.method.byte_len() + req.path.byte_len() + req.body.byte_len() }
fn DecodeText(&text: str): str! { value = json.decode(text)? return value.as_str() }
fn LegacyInput(req: HttpRequest): str { return string.replace(req.body, "x", "y") }
fn main() {}
"#,
    );
    let legacy = generated
        .split("KuString LegacyInput(KuStruct___ku_http_request req) {")
        .nth(1)
        .expect("legacy stdlib input definition")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(legacy.contains("ku_string_clone"), "{legacy}");
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
    harness.push_str(
        r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
#define CHECK(value) do { if (!(value)) { fprintf(stderr, "JSON borrow check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
int main(void) {
  const size_t bytes = 65536;
  KuStruct___ku_http_request request = {0};
  request.method = ku_string_static((const uint8_t*)"POST", 4);
  request.path = ku_string_static((const uint8_t*)"/json", 5);
  request.body = (KuString){ (uint8_t*)ku_perf_malloc(bytes), bytes, bytes, KU_STRING_OWNED };
  CHECK(request.body.ptr);
  memset(request.body.ptr, ' ', bytes);
  memcpy(request.body.ptr + bytes - 3, "123", 3);
  uint8_t* original = request.body.ptr;
  const size_t baseline_live = ku_perf_live_allocations, baseline_bytes = ku_perf_live_bytes;
  ku_perf_calls = 0;
  for (size_t round = 0; round < 64; round++) {
    KuResult_int decoded = DecodeBody(&request);
    KuResult_int parsed = ParseBody(&request);
    KuResult_int tried = TryParseBody(&request);
    CHECK(decoded.ok && parsed.ok && tried.ok);
    CHECK(decoded.value == 123 && parsed.value == 123 && tried.value == 123);
    CHECK(ReadRequestFields(&request) == (int64_t)(bytes + 9));
  }
  CHECK(!ku_perf_calls && !ku_perf_overflow);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  CHECK(request.body.ptr == original && request.body.len == bytes && request.body.ptr[0] == ' ');
  CHECK(!memcmp(request.body.ptr + bytes - 3, "123", 3));

  /* The ordinary owning handler must also borrow its body for the parser,
     then release the request once at the normal owner boundary. */
  KuResult_int (*owning_readers[2])(KuStruct___ku_http_request) = { DecodeOwnedBody, DecodeTemporaryBody };
  for (size_t index = 0; index < 2; index++) {
    KuStruct___ku_http_request owned = request;
    owned.body = ku_string_clone(request.body);
    CHECK(owned.body.ptr != original && ku_perf_live_allocations > baseline_live);
    ku_perf_calls = 0;
    KuResult_int decoded_owned = owning_readers[index](owned);
    CHECK(decoded_owned.ok && decoded_owned.value == 123 && !ku_perf_calls);
    owned = (KuStruct___ku_http_request){0};
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  }

  const char* bad = "{\"a\":[\"owned\",]}";
  memset(request.body.ptr, ' ', bytes);
  memcpy(request.body.ptr + bytes - strlen(bad), bad, strlen(bad));
  for (size_t round = 0; round < 64; round++) {
    KuResult_int rejected[3] = { DecodeBody(&request), ParseBody(&request), TryParseBody(&request) };
    for (size_t index = 0; index < 3; index++) {
      CHECK(!rejected[index].ok);
      CHECK(rejected[index].error.domain.len == 4 && !memcmp(rejected[index].error.domain.ptr, "json", 4));
      CHECK(rejected[index].error.code.len == 11 && !memcmp(rejected[index].error.code.ptr, "parse_error", 11));
      ku_error_drop(&rejected[index].error);
    }
    CHECK(request.body.ptr == original && request.body.len == bytes);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  }
  ku_drop_struct___ku_http_request(&request);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);

  const char* encoded = "\"owned\\u0000tail\"";
  KuString input = ku_string_clone(ku_string_static((const uint8_t*)encoded, strlen(encoded)));
  KuResult_str text = DecodeText(&input);
  CHECK(text.ok && text.value.len == 10 && text.value.ptr != input.ptr);
  ku_string_drop(&input);
  CHECK(!memcmp(text.value.ptr, "owned\0tail", 10));
  ku_string_drop(&text.value);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  puts("JSON borrowed input copies=0; parser outputs and error cleanup verified");
  return 0;
}
"#,
    );
    let source = directory.path().join("borrow-json-request.c");
    fs::write(&source, harness).expect("write JSON borrowed request fixture");
    let Some(executable) = compile_harness(directory.path(), &source, "borrow-json-request") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("JSON borrowed request fixture must finish within its deadline");
    assert!(
        output.status.success(),
        "JSON borrow allocation gate failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("JSON borrowed input copies=0"));
    assert!(output.stderr.is_empty());
}

const JSON_FAILURE_HOOK: &str = r#"
static size_t ku_borrow_json_allocation_calls = 0, ku_borrow_fail_json_allocation = 0;
static size_t ku_borrow_abort_baseline_live = 0, ku_borrow_abort_baseline_bytes = 0;
static size_t ku_borrow_abort_caught = 0, ku_borrow_abort_finalized = 0;
static size_t ku_borrow_abort_target_calls = 0;
static void ku_borrow_check_abort(int64_t stage) {
  if (ku_perf_live_allocations != ku_borrow_abort_baseline_live || ku_perf_live_bytes != ku_borrow_abort_baseline_bytes) {
    fprintf(stderr, "borrowed argument temporary still live at %s entry\n", stage == 1 ? "catch" : "finally");
    exit(2);
  }
  if (stage == 1) ku_borrow_abort_caught++;
  else if (stage == 2) ku_borrow_abort_finalized++;
  else { fputs("invalid abort checkpoint\n", stderr); exit(2); }
}
#undef malloc
#undef calloc
#undef realloc
static int ku_borrow_json_should_fail(void) {
  return ++ku_borrow_json_allocation_calls == ku_borrow_fail_json_allocation;
}
static void* ku_borrow_json_malloc(size_t size) {
  if (ku_borrow_json_should_fail()) return NULL;
  return ku_perf_malloc(size);
}
static void* ku_borrow_json_calloc(size_t count, size_t size) {
  if (ku_borrow_json_should_fail()) return NULL;
  return ku_perf_calloc(count, size);
}
static void* ku_borrow_json_realloc(void* data, size_t size) {
  if (ku_borrow_json_should_fail()) return NULL;
  return ku_perf_realloc(data, size);
}
#define malloc ku_borrow_json_malloc
#define calloc ku_borrow_json_calloc
#define realloc ku_borrow_json_realloc
"#;

const TIMEOUT_HOOK: &str = r#"
static unsigned long long fingerprint(const uint8_t* data, size_t len);
static size_t ku_borrow_timeout_mode = 0, ku_borrow_timeout_fired = 0;
static size_t ku_borrow_timeout_copy_calls = 0, ku_borrow_timeout_argument_calls = 0;
static size_t ku_borrow_timeout_target_calls = 0;
static const uint8_t* ku_borrow_timeout_source = NULL;
static size_t ku_borrow_timeout_source_len = 0;
static unsigned long long ku_borrow_timeout_source_hash = 0;
static void ku_borrow_timeout_mark(size_t stage) {
  if (ku_borrow_timeout_mode == stage) {
    if (__ku_handler_timed_out || __ku_handler_unwind_depth || __ku_handler_cleanup_deadline) {
      fputs("timeout injection reached an already unwinding frame\n", stderr); exit(2);
    }
    ku_borrow_timeout_fired++;
    __ku_handler_timed_out = 1;
  }
}
static void ku_borrow_timeout_validate_text(KuString text) {
  if (text.storage != KU_STRING_OWNED || text.ptr == ku_borrow_timeout_source ||
      text.len != ku_borrow_timeout_source_len ||
      fingerprint(text.ptr, text.len) != ku_borrow_timeout_source_hash) {
    fputs("timeout target lost its independently owned borrowed argument\n", stderr); exit(2);
  }
}
static void ku_borrow_check_timeout_finally(int64_t stage) {
  ku_borrow_check_abort(stage);
  if (stage != 2 ||
      fingerprint(ku_borrow_timeout_source, ku_borrow_timeout_source_len) != ku_borrow_timeout_source_hash) {
    fputs("timeout finally changed the caller's source\n", stderr); exit(2);
  }
  if (ku_borrow_timeout_mode) {
    if (!__ku_handler_timed_out || !__ku_handler_unwind_depth ||
        __ku_handler_cleanup_deadline <= __ku_handler_now_ms()) {
      fputs("timeout finally did not enter bounded structured cleanup\n", stderr); exit(2);
    }
  } else if (__ku_handler_timed_out || __ku_handler_unwind_depth || __ku_handler_cleanup_deadline) {
    fputs("successful borrowed call unexpectedly entered timeout cleanup\n", stderr); exit(2);
  }
}
"#;

const WRAPPER: &str = r#"
#ifndef __has_feature
#define __has_feature(x) 0
#endif
#if defined(KU_BORROW_REQUIRE_ASAN) && !defined(__SANITIZE_ADDRESS__) && !__has_feature(address_sanitizer)
#error Borrow sanitizer gate requires AddressSanitizer instrumentation
#endif
#undef malloc
#undef calloc
#undef realloc
#undef free

#define CHECK(value) do { if (!(value)) { fprintf(stderr, "check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)

static unsigned long long fingerprint(const uint8_t* data, size_t len) {
  unsigned long long value = 1469598103934665603ULL;
  for (size_t index = 0; index < len; index++) {
    value ^= data[index]; value *= 1099511628211ULL;
  }
  return value;
}

static int measure(size_t bytes) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  KuArray_struct_User users = { 1, 0, 1 };
  users.data = (KuStruct_User*)ku_perf_calloc(1, sizeof(KuStruct_User));
  CHECK(users.data != 0);
  users.data[0].name = (KuString){ (uint8_t*)ku_perf_malloc(bytes), bytes, bytes, KU_STRING_OWNED };
  users.data[0].tags = (KuArray_str){ 1, (KuString*)ku_perf_calloc(1, sizeof(KuString)), 1 };
  CHECK(users.data[0].name.ptr && users.data[0].tags.data);
  users.data[0].tags.data[0] = (KuString){ (uint8_t*)ku_perf_malloc(3), 3, 3, KU_STRING_OWNED };
  CHECK(users.data[0].tags.data[0].ptr);
  for (size_t index = 0; index < bytes; index++) users.data[0].name.ptr[index] = (uint8_t)('a' + index % 26);
  memcpy(users.data[0].tags.data[0].ptr, "Ku!", 3);
  const unsigned long long expected_hash = fingerprint(users.data[0].name.ptr, bytes);
  const size_t baseline_live = ku_perf_live_allocations, baseline_bytes = ku_perf_live_bytes;
  ku_perf_calls = 0; ku_perf_total_bytes = 0; ku_perf_peak_bytes = baseline_bytes;
  CHECK(Measure(&users) == (int64_t)(256 * (bytes + 3)));
  CHECK(!ku_perf_calls && !ku_perf_total_bytes && ku_perf_peak_bytes == baseline_bytes);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  CHECK(users.len == 1 && users.data[0].name.len == bytes && users.data[0].tags.len == 1);
  CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
  CHECK(!memcmp(users.data[0].tags.data[0].ptr, "Ku!", 3));
  printf("borrow bytes=%zu reads=512 allocations=0\n", bytes);

  /* The clone control both checks independent ownership and demonstrates that
     the allocation hook would detect a hidden clone in the borrowing loop. */
  KuString copy = Copy(&users.data[0].name);
  CHECK(ku_perf_calls > 0 && copy.storage == KU_STRING_OWNED && copy.len == bytes);
  CHECK(copy.ptr != users.data[0].name.ptr && fingerprint(copy.ptr, bytes) == expected_hash);
  copy.ptr[0] = '!';
  CHECK(users.data[0].name.ptr[0] == 'a');
  ku_string_drop(&copy);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);

  ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
  CHECK(Burst(&users.data[0].name, 1) == (int64_t)bytes);
  CHECK(ku_perf_calls > 0 && ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  const size_t one_call_allocations = ku_perf_calls;
  const size_t one_call_peak = ku_perf_peak_bytes;
  CHECK(one_call_peak > baseline_bytes);
  ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
  CHECK(Burst(&users.data[0].name, 256) == (int64_t)(bytes * 256));
  printf("borrow temporary bytes=%zu rounds=256 allocations=%zu extra_peak=%zu one_call_peak=%zu\n",
    bytes, ku_perf_calls, ku_perf_peak_bytes - baseline_bytes, one_call_peak - baseline_bytes);
  CHECK(ku_perf_calls == one_call_allocations * 256);
  /* Delaying the temporary drop until the next loop assignment would overlap
     two clones and exceed the measured one-call peak. */
  CHECK(ku_perf_peak_bytes == one_call_peak);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
  CHECK(ProjectedBurst(&users.data[0].name, 1) == (int64_t)(bytes * 2));
  const size_t projected_allocations = ku_perf_calls, projected_peak = ku_perf_peak_bytes;
  CHECK(projected_allocations > 0);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
  CHECK(ProjectedBurst(&users.data[0].name, 256) == (int64_t)(bytes * 512));
  CHECK(ku_perf_calls == projected_allocations * 256 && ku_perf_peak_bytes == projected_peak);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  printf("borrow projected temporary bytes=%zu rounds=256 allocations=%zu extra_peak=%zu\n", bytes, ku_perf_calls, projected_peak - baseline_bytes);
  ku_borrow_abort_baseline_live = baseline_live;
  ku_borrow_abort_baseline_bytes = baseline_bytes;
  ku_borrow_abort_target_calls = 0;
  for (size_t nested = 0; nested < 2; nested++) {
    ku_borrow_abort_caught = 0; ku_borrow_abort_finalized = 0;
    ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
    CHECK((nested ? NestedAbortArgumentBurst(&users.data[0].name, 1) : AbortArgumentBurst(&users.data[0].name, 1)) == 2);
    CHECK(ku_borrow_abort_caught == 1 && ku_borrow_abort_finalized == 1 && !ku_borrow_abort_target_calls);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
    const size_t aborted_allocations = ku_perf_calls, aborted_peak = ku_perf_peak_bytes;
    CHECK(aborted_allocations > 0 && aborted_peak > baseline_bytes);
    ku_borrow_abort_caught = 0; ku_borrow_abort_finalized = 0;
    ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
    CHECK((nested ? NestedAbortArgumentBurst(&users.data[0].name, 256) : AbortArgumentBurst(&users.data[0].name, 256)) == 512);
    CHECK(ku_borrow_abort_caught == 256 && ku_borrow_abort_finalized == 256 && !ku_borrow_abort_target_calls);
    CHECK(ku_perf_calls == aborted_allocations * 256 && ku_perf_peak_bytes == aborted_peak);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
    CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
    printf("borrow aborted temporary bytes=%zu rounds=256 nested=%zu allocations=%zu extra_peak=%zu\n", bytes, nested, ku_perf_calls, aborted_peak - baseline_bytes);
  }
  CHECK(RetainedArgument(&users.data[0].name) == (int64_t)(bytes * 2 + 1));
  CHECK(ku_borrow_abort_target_calls == 1);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  ku_borrow_timeout_source = users.data[0].name.ptr;
  ku_borrow_timeout_source_len = bytes;
  ku_borrow_timeout_source_hash = expected_hash;
  for (size_t mode = 0; mode <= 3; mode++) {
    ku_borrow_timeout_mode = mode; ku_borrow_timeout_fired = 0;
    ku_borrow_timeout_copy_calls = 0; ku_borrow_timeout_argument_calls = 0; ku_borrow_timeout_target_calls = 0;
    ku_borrow_abort_caught = 0; ku_borrow_abort_finalized = 0;
    ku_perf_calls = 0; ku_perf_peak_bytes = baseline_bytes;
    CHECK(!__ku_call_depth && !__ku_handler_timeout_finish());
    __ku_handler_timeout_begin(5);
    CHECK(!__ku_handler_timeout_poll());
    CHECK(TimeoutBorrow(&users.data[0].name) == (mode ? 0 : 7));
    CHECK(ku_borrow_abort_finalized == 1 && !ku_borrow_abort_caught);
    CHECK(ku_borrow_timeout_copy_calls == 1);
    CHECK(ku_borrow_timeout_argument_calls == (mode == 1 ? 0 : 1));
    CHECK(ku_borrow_timeout_target_calls == (mode == 1 || mode == 2 ? 0 : 1));
    CHECK(ku_borrow_timeout_fired == (mode ? 1 : 0));
    CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
    CHECK(__ku_handler_timeout_finish() == (mode ? 1 : 0));
    CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline && !__ku_handler_timed_out);
    CHECK(ku_perf_calls > 0 && ku_perf_peak_bytes == baseline_bytes + bytes);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
    CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
    printf("borrow timeout bytes=%zu mode=%zu target_calls=%zu extra_peak=%zu\n", bytes, mode, ku_borrow_timeout_target_calls, ku_perf_peak_bytes - baseline_bytes);
  }
  ku_borrow_timeout_mode = 0;
  /* Measure this serializer's actual allocation sites before injecting one
     failure at each ordinal. Serializer output allocations are required;
     they must neither clone the borrowed input nor survive the returned Result. */
  ku_borrow_json_allocation_calls = 0;
  KuResult_str successful = Encode(&users);
  const size_t json_allocations = ku_borrow_json_allocation_calls;
  CHECK(successful.ok && successful.value.len > bytes);
  CHECK(json_allocations > 0 && json_allocations <= 256);
  ku_result_drop_str(&successful);
  CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
  CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
  for (size_t failure = 1; failure <= json_allocations; failure++) {
    ku_borrow_json_allocation_calls = 0; ku_borrow_fail_json_allocation = failure;
    KuResult_str encoded = Encode(&users);
    ku_borrow_fail_json_allocation = 0;
    CHECK(ku_borrow_json_allocation_calls >= failure);
    CHECK(!encoded.ok && encoded.error.code.len == 13);
    CHECK(!memcmp(encoded.error.code.ptr, "out_of_memory", 13));
    ku_result_drop_str(&encoded);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
    CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
  }
  printf("borrow JSON bytes=%zu allocation_failure_sites=%zu\n", bytes, json_allocations);
  for (size_t round = 0; round < 2; round++) {
    KuResult_str encoded = Encode(&users);
    CHECK(encoded.ok && encoded.value.len > bytes);
    ku_result_drop_str(&encoded);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
    CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
  }
  CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
  ku_array_drop_struct_User(&users);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  return 0;
}

int main(void) {
  CHECK(measure(16) == 0);
  CHECK(measure(65536) == 0);
  puts("borrow allocation closed loop");
  return 0;
}
"#;

#[test]
fn borrow_native_json_take_moves_subtrees_and_small_object_scratch_is_bounded() {
    let directory = TempDir::new("json-take-scratch");
    let generated = emit_c(
        directory.path(),
        r#"
fn Transfer(&text: str): str! { root=json.decode(text)? value=root.take("data")? return json.encode(value) }
fn Key(): str! { return ok("data") }
fn BadKey(): str! { fail "key failed" }
fn TakeEffectfulKey(&text: str): str! { root=json.decode(text)? value=root.take(Key()?)? return json.encode(value) }
fn TakeFailedKey(&text: str): str! { root=json.decode(text)? value=root.take(BadKey()?)? return json.encode(value) }
fn main() {}
"#,
    );
    let effectful = generated
        .split("KuResult_str TakeEffectfulKey(const KuString* text) {")
        .nth(1)
        .expect("generated consuming method caller");
    assert!(
        !effectful
            .split("\n}\n")
            .next()
            .unwrap()
            .contains("ku_value_clone("),
        "consuming take must not clone before its key"
    );
    let mut harness = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOK}\n{JSON_FAILURE_HOOK}\nstatic size_t ku_fixture_clone_copy_bytes;\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    let clone_start = harness
        .find("static bool ku_object_string_try_clone(KuString value, KuString* out) {")
        .unwrap();
    let counted_clone = harness[clone_start..].replacen("if (value.len) memcpy(data, value.ptr, value.len);", "if (value.len) { memcpy(data, value.ptr, value.len); ku_fixture_clone_copy_bytes += value.len; }", 1);
    harness.truncate(clone_start);
    harness.push_str(&counted_clone);
    harness.push_str(r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
#define CHECK(value) do { if (!(value)) { fprintf(stderr, "JSON take/scratch failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
static KuString fixture_text(const char* text) { return ku_string_static((const uint8_t*)text, strlen(text)); }
int main(void) {
  const char* inputs[] = {
    "{\"data\":{\"items\":[1,true,null],\"nested\":{\"a\":-9223372036854775808}},\"take\":false,\"drop\":\"owner\"}",
    "{\"data\":[0,1,-1,9223372036854775807,-9223372036854775808],\"drop\":\"owner\"}",
    "{\"data\":\"中文😀\\u0000\\n\\\"\\\\tail\",\"drop\":\"owner\"}"
  };
  for (size_t i=0;i<sizeof(inputs)/sizeof(inputs[0]);i++) {
    KuString text=fixture_text(inputs[i]);
    KuResult_kuvalue parsed=ku_json_parse(text); CHECK(parsed.ok);
    KuValue* original=ku_object_get(parsed.value.as.o,fixture_text("data")); CHECK(original);
    KuValue saved=*original;
    ku_perf_calls=0; ku_perf_total_bytes=0; ku_fixture_clone_copy_bytes=0;
    KuResult_kuvalue copied=ku_value_get_result(parsed.value,fixture_text("data")); CHECK(copied.ok);
    size_t copy_calls=ku_perf_calls,allocated=ku_perf_total_bytes,copied_bytes=ku_fixture_clone_copy_bytes; CHECK(copy_calls>0 && allocated>0); if(i!=1)CHECK(copied_bytes>0);
    ku_perf_calls=0; ku_perf_total_bytes=0; ku_fixture_clone_copy_bytes=0; ku_borrow_json_allocation_calls=0; ku_borrow_fail_json_allocation=1;
    KuResult_kuvalue selected=ku_value_take_result(ku_value_move(&parsed.value),fixture_text("data"));
    ku_borrow_fail_json_allocation=0;
    CHECK(selected.ok && !ku_perf_calls && !ku_perf_total_bytes && !ku_borrow_json_allocation_calls && !ku_fixture_clone_copy_bytes);
    CHECK(selected.value.tag==saved.tag);
    if(saved.tag==KU_OBJECT)CHECK(selected.value.as.o==saved.as.o);
    else if(saved.tag==KU_ARRAY)CHECK(selected.value.as.arr==saved.as.arr);
    else CHECK(selected.value.as.s.ptr==saved.as.s.ptr);
    CHECK(ku_value_equal(copied.value,selected.value));
    KuResult_str encoded=ku_json_stringify(selected.value); CHECK(encoded.ok);
    KuResult_str result=Transfer(&text); CHECK(result.ok && ku_string_equal(result.value,encoded.value));
    KuResult_str effects=TakeEffectfulKey(&text); CHECK(effects.ok && ku_string_equal(effects.value,encoded.value));
    ku_result_drop_str(&effects);ku_result_drop_str(&result);ku_result_drop_str(&encoded);
    ku_result_drop_kuvalue(&copied);ku_result_drop_kuvalue(&selected);ku_result_drop_kuvalue(&parsed);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
    KuResult_str failed=TakeFailedKey(&text);CHECK(!failed.ok);ku_result_drop_str(&failed);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
    printf("JSON take corpus=%zu legacy_clone_calls=%zu legacy_clone_allocated_bytes=%zu legacy_string_copy_bytes=%zu take_clone_calls=0 take_clone_bytes=0\n",i,copy_calls,allocated,copied_bytes);
  }
  for(size_t mode=0;mode<3;mode++) {
    KuResult_kuvalue value=ku_json_parse(fixture_text(mode==2?"[\"owned\"]":"{\"other\":\"owned\"}"));CHECK(value.ok);
    if(mode==1) {ku_borrow_json_allocation_calls=0;ku_borrow_fail_json_allocation=1;}
    KuResult_kuvalue missing=ku_value_take_result(ku_value_move(&value.value),fixture_text("missing"));
    ku_borrow_fail_json_allocation=0;CHECK(!missing.ok);
    const char* code=mode==0?"missing_key":mode==1?"out_of_memory":"type_unsupported";
    CHECK(ku_string_equal(missing.error.code,fixture_text(code)));ku_result_drop_kuvalue(&missing);ku_result_drop_kuvalue(&value);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
  }
  const char* objects[]={"{\"b\":1,\"a\":2}","{\"h\":1,\"g\":2,\"f\":3,\"e\":4,\"d\":5,\"c\":6,\"b\":7,\"a\":8}","{\"i\":0,\"h\":1,\"g\":2,\"f\":3,\"e\":4,\"d\":5,\"c\":6,\"b\":7,\"a\":8}","{\"b\":{\"d\":1,\"c\":2},\"a\":{\"f\":3,\"e\":4}}"};
  for(size_t i=0;i<4;i++) {
    KuResult_kuvalue object=ku_json_parse(fixture_text(objects[i]));CHECK(object.ok);
    size_t live=ku_perf_live_allocations,bytes=ku_perf_live_bytes;
    ku_borrow_json_allocation_calls=0;
    KuResult_str encoded=ku_json_stringify(object.value);CHECK(encoded.ok);
    size_t calls=ku_borrow_json_allocation_calls;CHECK(calls==(i==2?2:1));
    KuResult_kuvalue decoded=ku_json_parse(encoded.value);CHECK(decoded.ok&&ku_value_equal(decoded.value,object.value));
    ku_result_drop_kuvalue(&decoded);ku_result_drop_str(&encoded);
    CHECK(ku_perf_live_allocations==live&&ku_perf_live_bytes==bytes);
    for(size_t fail=1;fail<=calls;fail++) {
      ku_borrow_json_allocation_calls=0;ku_borrow_fail_json_allocation=fail;
      KuResult_str rejected=ku_json_stringify(object.value);ku_borrow_fail_json_allocation=0;
      CHECK(!rejected.ok && ku_string_equal(rejected.error.code,fixture_text("out_of_memory")));
      ku_result_drop_str(&rejected);CHECK(ku_perf_live_allocations==live&&ku_perf_live_bytes==bytes);
    }
    printf("JSON scratch object=%zu encoder_allocations=%zu\n",i,calls);
    ku_result_drop_kuvalue(&object);CHECK(!ku_perf_live_allocations&&!ku_perf_live_bytes);
  }
  CHECK(!ku_perf_overflow);puts("JSON consuming take and bounded scratch verified");return 0;
}
"#);
    let source = directory.path().join("json-take-scratch.c");
    fs::write(&source, harness).unwrap();
    let Some(executable) = compile_harness(directory.path(), &source, "json-take-scratch") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("JSON consuming take and bounded scratch verified"),
        "{stdout}"
    );
    eprint!("{stdout}");
}
