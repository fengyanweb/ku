#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct { uint8_t *ptr; size_t len; size_t capacity; uint8_t storage; } KuProjectionString;
typedef struct { KuProjectionString domain, code, message; } KuBootstrapError;
typedef struct {
    bool ok;
    KuProjectionString value;
    KuBootstrapError error;
} KuBootstrapResult_KuProjectionString;

typedef KuProjectionString KuString;
typedef KuBootstrapError KuError;
typedef KuBootstrapResult_KuProjectionString KuResult_str;

enum { KU_STRING_STATIC = 0, KU_STRING_OWNED = 1 };

static KuString ku_string_static(const uint8_t *bytes, size_t len) {
    return (KuString){(uint8_t *)bytes, len, 0, KU_STRING_STATIC};
}

static KuString ku_string_alloc(size_t len) {
    if (!len) return (KuString){0};
    uint8_t *data = malloc(len);
    if (!data) {
        fputs("out of memory\n", stderr);
        exit(EXIT_FAILURE);
    }
    return (KuString){data, len, len, KU_STRING_OWNED};
}

static KuError ku_error_make(KuString domain, KuString code, KuString message) {
    return (KuError){domain, code, message};
}

static size_t ku_string_char_len(KuString value) {
    size_t count = 0;
    for (size_t i = 0; i < value.len; ++i) {
        if ((value.ptr[i] & 0xc0u) != 0x80u) ++count;
    }
    return count;
}

#include "c_string_slice.c"

KuBootstrapResult_KuProjectionString KuBootstrapStringSlice(const KuProjectionString *input,
                                                           int64_t start, int64_t end) {
    return ku_string_slice(*input, start, end);
}
