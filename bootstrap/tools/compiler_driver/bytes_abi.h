#ifndef KU_BOOTSTRAP_BYTES_ABI_H
#define KU_BOOTSTRAP_BYTES_ABI_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* The typed plan distinguishes bytes from str. Both C carriers use this
 * layout and the native static/owned storage tags. */
typedef struct { uint8_t *ptr; size_t len, capacity; uint8_t storage; } KuProjectionString;
typedef struct { KuProjectionString domain, code, message; } KuBootstrapError;
typedef struct {
    bool ok;
    KuProjectionString value;
    KuBootstrapError error;
} KuBootstrapResult_KuProjectionString;
typedef struct { bool ok; int64_t value; KuBootstrapError error; } KuBootstrapResult_int64_t;

typedef KuProjectionString KuString;
typedef KuProjectionString KuBytes;
typedef KuBootstrapError KuError;
typedef KuBootstrapResult_KuProjectionString KuResult_bytes;
typedef KuBootstrapResult_int64_t KuResult_int;
enum { KU_BYTES_STATIC = 0, KU_BYTES_OWNED = 1 };

static inline KuString ku_string_static(const uint8_t *bytes, size_t len) {
    return (KuString){(uint8_t *)bytes, len, 0, KU_BYTES_STATIC};
}

static inline KuError ku_error_make(KuString domain, KuString code, KuString message) {
    return (KuError){domain, code, message};
}

#include "c_bytes_storage.c"

/* The native source is embedded verbatim and also contains helpers for
 * other operations. Reference them for strict unused-function diagnostics. */
static inline void ku_bootstrap_bytes_storage_helpers(void) {
    (void)ku_move_bytes;
    (void)ku_clone_bytes;
    (void)ku_drop_bytes;
    (void)ku_bytes_from_str;
}

#endif
