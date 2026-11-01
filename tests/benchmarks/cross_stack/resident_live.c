#define _GNU_SOURCE
#include <errno.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Diagnostic glibc boundary only. Never linked into a shipped Ku binary.
 * No dlsym, logger, observer thread, signal handler or dynamic storage. */
extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);
extern void __libc_free(void *);
extern void *__libc_memalign(size_t, size_t);
#define CAPACITY 16384u
/* Exported fixed-layout words, read only while every process thread is stopped.
 * status: version, capacity, inflight, lock, lost, recursive, allocations,
 * frees, untracked_frees, live_count, peak_live. Records: ptr,size,pc,tid,kind.
 * Neither size nor allocator footprint is residency. */
uint64_t ku_resident_status[11] = {1, CAPACITY};
uint64_t ku_resident_records[CAPACITY][5];
static _Thread_local unsigned entered __attribute__((tls_model("initial-exec")));

static int lock_table(void) {
  for (size_t attempt = 0; attempt < 1000000u; attempt++)
    if (!__atomic_exchange_n(&ku_resident_status[3], 1, __ATOMIC_ACQUIRE)) return 1;
  __atomic_fetch_add(&ku_resident_status[4], 1, __ATOMIC_RELAXED);
  return 0;
}
static void unlock_table(void) {
  __atomic_store_n(&ku_resident_status[3], 0, __ATOMIC_RELEASE);
}
static size_t cell_for(uintptr_t pointer) {
  return (size_t)(((pointer >> 4) * UINT64_C(11400714819323198485)) >> 50);
}
static void insert(void *pointer, size_t bytes, uintptr_t pc, unsigned kind) {
  if (!pointer) return;
  size_t cell = cell_for((uintptr_t)pointer), empty = CAPACITY;
  for (size_t probes = 0; probes < CAPACITY; probes++) {
    uint64_t *row = ku_resident_records[cell];
    if (!row[0]) {
      if (empty == CAPACITY) empty = cell;
      break;
    }
    if (row[0] == (uintptr_t)pointer) {
      /* Duplicate live pointer invalidates the diagnostic coverage. */
      ku_resident_status[4]++;
      return;
    }
    cell = (cell + 1) & (CAPACITY - 1);
  }
  if (empty == CAPACITY) { ku_resident_status[4]++; return; }
  uint64_t *row = ku_resident_records[empty];
  row[0] = (uintptr_t)pointer; row[1] = bytes; row[2] = pc;
  row[3] = (uint64_t)syscall(SYS_gettid); row[4] = kind;
  ku_resident_status[6]++;
  if (++ku_resident_status[9] > ku_resident_status[10])
    ku_resident_status[10] = ku_resident_status[9];
}
static void remove_pointer(void *pointer) {
  if (!pointer) return;
  size_t cell = cell_for((uintptr_t)pointer);
  for (size_t probes = 0; probes < CAPACITY; probes++) {
    uint64_t *row = ku_resident_records[cell];
    if (!row[0]) break;
    if (row[0] == (uintptr_t)pointer) {
      size_t hole = cell, scan = (cell + 1) & (CAPACITY - 1);
      row[0] = 0;
      for (size_t moved = 0; moved < CAPACITY - 1; moved++) {
        uint64_t *next = ku_resident_records[scan];
        if (!next[0]) break;
        size_t home = cell_for(next[0]);
        if (((hole - home) & (CAPACITY - 1)) < ((scan - home) & (CAPACITY - 1))) {
          for (size_t word = 0; word < 5; word++) ku_resident_records[hole][word] = next[word];
          hole = scan;
          next[0] = 0;
        }
        scan = (scan + 1) & (CAPACITY - 1);
      }
      for (size_t word = 0; word < 5; word++) ku_resident_records[hole][word] = 0;
      ku_resident_status[7]++; ku_resident_status[9]--; return;
    }
    cell = (cell + 1) & (CAPACITY - 1);
  }
  ku_resident_status[8]++;
}
static int begin(void) {
  if (entered++) {
    __atomic_fetch_add(&ku_resident_status[5], 1, __ATOMIC_RELAXED);
    return 0;
  }
  __atomic_fetch_add(&ku_resident_status[2], 1, __ATOMIC_ACQ_REL);
  return 1;
}
static void end(void) {
  __atomic_fetch_sub(&ku_resident_status[2], 1, __ATOMIC_RELEASE);
  entered--;
}
#define CALLER ((uintptr_t)__builtin_extract_return_addr(__builtin_return_address(0)))
void *malloc(size_t bytes) {
  if (!begin()) { void *p = __libc_malloc(bytes); entered--; return p; }
  void *p = __libc_malloc(bytes); int saved = errno;
  if (lock_table()) { insert(p, bytes, CALLER, 1); unlock_table(); } end(); errno = saved; return p;
}
void *calloc(size_t count, size_t width) {
  if (!begin()) { void *p = __libc_calloc(count, width); entered--; return p; }
  void *p = __libc_calloc(count, width); int saved = errno;
  if (lock_table()) { insert(p, p ? count * width : 0, CALLER, 2); unlock_table(); } end(); errno = saved; return p;
}
void *realloc(void *pointer, size_t bytes) {
  if (!begin()) { void *p = __libc_realloc(pointer, bytes); entered--; return p; }
  void *p = __libc_realloc(pointer, bytes); int saved = errno;
  if (lock_table()) {
    if (p || !bytes) { remove_pointer(pointer); insert(p, bytes, CALLER, 3); }
    unlock_table();
  } end(); errno = saved; return p;
}
void free(void *pointer) {
  if (!begin()) { __libc_free(pointer); entered--; return; }
  int saved = errno;
  if (lock_table()) { remove_pointer(pointer); unlock_table(); } __libc_free(pointer);
  end(); errno = saved;
}
void *memalign(size_t alignment, size_t bytes) {
  if (!begin()) { void *p = __libc_memalign(alignment, bytes); entered--; return p; }
  void *p = __libc_memalign(alignment, bytes); int saved = errno;
  if (lock_table()) { insert(p, bytes, CALLER, 4); unlock_table(); } end(); errno = saved; return p;
}
void *aligned_alloc(size_t alignment, size_t bytes) {
  if (!begin()) { void *p = __libc_memalign(alignment, bytes); entered--; return p; }
  void *p = __libc_memalign(alignment, bytes); int saved = errno;
  if (lock_table()) { insert(p, bytes, CALLER, 5); unlock_table(); } end(); errno = saved; return p;
}
int posix_memalign(void **output, size_t alignment, size_t bytes) {
  if (!alignment || alignment % sizeof(void *) || (alignment & (alignment - 1))) return EINVAL;
  int saved = errno;
  if (!begin()) {
    void *p = __libc_memalign(alignment, bytes); entered--;
    errno = saved; if (!p) return ENOMEM; *output = p; return 0;
  }
  void *p = __libc_memalign(alignment, bytes);
  if (lock_table()) { insert(p, bytes, CALLER, 6); unlock_table(); } end(); errno = saved;
  if (!p) return ENOMEM;
  *output = p; return 0;
}
