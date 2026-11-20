#ifdef _WIN32
#error "The bootstrap driver needs a UTF-16 argv adapter on Windows"
#endif

/* Keep the generated runtime and exact entry symbol in this translation unit. */
#define main __ku_bootstrap_unused_main
#include "driver.c"
#undef main
#include <unistd.h>

typedef KuResult_null (*BootstrapEntrySignature)(const KuString*, const KuString*, const KuString*);
_Static_assert(
    _Generic(&BootstrapEntry, BootstrapEntrySignature: 1, default: 0),
    "bootstrap driver must provide BootstrapEntry(&str, &str, &str): null!"
);

enum { ARG_BYTES = 512, ABSOLUTE_PATH_BYTES = 4096 };

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

int main(int argc, char** argv) {
    if ((argc != 3 && argc != 4) ||
        (argc == 3 && strcmp(argv[1], "check") && strcmp(argv[1], "check-project")) ||
        (argc == 4 && strcmp(argv[1], "emit"))) {
        fputs("usage: ku-bootstrap check <file.ku>\n"
              "       ku-bootstrap check-project <modules.txt>\n"
              "       ku-bootstrap emit <file.ku> <function>\n", stderr);
        return 2;
    }

    KuString command = {0}, path = {0}, symbol = {0};
    if (!argument_view(argv[1], &command) || !argument_view(argv[2], &path) ||
        (argc == 4 && !argument_view(argv[3], &symbol))) {
        fputs("compiler arguments must contain 1 to 512 bytes of UTF-8\n", stderr);
        return 2;
    }
    char absolute[ABSOLUTE_PATH_BYTES];
    if (!caller_path(&path, absolute)) {
        fputs("cannot resolve the input path within the UTF-8 path limit\n", stderr);
        return 2;
    }

    __ku_sync_reset();
    /* These views live through the synchronous call. Ku owns anything it clones. */
    return finish(BootstrapEntry(&command, &path, &symbol));
}
