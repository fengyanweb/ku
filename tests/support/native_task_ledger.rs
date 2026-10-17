//! Thread-safe observations over the existing native allocation hook.
pub const LEDGER_LOCK: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"source scope check line %d: %s\n",__LINE__,#c); abort(); } } while (0)
#if defined(_WIN32)
static SRWLOCK fixture_allocation_lock=SRWLOCK_INIT;
static void fixture_alloc_lock(void) { AcquireSRWLockExclusive(&fixture_allocation_lock); }
static void fixture_alloc_unlock(void) { ReleaseSRWLockExclusive(&fixture_allocation_lock); }
#else
static pthread_mutex_t fixture_allocation_lock=PTHREAD_MUTEX_INITIALIZER;
static void fixture_alloc_lock(void) { CHECK(!pthread_mutex_lock(&fixture_allocation_lock)); }
static void fixture_alloc_unlock(void) { CHECK(!pthread_mutex_unlock(&fixture_allocation_lock)); }
#endif
"#;

pub const LOCKED_ALLOCATIONS: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
static void* fixture_malloc(size_t size) { fixture_alloc_lock(); void* p=ku_perf_malloc(size); fixture_alloc_unlock(); return p; }
static void* fixture_calloc(size_t count,size_t size) { fixture_alloc_lock(); void* p=ku_perf_calloc(count,size); fixture_alloc_unlock(); return p; }
static void* fixture_realloc(void* old,size_t size) { fixture_alloc_lock(); void* p=ku_perf_realloc(old,size); fixture_alloc_unlock(); return p; }
static void fixture_free(void* p) { fixture_alloc_lock(); ku_perf_free(p); fixture_alloc_unlock(); }
typedef struct FixtureLedger { size_t allocations,bytes,calls; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock(); FixtureLedger out={ku_perf_live_allocations,ku_perf_live_bytes,ku_perf_calls,ku_perf_overflow};
  fixture_alloc_unlock(); return out;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
"#;
