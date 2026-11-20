#include "fs_read.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#include <wchar.h>
#endif

static KuString ku_string_static(const uint8_t* bytes, size_t length) {
    return (KuString){(uint8_t*)bytes, length, 0, KU_STRING_STATIC};
}

static KuError ku_error_make(KuString domain, KuString code, KuString message) {
    return (KuError){domain, code, message};
}

#include "c_fs_common.c"
#include "c_fs_current_directory.c"

#define ku_fs_native_path(path, error, code, message) \
    ku_fs_native_path_with_allocator(path, error, code, message, NULL)
#include "c_fs_read.c"
#undef ku_fs_native_path

KuResult_str KuBootstrapFsRead(const KuString* path) {
    KuResult_str result = {0};
    result.ok = ku_fs_read_impl(*path, &result.value, &result.error);
    return result;
}
