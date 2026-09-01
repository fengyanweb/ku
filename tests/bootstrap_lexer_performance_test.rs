//! Allocation and peak-live-byte scaling gate for the native stage-1 Ku lexer.
//!
//! Wall-clock data is diagnostic only: the enforced signal is memory/allocation
//! growth, which is considerably less sensitive to CI host scheduling.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use native_pg_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

#[test]
fn bootstrap_lexer_native_allocation_and_peak_memory_scale_linearly() {
    let directory = TempDir::new("bootstrap-lexer-performance");
    let stage1 = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bootstrap")
        .join("stage1");
    for name in ["token.ku", "lexer.ku"] {
        fs::copy(stage1.join(name), directory.path().join(name))
            .unwrap_or_else(|error| panic!("copy bootstrap stage-1 {name}: {error}"));
    }
    let generated = emit_c(
        directory.path(),
        r#"import { Scan } from "./lexer.ku"
fn Count(source: str): int! {
    tokens = Scan(source)?
    return ok(tokens.len())
}
fn main(): null! {
    return ok(null)
}
"#,
    );
    assert!(generated.contains("typedef struct KuString {"));
    assert!(generated.contains("KuResult_int Count(KuString source)"));
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
    harness.push_str(PERFORMANCE_WRAPPER);
    let source = directory.path().join("bootstrap-lexer-performance.c");
    fs::write(&source, harness).expect("write bootstrap lexer performance harness");
    let Some(executable) =
        compile_harness(directory.path(), &source, "bootstrap-lexer-performance")
    else {
        return;
    };
    let mut command = Command::new(executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS).unwrap_or_else(|error| {
        panic!("bootstrap lexer performance gate was not bounded: {error}")
    });
    assert!(
        output.status.success(),
        "bootstrap lexer performance gate failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    for (bytes, tokens) in [(2_304, 769), (4_608, 1_537), (9_216, 3_073)] {
        assert!(
            stdout.lines().any(|line| {
                line.starts_with(&format!("lexer bytes={bytes} tokens={tokens} rounds=32 "))
                    && line.contains("elapsed_ms=")
                    && line.contains(" allocs=")
                    && line.contains(" peak=")
            }),
            "missing lexer metric for {bytes} bytes/{tokens} tokens:\n{stdout}"
        );
    }
    assert!(stdout.ends_with("bootstrap lexer allocation closed loop\n"));
    eprint!("{stdout}");
    assert!(
        output.stderr.is_empty(),
        "unexpected lexer benchmark stderr"
    );
}

const ALLOCATION_HOOK: &str = r#"
#include <stddef.h>
typedef union KuPerfAllocation {
  struct { size_t size; } record;
  /* Align every scalar/pointer used by the Ku ABI, including compilers whose
     C library does not expose max_align_t (the MSVC test host is one). */
  long double scalar_alignment;
  int64_t integer_alignment;
  void* pointer_alignment;
} KuPerfAllocation;
static size_t ku_perf_live_allocations = 0, ku_perf_live_bytes = 0;
static size_t ku_perf_peak_bytes = 0, ku_perf_calls = 0, ku_perf_total_bytes = 0;
static int ku_perf_overflow = 0;
static void ku_perf_add(size_t* target, size_t value) {
  if (value > SIZE_MAX - *target) { ku_perf_overflow = 1; *target = SIZE_MAX; }
  else *target += value;
}
static void* ku_perf_allocate(size_t size, int clear) {
  ku_perf_calls++;
  ku_perf_add(&ku_perf_total_bytes, size);
  if (size > SIZE_MAX - sizeof(KuPerfAllocation)) return 0;
  size_t storage = sizeof(KuPerfAllocation) + size;
  KuPerfAllocation* allocation = (KuPerfAllocation*)malloc(storage ? storage : 1);
  if (!allocation) return 0;
  allocation->record.size = size;
  ku_perf_live_allocations++;
  ku_perf_add(&ku_perf_live_bytes, size);
  if (ku_perf_live_bytes > ku_perf_peak_bytes) ku_perf_peak_bytes = ku_perf_live_bytes;
  void* value = (void*)(allocation + 1);
  if (clear && size) memset(value, 0, size);
  return value;
}
static void* ku_perf_malloc(size_t size) { return ku_perf_allocate(size, 0); }
static void* ku_perf_calloc(size_t count, size_t size) {
  if (count && size > SIZE_MAX / count) return 0;
  return ku_perf_allocate(count * size, 1);
}
static void ku_perf_free(void* value) {
  if (!value) return;
  KuPerfAllocation* allocation = ((KuPerfAllocation*)value) - 1;
  if (!ku_perf_live_allocations || allocation->record.size > ku_perf_live_bytes) {
    fputs("lexer allocation accounting underflow\n", stderr); exit(2);
  }
  ku_perf_live_allocations--;
  ku_perf_live_bytes -= allocation->record.size;
  free(allocation);
}
static void* ku_perf_realloc(void* value, size_t size) {
  if (!value) return ku_perf_allocate(size, 0);
  if (!size) { ku_perf_free(value); return 0; }
  KuPerfAllocation* allocation = ((KuPerfAllocation*)value) - 1;
  size_t old_size = allocation->record.size;
  if (!ku_perf_live_allocations || old_size > ku_perf_live_bytes || size > SIZE_MAX - sizeof(KuPerfAllocation)) return 0;
  ku_perf_calls++;
  ku_perf_add(&ku_perf_total_bytes, size);
  KuPerfAllocation* replacement = (KuPerfAllocation*)realloc(allocation, sizeof(KuPerfAllocation) + size);
  if (!replacement) return 0;
  replacement->record.size = size;
  ku_perf_live_bytes -= old_size;
  ku_perf_add(&ku_perf_live_bytes, size);
  if (ku_perf_live_bytes > ku_perf_peak_bytes) ku_perf_peak_bytes = ku_perf_live_bytes;
  return (void*)(replacement + 1);
}
#define malloc ku_perf_malloc
#define calloc ku_perf_calloc
#define realloc ku_perf_realloc
#define free ku_perf_free
"#;

const PERFORMANCE_WRAPPER: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free

#define CHECK(value) do { if (!(value)) { fprintf(stderr, "check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
typedef struct {
  size_t calls_sum, max_calls, max_total, max_peak;
  unsigned long long elapsed_ms;
} KuPerfMetric;
static unsigned long long fingerprint(const uint8_t* data, size_t len) {
  unsigned long long hash = 1469598103934665603ULL;
  for (size_t i = 0; i < len; i++) { hash ^= data[i]; hash *= 1099511628211ULL; }
  return hash;
}
static int check_string_chars_storage(void) {
  uint8_t* ascii_input = (uint8_t*)ku_perf_malloc(128);
  CHECK(ascii_input != 0);
  for (size_t i = 0; i < 128; i++) ascii_input[i] = (uint8_t)i;
  ku_perf_calls = 0; ku_perf_total_bytes = 0; ku_perf_peak_bytes = 0; ku_perf_overflow = 0;
  KuString ascii_source = { ascii_input, 128, 128, KU_STRING_OWNED };
  KuArray_str ascii = ku_string_chars(ascii_source);
  CHECK(ascii.len == 128 && ascii.data && ku_perf_calls == 1
      && ku_perf_live_allocations == 2 && ku_perf_live_bytes == 128 + 128 * sizeof(KuString));
  for (size_t i = 0; i < 128; i++) {
    CHECK(ascii.data[i].ptr && ascii.data[i].len == 1 && ascii.data[i].capacity == 0
        && ascii.data[i].storage == KU_STRING_STATIC && ascii.data[i].ptr[0] == (uint8_t)i);
    ascii_input[i] = (uint8_t)(127 - i);
    CHECK(ascii.data[i].ptr[0] == (uint8_t)i);
  }
  ku_string_drop(&ascii_source);
  CHECK(!ascii_source.ptr && !ascii_source.len && ku_perf_live_allocations == 1);

  size_t before_clone_calls = ku_perf_calls;
  KuArray_str cloned = ku_array_clone_str(ascii);
  CHECK(cloned.len == ascii.len && cloned.data != ascii.data && ku_perf_calls == before_clone_calls + 1
      && ku_perf_live_allocations == 2);
  for (size_t i = 0; i < 128; i++) {
    CHECK(cloned.data[i].ptr == ascii.data[i].ptr && cloned.data[i].storage == KU_STRING_STATIC
        && cloned.data[i].len == 1 && cloned.data[i].ptr[0] == (uint8_t)i);
  }
  ku_array_drop_str(&ascii);
  CHECK(!ascii.data && !ascii.len && ku_perf_live_allocations == 1);
  for (size_t i = 0; i < 128; i++) CHECK(cloned.data[i].ptr[0] == (uint8_t)i);

  before_clone_calls = ku_perf_calls;
  KuString letter_clone = ku_string_clone(cloned.data['A']);
  CHECK(letter_clone.ptr == cloned.data['A'].ptr && letter_clone.storage == KU_STRING_STATIC
      && ku_perf_calls == before_clone_calls);
  KuString joined = ku_string_concat(letter_clone, ku_string_static((const uint8_t*)"!", 1));
  CHECK(joined.storage == KU_STRING_OWNED && joined.len == 2 && joined.ptr[0] == 'A' && joined.ptr[1] == '!');
  CHECK(letter_clone.storage == KU_STRING_STATIC && letter_clone.len == 1 && letter_clone.ptr[0] == 'A'
      && cloned.data['A'].storage == KU_STRING_STATIC && cloned.data['A'].ptr[0] == 'A');
  ku_string_drop(&joined); ku_string_drop(&letter_clone); ku_array_drop_str(&cloned);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);

  static const uint8_t expected_utf8[] = { 0xc2, 0x80, 0xf0, 0x9f, 0x98, 0x80 };
  uint8_t* utf8_input = (uint8_t*)ku_perf_malloc(sizeof(expected_utf8));
  CHECK(utf8_input != 0); memcpy(utf8_input, expected_utf8, sizeof(expected_utf8));
  ku_perf_calls = 0; ku_perf_total_bytes = 0; ku_perf_peak_bytes = 0; ku_perf_overflow = 0;
  KuString unicode_source = { utf8_input, sizeof(expected_utf8), sizeof(expected_utf8), KU_STRING_OWNED };
  KuArray_str unicode = ku_string_chars(unicode_source);
  CHECK(unicode.len == 2 && unicode.data && ku_perf_calls == 3 && ku_perf_live_allocations == 4);
  CHECK(unicode.data[0].storage == KU_STRING_OWNED && unicode.data[0].len == 2
      && !memcmp(unicode.data[0].ptr, expected_utf8, 2));
  CHECK(unicode.data[1].storage == KU_STRING_OWNED && unicode.data[1].len == 4
      && !memcmp(unicode.data[1].ptr, expected_utf8 + 2, 4));
  memset(utf8_input, 0, sizeof(expected_utf8)); ku_string_drop(&unicode_source);
  CHECK(!unicode_source.ptr && !unicode_source.len && ku_perf_live_allocations == 3);
  CHECK(!memcmp(unicode.data[0].ptr, expected_utf8, 2)
      && !memcmp(unicode.data[1].ptr, expected_utf8 + 2, 4));
  ku_array_drop_str(&unicode);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  return 0;
}
static int measure(size_t repetitions, KuPerfMetric* metric) {
  static const char line[] = "name = 1\n";
  const size_t bytes = repetitions * (sizeof(line) - 1);
  uint8_t* input = (uint8_t*)malloc(bytes ? bytes : 1);
  if (!input) return 1;
  for (size_t i = 0; i < repetitions; i++) memcpy(input + i * (sizeof(line) - 1), line, sizeof(line) - 1);
  const unsigned long long expected_hash = fingerprint(input, bytes);
  const int64_t expected_tokens = (int64_t)(repetitions * 3 + 1);
  unsigned long long started = __ku_handler_now_ms();
  memset(metric, 0, sizeof(*metric));
  for (int round = 0; round < 32; round++) {
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
    ku_perf_calls = 0; ku_perf_total_bytes = 0; ku_perf_peak_bytes = 0; ku_perf_overflow = 0;
    KuString source = { input, bytes, 0, KU_STRING_STATIC };
    KuResult_int counted = Count(source);
    CHECK(counted.ok && counted.value == expected_tokens);
    CHECK(!counted.error.domain.ptr && !counted.error.code.ptr && !counted.error.message.ptr);
    CHECK(fingerprint(input, bytes) == expected_hash);
    CHECK(!ku_perf_overflow && !ku_perf_live_allocations && !ku_perf_live_bytes);
    metric->calls_sum += ku_perf_calls;
    if (ku_perf_calls > metric->max_calls) metric->max_calls = ku_perf_calls;
    if (ku_perf_total_bytes > metric->max_total) metric->max_total = ku_perf_total_bytes;
    if (ku_perf_peak_bytes > metric->max_peak) metric->max_peak = ku_perf_peak_bytes;
  }
  unsigned long long finished = __ku_handler_now_ms();
  free(input);
  metric->elapsed_ms = finished >= started ? finished - started : 0;
  printf("lexer bytes=%zu tokens=%lld rounds=32 elapsed_ms=%llu allocs=%zu peak=%zu\n",
    bytes, (long long)expected_tokens, metric->elapsed_ms, metric->calls_sum, metric->max_peak);
  return 0;
}
static int linear_upper(size_t larger, size_t smaller, size_t allowance) {
  return smaller <= (SIZE_MAX - allowance) / 6 && larger <= smaller * 6 + allowance;
}
int main(void) {
  KuPerfMetric small = {0}, medium = {0}, large = {0};
  CHECK(check_string_chars_storage() == 0);
  CHECK(measure(256, &small) == 0);
  CHECK(measure(512, &medium) == 0);
  CHECK(measure(1024, &large) == 0);
  CHECK(small.max_calls && small.max_total && small.max_peak);
  CHECK(medium.max_calls >= small.max_calls && large.max_calls >= medium.max_calls);
  CHECK(medium.max_total >= small.max_total && large.max_total >= medium.max_total);
  CHECK(medium.max_peak >= small.max_peak && large.max_peak >= medium.max_peak);
  /* Four times the input permits at most six times the allocation work/live
     payload plus a fixed 256 KiB runtime allowance. This admits geometric
     capacity rounding while rejecting sustained quadratic token copying. */
  CHECK(linear_upper(large.max_calls, small.max_calls, 256));
  CHECK(linear_upper(large.max_total, small.max_total, 256 * 1024));
  CHECK(linear_upper(large.max_peak, small.max_peak, 256 * 1024));
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  puts("bootstrap lexer allocation closed loop");
  return 0;
}
"#;
