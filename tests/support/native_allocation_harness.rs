//! Allocation instrumentation shared by generated-native resource gates.
//!
//! Inject this immediately before the generated `KuString` declaration, after
//! the generated platform headers. The macros then cover every direct runtime
//! `malloc`/`calloc`/`realloc`/`free` call while leaving a test wrapper appended
//! after matching `#undef`s free to allocate untracked immutable input buffers.

pub const ALLOCATION_HOOK: &str = r#"
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
    fputs("native allocation accounting underflow\n", stderr); exit(2);
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
