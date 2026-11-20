#ifndef KU_BOOTSTRAP_FS_READ_H
#define KU_BOOTSTRAP_FS_READ_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* Keep the tagged carriers compatible with the native synchronous ABI. */
typedef struct KuString {
    uint8_t* ptr;
    size_t len;
    size_t capacity;
    uint8_t storage;
} KuString;
#ifndef KU_BOOTSTRAP_FS_NO_COMPAT_STRING
typedef KuString KuBootstrapString;
#endif

enum { KU_STRING_STATIC = 0, KU_STRING_OWNED = 1 };

typedef struct KuError {
    KuString domain;
    KuString code;
    KuString message;
} KuError;

typedef struct KuResult_str {
    bool ok;
    KuString value;
    KuError error;
} KuResult_str;

/* The caller owns path. A successful result owns its buffer independently.
 * Absolute paths are supplied by the compiler host. Relative paths, when
 * used directly, are interpreted from the caller's current directory. */
KuResult_str KuBootstrapFsRead(const KuString* path);

#endif
