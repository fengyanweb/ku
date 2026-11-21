#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct { uint8_t *ptr; size_t len; size_t capacity; uint8_t storage; } KuProjectionString;
typedef KuProjectionString KuString;
enum { KU_STRING_OWNED = 1 };

static KuString ku_string_alloc(size_t len) {
    if (!len) return (KuString){0};
    uint8_t *data = malloc(len);
    if (!data) {
        fputs("out of memory\n", stderr);
        exit(1);
    }
    return (KuString){data, len, len, KU_STRING_OWNED};
}

/* A standalone synchronous program has no HTTP handler deadline. */
static int __ku_handler_timeout_poll(void) { return 0; }

#include "c_string_unicode_base.c"
#include "string_trim_whitespace.h"
#include "c_string_unicode_trim.c"

KuProjectionString KuBootstrapStringTrim(const KuProjectionString *input) {
    return ku_string_unicode_trim(*input);
}
