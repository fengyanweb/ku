/* Keep the generated runtime and exact entry symbol in this translation unit. */
#define main __ku_bootstrap_unused_main
#include "driver.c"
#undef main
#ifdef _WIN32
#include <fcntl.h>
#include <io.h>
#include <wchar.h>
#include <windows.h>
#else
#include <unistd.h>
#endif

typedef KuResult_null (*BootstrapEntrySignature)(const KuString*, const KuString*, const KuString*);
_Static_assert(
    _Generic(&BootstrapEntry, BootstrapEntrySignature: 1, default: 0),
    "bootstrap driver must provide BootstrapEntry(&str, &str, &str): null!"
);

enum { ARG_BYTES = 512, ABSOLUTE_PATH_BYTES = 4096 };

#ifdef _WIN32
static bool wide_string_view(const wchar_t* word, int units, char* utf8,
                             int capacity, KuString* view) {
    int bytes = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, word, units,
                                    utf8, capacity - 1, NULL, NULL);
    if (bytes <= 0) return false;
    utf8[bytes] = '\0';
    *view = ku_string_static((const uint8_t*)utf8, (size_t)bytes);
    return true;
}

static bool argument_view(const wchar_t* word, char utf8[ARG_BYTES + 1], KuString* view) {
    int units = 0;
    while (units <= ARG_BYTES && word[units]) units++;
    return units > 0 && units <= ARG_BYTES &&
           wide_string_view(word, units, utf8, ARG_BYTES + 1, view);
}

static bool caller_path(const wchar_t* input, KuString* path,
                        char absolute[ABSOLUTE_PATH_BYTES]) {
    wchar_t wide[ABSOLUTE_PATH_BYTES];
    DWORD length = GetFullPathNameW(input, ABSOLUTE_PATH_BYTES, wide, NULL);
    if (!length || length >= ABSOLUTE_PATH_BYTES) return false;
    /* The Ku project binder uses slash-delimited logical paths. Ordinary DOS
       and UNC paths allow '/', but Win32 device/verbatim paths do not. */
    for (DWORD index = 0; index < length; index++) {
        if (wide[index] == L'\\') wide[index] = L'/';
    }
    if (length >= 4 && wide[0] == L'/' && wide[1] == L'/' &&
        (wide[2] == L'?' || wide[2] == L'.') && wide[3] == L'/') return false;
    return wide_string_view(wide, (int)length, absolute, ABSOLUTE_PATH_BYTES, path);
}
#else
static bool argument_view(const char* word, KuString* view) {
    size_t length = 0;
    while (length <= ARG_BYTES && word[length]) length++;
    if (!length || length > ARG_BYTES ||
        !ku_fs_utf8_valid((const uint8_t*)word, length)) {
        return false;
    }
    *view = ku_string_static((const uint8_t*)word, length);
    return true;
}

static bool caller_path(KuString* path, char absolute[ABSOLUTE_PATH_BYTES]) {
    if (path->ptr[0] == '/') return true;
    if (!getcwd(absolute, ABSOLUTE_PATH_BYTES)) return false;
    size_t length = strlen(absolute);
    size_t separator = length && absolute[length - 1] != '/';
    if (path->len + separator >= ABSOLUTE_PATH_BYTES - length) return false;
    if (separator) absolute[length++] = '/';
    memcpy(absolute + length, path->ptr, path->len);
    length += path->len;
    absolute[length] = '\0';
    if (!ku_fs_utf8_valid((const uint8_t*)absolute, length)) return false;
    *path = ku_string_static((const uint8_t*)absolute, length);
    return true;
}
#endif

static int finish(KuResult_null result) {
    KuSyncExitSignal signal = __ku_sync_take();
    if (signal.kind != KU_SYNC_EXIT_NONE) {
        ku_result_drop_null(&result);
        ku_output_stream_write(stderr, __ku_sync_error_message(signal),
                               __ku_sync_error_message_length(signal));
        ku_output_stream_write(stderr, "\n", 1);
        return 1;
    }
    int status = result.ok ? 0 : 1;
    if (status) {
        ku_string_write(stderr, result.error.message);
        ku_output_stream_write(stderr, "\n", 1);
    }
    ku_result_drop_null(&result);
    return status;
}

static int usage(void) {
    fputs("usage: ku-bootstrap check <file.ku>\n"
          "       ku-bootstrap check-project <modules.txt>\n"
         "       ku-bootstrap emit <file.ku> <function>\n"
          "       ku-bootstrap emit-project <modules.txt> <function>\n"
          "       ku-bootstrap emit-program <modules.txt>\n", stderr);
    return 2;
}

static int invalid_argument(void) {
    fputs("compiler arguments must contain 1 to 512 bytes of UTF-8\n", stderr);
    return 2;
}

static int invalid_path(void) {
    fputs("cannot resolve the input path within the UTF-8 path limit\n", stderr);
    return 2;
}

static int invoke(const KuString* command, const KuString* path, const KuString* symbol) {
    __ku_sync_reset();
    /* These views live through the synchronous call. Ku owns anything it clones. */
    return finish(BootstrapEntry(command, path, symbol));
}

#ifdef _WIN32
int wmain(int argc, wchar_t** argv) {
    if ((argc != 3 && argc != 4) ||
        (argc == 3 && wcscmp(argv[1], L"check") && wcscmp(argv[1], L"check-project") &&
         wcscmp(argv[1], L"emit-program")) ||
        (argc == 4 && wcscmp(argv[1], L"emit") && wcscmp(argv[1], L"emit-project"))) return usage();

    char command_bytes[ARG_BYTES + 1], path_bytes[ARG_BYTES + 1], symbol_bytes[ARG_BYTES + 1];
    KuString command = {0}, path = {0}, symbol = {0};
    if (!argument_view(argv[1], command_bytes, &command) ||
        !argument_view(argv[2], path_bytes, &path) ||
        (argc == 4 && !argument_view(argv[3], symbol_bytes, &symbol))) return invalid_argument();

    char absolute[ABSOLUTE_PATH_BYTES];
    if (!caller_path(argv[2], &path, absolute)) return invalid_path();
    if ((argc == 4 || !wcscmp(argv[1], L"emit-program")) && _setmode(_fileno(stdout), _O_BINARY) == -1) {
        fputs("cannot write emitted C in binary mode\n", stderr);
        return 1;
    }
    return invoke(&command, &path, &symbol);
}
#else
int main(int argc, char** argv) {
    if ((argc != 3 && argc != 4) ||
        (argc == 3 && strcmp(argv[1], "check") && strcmp(argv[1], "check-project") &&
         strcmp(argv[1], "emit-program")) ||
        (argc == 4 && strcmp(argv[1], "emit") && strcmp(argv[1], "emit-project"))) return usage();

    KuString command = {0}, path = {0}, symbol = {0};
    if (!argument_view(argv[1], &command) || !argument_view(argv[2], &path) ||
        (argc == 4 && !argument_view(argv[3], &symbol))) return invalid_argument();
    char absolute[ABSOLUTE_PATH_BYTES];
    if (!caller_path(&path, absolute)) return invalid_path();
    return invoke(&command, &path, &symbol);
}
#endif
