#if defined(_WIN32) && defined(_MSC_VER)
#include <share.h>
#endif

static bool ku_fs_read_native_impl(const KuFsNativeChar*, KuString*, KuError*, KuFsAllocatorV1*);
static bool ku_fs_read_impl(KuString path, KuString* value, KuError* error) {
  if (value) *value = (KuString){0};
  if (error) *error = (KuError){0};
  KuFsNativeChar* native = ku_fs_native_path(path, error, "read_failed", "failed to read: invalid path");
  if (!native) return false;
  bool ok=ku_fs_read_native_impl(native,value,error,NULL);
  free(native); return ok;
}

static bool ku_fs_read_native_impl(const KuFsNativeChar* native, KuString* value, KuError* error, KuFsAllocatorV1* allocator) {
#if defined(_WIN32) && defined(_MSC_VER)
  FILE* file = _wfsopen(native, L"rb", _SH_DENYNO);
#elif defined(_WIN32)
  FILE* file = _wfopen(native, L"rb");
#else
  FILE* file = fopen(native, "rb");
#endif
  if (!file) { ku_fs_set_error(error, "read_failed", "failed to read file"); return false; }

  size_t capacity = 4096;
  uint8_t* data = (uint8_t*)ku_fs_allocate(allocator,capacity);
  if (!data) {
    (void)fclose(file);
    ku_fs_set_error(error, "read_failed", "failed to read: out of memory");
    return false;
  }
  size_t len = 0;
  bool failed = false;
  while (!failed) {
    if (len == KU_FS_MAX_IO_BYTES) {
      uint8_t extra = 0;
      size_t got = fread(&extra, 1, 1, file);
      if (got == 1) {
        ku_fs_set_error(error, "file_too_large", "failed to read: file is too large");
        failed = true;
      } else if (ferror(file)) {
        ku_fs_set_error(error, "read_failed", "failed to read file");
        failed = true;
      }
      break;
    }
    if (len == capacity) {
      size_t next = capacity <= KU_FS_MAX_IO_BYTES / 2 ? capacity * 2 : KU_FS_MAX_IO_BYTES;
      if (next <= capacity) {
        ku_fs_set_error(error, "read_failed", "failed to read: buffer limit reached");
        failed = true;
        break;
      }
      uint8_t* grown;
      if (allocator) {
        grown=(uint8_t*)ku_fs_allocate(allocator,next);
        if (grown) { memcpy(grown,data,len); ku_fs_release(allocator,data); }
      } else grown=(uint8_t*)realloc(data,next);
      if (!grown) {
        ku_fs_set_error(error, "read_failed", "failed to read: out of memory");
        failed = true;
        break;
      }
      data = grown;
      capacity = next;
    }
    size_t got = fread(data + len, 1, capacity - len, file);
    if (got != 0) {
      len += got;
      continue;
    }
    if (ferror(file)) {
      ku_fs_set_error(error, "read_failed", "failed to read file");
      failed = true;
    }
    break;
  }
  if (fclose(file) != 0 && !failed) {
    ku_fs_set_error(error, "read_failed", "failed to close file after reading");
    failed = true;
  }
  if (!failed && !ku_fs_utf8_valid(data, len)) {
    ku_fs_set_error(error, "read_failed", "failed to read: file is not valid UTF-8");
    failed = true;
  }
  if (failed) {
    ku_fs_release(allocator,data);
    return false;
  }
  if (value) *value = (KuString){ data, len, capacity, KU_STRING_OWNED };
  else ku_fs_release(allocator,data);
  return true;
}
