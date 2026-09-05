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

const JSON_FAILURE_HOOK: &str = r#"
static size_t ku_borrow_realloc_calls = 0, ku_borrow_fail_realloc = 0;
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
#undef realloc
static void* ku_borrow_realloc(void* data, size_t size) {
  if (++ku_borrow_realloc_calls == ku_borrow_fail_realloc) return NULL;
  return ku_perf_realloc(data, size);
}
#define realloc ku_borrow_realloc
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
  for (size_t failure = 1; failure <= (bytes > 16 ? 3 : 1); failure++) {
    ku_borrow_realloc_calls = 0; ku_borrow_fail_realloc = failure;
    KuResult_str encoded = Encode(&users);
    CHECK(!encoded.ok && encoded.error.code.len == 13);
    CHECK(!memcmp(encoded.error.code.ptr, "out_of_memory", 13));
    ku_result_drop_str(&encoded);
    CHECK(ku_perf_live_allocations == baseline_live && ku_perf_live_bytes == baseline_bytes);
    CHECK(fingerprint(users.data[0].name.ptr, bytes) == expected_hash);
  }
  ku_borrow_fail_realloc = 0;
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
