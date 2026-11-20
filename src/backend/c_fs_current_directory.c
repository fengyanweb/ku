#if defined(_WIN32)
typedef wchar_t KuFsNativeChar;
static KuFsNativeChar* ku_fs_native_path_with_allocator(KuString path, KuError* error, const char* code, const char* message, KuFsAllocatorV1* allocator) {
  if (!ku_fs_path_valid(path)) { ku_fs_set_error(error, code, message); return NULL; }
  if (path.len == 0) {
    wchar_t* empty = (wchar_t*)ku_fs_allocate(allocator,2 * sizeof(wchar_t));
    if (!empty) { ku_fs_set_error(error, code, message); return NULL; }
    empty[0] = L'.';
    empty[1] = L'\0';
    return empty;
  }
  int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char*)path.ptr, (int)path.len, NULL, 0);
  if (count <= 0 || (size_t)count > SIZE_MAX / sizeof(wchar_t) - 1) {
    ku_fs_set_error(error, code, message); return NULL;
  }
  wchar_t* native = (wchar_t*)ku_fs_allocate(allocator,((size_t)count + 1) * sizeof(wchar_t));
  if (!native) { ku_fs_set_error(error, code, message); return NULL; }
  if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (const char*)path.ptr, (int)path.len, native, count) != count) {
    ku_fs_release(allocator,native); ku_fs_set_error(error, code, message); return NULL;
  }
  native[count] = L'\0';
  return native;
}
#else
typedef char KuFsNativeChar;
static KuFsNativeChar* ku_fs_native_path_with_allocator(KuString path, KuError* error, const char* code, const char* message, KuFsAllocatorV1* allocator) {
  if (!ku_fs_path_valid(path)) { ku_fs_set_error(error, code, message); return NULL; }
  size_t capacity = path.len == 0 ? 2 : path.len + 1;
  char* native = (char*)ku_fs_allocate(allocator,capacity);
  if (!native) { ku_fs_set_error(error, code, message); return NULL; }
  if (path.len) {
    memcpy(native, path.ptr, path.len);
    native[path.len] = '\0';
  } else {
    native[0] = '.';
    native[1] = '\0';
  }
  return native;
}
#endif
