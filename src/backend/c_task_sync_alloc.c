/* Only artifacts containing a verified SyncCall emit these hooks. Ordinary
 * helper calls borrow one same-thread reservation context; no pointer or owner
 * is retained by it. Keep each complete allocation request prepaid until the
 * call returns, including old/new realloc storage and freed local temporaries.
 * The frame's existing typed scanner then refunds the conservative excess.
 * External engines retain their existing internal-allocation contracts; every
 * Ku-owned C output copy is covered here. This is not an allocator/RSS limit. */
typedef struct KuTaskSyncAllocation {
  uint32_t (*reserve)(const void*, size_t);
  const void* context;
  uint32_t status;
  bool reserving;
} KuTaskSyncAllocation;
static KU_THREAD_LOCAL KuTaskSyncAllocation __ku_task_sync_allocation;

static bool __ku_task_sync_preallocate(size_t bytes) {
  KuTaskSyncAllocation* allocation = &__ku_task_sync_allocation;
  if (!allocation->reserve) return true;
  if (allocation->status) return false;
  if (allocation->reserving) {
    allocation->status = 10u; /* KU_TASK_DRIVER_INVALID_STATE */
    return false;
  }
  allocation->reserving = true;
  uint32_t status = allocation->reserve(allocation->context, bytes);
  allocation->reserving = false;
  if (status) allocation->status = status;
  return allocation->status == 0u;
}
static void* __ku_task_sync_malloc(size_t bytes) {
  if (!__ku_task_sync_preallocate(bytes)) return NULL;
  void* result = malloc(bytes);
  if (!result && bytes && __ku_task_sync_allocation.reserve)
    __ku_task_sync_allocation.status = 98u; /* allocation failed */
  return result;
}
static void* __ku_task_sync_calloc(size_t count, size_t bytes) {
  if (count && bytes > SIZE_MAX / count) {
    if (__ku_task_sync_allocation.reserve) __ku_task_sync_allocation.status = 9u;
    return NULL;
  }
  if (!__ku_task_sync_preallocate(count * bytes)) return NULL;
  void* result = calloc(count, bytes);
  if (!result && count && bytes && __ku_task_sync_allocation.reserve)
    __ku_task_sync_allocation.status = 98u;
  return result;
}
static void* __ku_task_sync_realloc(void* old, size_t bytes) {
  if (!__ku_task_sync_preallocate(bytes)) return NULL;
  /* On failure the existing caller still owns old, exactly as with realloc. */
  void* result = realloc(old, bytes);
  if (!result && bytes && __ku_task_sync_allocation.reserve)
    __ku_task_sync_allocation.status = 98u;
  return result;
}
static KuString __ku_task_sync_clone_string(KuString value) {
  if (!__ku_task_sync_allocation.reserve) return ku_string_clone(value);
  if (value.storage == KU_STRING_STATIC || !value.ptr) return value;
  size_t capacity = value.len ? value.len : 1u;
  uint8_t* data = (uint8_t*)__ku_task_sync_malloc(capacity);
  if (!data) return (KuString){0};
  if (value.len) memcpy(data, value.ptr, value.len);
  return (KuString){data, value.len, capacity, KU_STRING_OWNED};
}
static KuString __ku_task_sync_concat_string(KuString left, KuString right) {
  if (!__ku_task_sync_allocation.reserve) return ku_string_concat(left, right);
  if (right.len > SIZE_MAX - left.len) {
    __ku_task_sync_allocation.status = 9u;
    return (KuString){0};
  }
  size_t length = left.len + right.len, capacity = length ? length : 1u;
  uint8_t* data = (uint8_t*)__ku_task_sync_malloc(capacity);
  if (!data) return (KuString){0};
  if (left.len) memcpy(data, left.ptr, left.len);
  if (right.len) memcpy(data + left.len, right.ptr, right.len);
  return (KuString){data, length, length, KU_STRING_OWNED};
}
static KuString __ku_task_sync_string_from_int(int64_t value) {
  if (!__ku_task_sync_allocation.reserve) return ku_string_from_int(value);
  char buffer[24];
  int length = snprintf(buffer, sizeof(buffer), "%lld", (long long)value);
  if (length <= 0) return (KuString){0};
  uint8_t* data = (uint8_t*)__ku_task_sync_malloc((size_t)length);
  if (!data) return (KuString){0};
  memcpy(data, buffer, (size_t)length);
  return (KuString){data, (size_t)length, (size_t)length, KU_STRING_OWNED};
}
static KuError __ku_task_sync_clone_error(KuError value) {
  return (KuError){__ku_task_sync_clone_string(value.domain),
      __ku_task_sync_clone_string(value.code),
      __ku_task_sync_clone_string(value.message)};
}
/* The system headers and the original basic string helpers precede this
 * boundary. Undefine all seven macros before emitting any Task host/driver so
 * its grow_charge callback cannot recursively enter these wrappers. */
#define malloc(bytes) __ku_task_sync_malloc(bytes)
#define calloc(count, bytes) __ku_task_sync_calloc((count), (bytes))
#define realloc(old, bytes) __ku_task_sync_realloc((old), (bytes))
#define ku_string_clone(...) __ku_task_sync_clone_string(__VA_ARGS__)
#define ku_string_concat(...) __ku_task_sync_concat_string(__VA_ARGS__)
#define ku_string_from_int(...) __ku_task_sync_string_from_int(__VA_ARGS__)
#define ku_error_clone(...) __ku_task_sync_clone_error(__VA_ARGS__)
