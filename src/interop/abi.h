#ifndef KU_INTEROP_ABI_V1_H
#define KU_INTEROP_ABI_V1_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#define KU_INTEROP_EXPORT __declspec(dllexport)
#else
#define KU_INTEROP_EXPORT __attribute__((visibility("default")))
#endif

#define KU_INTEROP_NULL 0u
#define KU_INTEROP_INT 1u
#define KU_INTEROP_STR 2u
#define KU_INTEROP_BOOL 3u

typedef struct KuInteropValue {
    uint32_t tag;
    uint32_t reserved;
    int64_t integer;
    const uint8_t *data;
    size_t length;
} KuInteropValue;

#ifndef KU_INTEROP_HOST
#ifdef __cplusplus
extern "C" {
#endif

/* Arguments are borrowed for this call. The producer owns its output until
 * ku_interop_free_v1. Zero denotes success; any other status returns a string
 * error. A string is UTF-8 with the given byte length, without a required NUL.
 * The host calls free once on the calling thread before another plugin call.
 * Both functions must contain their own language exceptions. */
KU_INTEROP_EXPORT int ku_interop_call_v1(
    const char *operation, const KuInteropValue *args, size_t nargs, KuInteropValue *result);
KU_INTEROP_EXPORT void ku_interop_free_v1(KuInteropValue *result);

#ifdef __cplusplus
}
#endif
#endif /* KU_INTEROP_HOST */

#endif
