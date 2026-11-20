#define KU_FS_MAX_PATH_BYTES ((size_t)32768)
#define KU_FS_MAX_IO_BYTES ((size_t)1000000)
#define KU_FS_MAX_DIRECTORY_ENTRIES ((size_t)10000)
#define KU_FS_MAX_DIRECTORY_OUTPUT_BYTES ((size_t)1000000)

typedef struct KuFsAllocatorV1 {
  void* context;
  void* (*allocate)(void*, size_t);
  void (*release)(void*, void*);
} KuFsAllocatorV1;
static void* ku_fs_allocate(KuFsAllocatorV1* allocator, size_t bytes) {
  return allocator ? allocator->allocate(allocator->context,bytes) : malloc(bytes);
}
static void ku_fs_release(KuFsAllocatorV1* allocator, void* pointer) {
  if (allocator) allocator->release(allocator->context,pointer); else free(pointer);
}

static bool ku_fs_utf8_valid(const uint8_t* data, size_t len) {
  if (len != 0 && !data) return false;
  size_t i = 0;
  while (i < len) {
    uint8_t c = data[i];
    if (c <= 0x7f) { i++; continue; }
    if (c >= 0xc2 && c <= 0xdf) {
      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return false;
      i += 2; continue;
    }
    if (c == 0xe0) {
      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return false;
      i += 3; continue;
    }
    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {
      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return false;
      i += 3; continue;
    }
    if (c == 0xed) {
      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return false;
      i += 3; continue;
    }
    if (c == 0xf0) {
      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return false;
      i += 4; continue;
    }
    if (c >= 0xf1 && c <= 0xf3) {
      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return false;
      i += 4; continue;
    }
    if (c == 0xf4) {
      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return false;
      i += 4; continue;
    }
    return false;
  }
  return true;
}

static bool ku_fs_path_valid(KuString path) {
  if (path.len > KU_FS_MAX_PATH_BYTES) return false;
  if (!ku_fs_utf8_valid(path.ptr, path.len)) return false;
  return path.len == 0 || memchr(path.ptr, 0, path.len) == NULL;
}

static void ku_fs_set_error(KuError* error, const char* code, const char* message) {
  if (!error) return;
  *error = ku_error_make(
      ku_string_static((const uint8_t*)"fs", 2),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}
