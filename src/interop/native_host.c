/* Emitted only into applications containing an interop call. */
#if defined(_WIN32)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#else
#include <dlfcn.h>
#include <unistd.h>
#ifdef __APPLE__
#include <mach-o/dyld.h>
#endif
#endif

#define KU_INTEROP_PAYLOAD_LIMIT ((size_t)16 * 1024 * 1024)
#define KU_INTEROP_PATH_LIMIT ((size_t)32768)

typedef int (*__KuInteropCall)(const char*, const KuInteropValue*, size_t, KuInteropValue*);
typedef void (*__KuInteropFree)(KuInteropValue*);

typedef struct {
    __KuInteropCall call;
    __KuInteropFree release;
} __KuInteropPlugin;

typedef struct {
    bool ok;
    int64_t integer;
    KuString text;
    KuError error;
} __KuInteropHostResult;

static KuError __ku_interop_error(const char* code, KuString message) {
    return ku_error_make(
        ku_string_static((const uint8_t*)"interop", sizeof("interop") - 1),
        ku_string_static((const uint8_t*)code, strlen(code)), message);
}

static __KuInteropHostResult __ku_interop_failed(const char* code, const char* message) {
    return (__KuInteropHostResult){ false, 0, {0},
        __ku_interop_error(code, ku_string_static((const uint8_t*)message, strlen(message))) };
}

static bool __ku_interop_utf8(const uint8_t* bytes, size_t length) {
    size_t i = 0;
    while (i < length) {
        uint8_t a = bytes[i++];
        if (a <= 0x7f) continue;
        if (a < 0xc2 || a > 0xf4 || i == length) return false;
        uint8_t b = bytes[i++];
        if ((b & 0xc0) != 0x80) return false;
        if (a < 0xe0) continue;
        if ((a == 0xe0 && b < 0xa0) || (a == 0xed && b >= 0xa0) ||
            (a == 0xf0 && b < 0x90) || (a == 0xf4 && b >= 0x90) ||
            i == length || (bytes[i++] & 0xc0) != 0x80) return false;
        if (a >= 0xf0 && (i == length || (bytes[i++] & 0xc0) != 0x80)) return false;
    }
    return true;
}

static bool __ku_interop_is_ascii_alnum(uint8_t byte) {
    return (byte >= 'a' && byte <= 'z') || (byte >= 'A' && byte <= 'Z') ||
        (byte >= '0' && byte <= '9');
}

static bool __ku_interop_relative_locator(KuString path) {
    static const char prefix[] = "ku-runtime.d/interop/";
    if (!path.ptr || path.len <= sizeof(prefix) - 1 || path.len > KU_INTEROP_PATH_LIMIT ||
        memcmp(path.ptr, prefix, sizeof(prefix) - 1) != 0) return false;
    for (size_t start = sizeof(prefix) - 1, i = start; i <= path.len; ++i) {
        if (i == path.len || path.ptr[i] == '/') {
            size_t count = i - start;
            if (count == 0 || (count == 1 && path.ptr[start] == '.') ||
                (count == 2 && path.ptr[start] == '.' && path.ptr[start + 1] == '.')) return false;
            start = i + 1;
        } else if (!__ku_interop_is_ascii_alnum(path.ptr[i]) &&
                   path.ptr[i] != '.' && path.ptr[i] != '-' && path.ptr[i] != '_') {
            return false;
        }
    }
    return true;
}

static bool __ku_interop_absolute_locator(KuString path) {
    if (!path.ptr || path.len == 0 || path.len > KU_INTEROP_PATH_LIMIT ||
        memchr(path.ptr, 0, path.len) != NULL) return false;
#ifdef _WIN32
    return path.len >= 3 &&
        ((path.ptr[0] == '\\' && path.ptr[1] == '\\') ||
         (__ku_interop_is_ascii_alnum(path.ptr[0]) && path.ptr[1] == ':' &&
          (path.ptr[2] == '\\' || path.ptr[2] == '/')));
#else
    return path.ptr[0] == '/';
#endif
}

#ifdef _WIN32
static HMODULE __ku_interop_open_module(KuString locator, bool relative) {
    wchar_t path[KU_INTEROP_PATH_LIMIT + 1];
    size_t directory_len = 0;
    if (relative) {
        DWORD length = GetModuleFileNameW(NULL, path, (DWORD)(KU_INTEROP_PATH_LIMIT + 1));
        if (length == 0 || length > (DWORD)KU_INTEROP_PATH_LIMIT) return NULL;
        directory_len = (size_t)length;
        while (directory_len && path[directory_len - 1] != L'\\' &&
               path[directory_len - 1] != L'/') --directory_len;
        if (!directory_len) return NULL;
    }
    int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS,
        (const char*)locator.ptr, (int)locator.len, path + directory_len,
        (int)(KU_INTEROP_PATH_LIMIT - directory_len));
    if (length <= 0) return NULL;
    for (size_t i = directory_len; i < directory_len + (size_t)length; ++i)
        if (path[i] == L'/') path[i] = L'\\';
    path[directory_len + (size_t)length] = L'\0';
    /* Restrict dependency search to the verified companion's directory,
       application directory, System32 and explicitly registered safe dirs. */
    return LoadLibraryExW(path, NULL,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
}
#else
static void* __ku_interop_open_module(KuString locator, bool relative) {
    char path[KU_INTEROP_PATH_LIMIT + 1];
    size_t directory_len = 0;
    if (relative) {
#ifdef __APPLE__
        uint32_t capacity = (uint32_t)sizeof(path);
        if (_NSGetExecutablePath(path, &capacity) != 0) return NULL;
        directory_len = strlen(path);
#elif defined(__linux__)
        ssize_t length = readlink("/proc/self/exe", path, sizeof(path));
        if (length < 0 || (size_t)length >= sizeof(path)) return NULL;
        directory_len = (size_t)length;
#else
        return NULL;
#endif
        while (directory_len && path[directory_len - 1] != '/') --directory_len;
        if (!directory_len) return NULL;
    }
    if (locator.len > KU_INTEROP_PATH_LIMIT - directory_len) return NULL;
    memcpy(path + directory_len, locator.ptr, locator.len);
    path[directory_len + locator.len] = '\0';
    return dlopen(path, RTLD_NOW | RTLD_LOCAL);
}
#endif

static bool __ku_interop_load(__KuInteropPlugin* owner, KuString locator) {
    bool relative = __ku_interop_relative_locator(locator);
    if (!relative && !__ku_interop_absolute_locator(locator)) return false;
#ifdef _WIN32
    HMODULE module = __ku_interop_open_module(locator, relative);
    if (!module) return false;
    __KuInteropCall call = (__KuInteropCall)GetProcAddress(module, "ku_interop_call_v1");
    __KuInteropFree release = (__KuInteropFree)GetProcAddress(module, "ku_interop_free_v1");
    if (!call || !release) {
        FreeLibrary(module);
        return false;
    }
#else
    void* module = __ku_interop_open_module(locator, relative);
    if (!module) return false;
    __KuInteropCall call = (__KuInteropCall)dlsym(module, "ku_interop_call_v1");
    __KuInteropFree release = (__KuInteropFree)dlsym(module, "ku_interop_free_v1");
    if (!call || !release) {
        dlclose(module);
        return false;
    }
#endif
    /* The plugin retains its module/function/owner state for this process.
       Hold the OS module reference for the lifetime of these function pointers. */
    owner->call = call;
    owner->release = release;
    return true;
}

static bool __ku_interop_valid_value(KuInteropValue value, uint32_t expected) {
    if (value.reserved != 0 || value.tag != expected) return false;
    if (expected == KU_INTEROP_STR) {
        return value.integer == 0 && value.length <= KU_INTEROP_PAYLOAD_LIMIT &&
            (value.length == 0 || (value.data && __ku_interop_utf8(value.data, value.length)));
    }
    return !value.data && value.length == 0 &&
        (expected == KU_INTEROP_INT || value.integer == 0 ||
         (expected == KU_INTEROP_BOOL && value.integer == 1));
}

static __KuInteropHostResult __ku_interop_perform(
    __KuInteropPlugin* owner, KuString locator, KuString operation,
    const KuInteropValue* args, size_t nargs, uint32_t return_tag) {
    /* The synchronously callable subset cannot block an HTTP owner. Task
       lowering does not admit the interop intrinsics. */
    if (__ku_handler_deadline != 0)
        return __ku_interop_failed("unavailable", "interop requires synchronous CLI execution");
    char name[201];
    if (!operation.ptr || !operation.len || operation.len >= sizeof(name))
        return __ku_interop_failed("contract_error", "invalid interop operation name");
    for (size_t i = 0; i < operation.len; ++i) {
        uint8_t ch = operation.ptr[i];
        if (!__ku_interop_is_ascii_alnum(ch) && ch != '_' && ch != '.' && ch != ':' && ch != '-')
            return __ku_interop_failed("contract_error", "invalid interop operation name");
        name[i] = (char)ch;
    }
    name[operation.len] = '\0';
    if (!owner->call && !__ku_interop_load(owner, locator))
        return __ku_interop_failed("load_error", "could not load the Ku interop companion");
    for (size_t i = 0; i < nargs; ++i) {
        if (args[i].tag == KU_INTEROP_STR && args[i].length > KU_INTEROP_PAYLOAD_LIMIT)
            return __ku_interop_failed("contract_error", "interop text argument exceeds the byte limit");
    }
    KuInteropValue value = {0};
    int status = owner->call(name, args, nargs, &value);
    uint32_t expected = status == 0 ? return_tag : KU_INTEROP_STR;
    if (!__ku_interop_valid_value(value, expected)) {
        owner->release(&value);
        return __ku_interop_failed("contract_error", "interop producer returned an invalid value");
    }
    KuString text = {0};
    if (expected == KU_INTEROP_STR && value.length) {
        uint8_t* copy = (uint8_t*)malloc(value.length);
        if (!copy) {
            owner->release(&value);
            return __ku_interop_failed("allocation_failed", "could not copy the interop result");
        }
        memcpy(copy, value.data, value.length);
        text = (KuString){ copy, value.length, value.length, KU_STRING_OWNED };
    }
    int64_t integer = value.integer;
    owner->release(&value);
    if (status != 0) return (__KuInteropHostResult){ false, 0, {0}, __ku_interop_error("call_error", text) };
    return (__KuInteropHostResult){ true, integer, text, {0} };
}

static inline KuInteropValue __ku_interop_arg_null(void) {
    return (KuInteropValue){ KU_INTEROP_NULL, 0, 0, NULL, 0 };
}

static inline KuInteropValue __ku_interop_arg_int(int64_t value) {
    return (KuInteropValue){ KU_INTEROP_INT, 0, value, NULL, 0 };
}

static inline KuInteropValue __ku_interop_arg_bool(bool value) {
    return (KuInteropValue){ KU_INTEROP_BOOL, 0, value, NULL, 0 };
}

static inline KuInteropValue __ku_interop_arg_str(KuString value) {
    return (KuInteropValue){ KU_INTEROP_STR, 0, 0, value.ptr, value.len };
}
