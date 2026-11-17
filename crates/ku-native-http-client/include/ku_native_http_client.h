#ifndef KU_NATIVE_HTTP_CLIENT_V1_H
#define KU_NATIVE_HTTP_CLIENT_V1_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* Private generated-runtime ABI. Input ranges are readable UTF-8 byte ranges,
 * remain live until the synchronous call returns, and may alias each other.
 * Output and descriptor storage must not overlap input storage. */
typedef struct KuHttpClientSliceV1 { const uint8_t* data; size_t len; } KuHttpClientSliceV1;
typedef struct KuHttpClientHeaderV1 { KuHttpClientSliceV1 name, value; } KuHttpClientHeaderV1;
typedef struct KuHttpClientInputV1 {
  uint32_t size, flags; /* bit 0: body is present (empty differs from absent). */
  KuHttpClientSliceV1 method, url, body;
  const KuHttpClientHeaderV1* headers;
  size_t header_count;
  uint64_t timeout_ms;
  uint64_t caller_budget_ms; /* zero means no earlier caller deadline. */
  size_t max_body_bytes;
} KuHttpClientInputV1;
typedef struct KuHttpClientOutputV1 {
  uint32_t size, status;
  int64_t http_status;
  KuHttpClientSliceV1 body;
  const KuHttpClientHeaderV1* headers;
  size_t header_count;
  KuHttpClientSliceV1 code, message;
  void* owner;
} KuHttpClientOutputV1;
/* Return statuses: 0 response, 1 HTTP Result error, 2 malformed ABI, 255 caught
 * panic. No C allocator may free returned views. Copy them before matched
 * release. Output is zero-initialized except size; live owners are rejected. */
uint32_t __ku_http_client_abi_version(void);
uint32_t __ku_http_client_v1_build_id(const uint8_t**, size_t*);
uint32_t __ku_http_client_call_v1(const KuHttpClientInputV1*, KuHttpClientOutputV1*);
uint32_t __ku_http_client_release_v1(KuHttpClientOutputV1*);
#ifdef __cplusplus
}
#endif
#endif
