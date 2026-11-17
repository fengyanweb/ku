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
/* Private owner-stepped transport. Only DNS may run on the existing bounded
 * resolver pool. Operation and callbacks are pinned by the physical job until
 * final retirement; I/O callbacks never wait. Reserve covers live old+new buffers;
 * refund releases an allocation only after it is freed. Drop never calls callbacks. */
typedef struct KuHttpClientAsyncV1 KuHttpClientAsyncV1;
typedef struct KuHttpClientAsyncIoV1 {
  void* context;
  intptr_t (*read)(void*,uint8_t*,size_t);
  intptr_t (*write)(void*,const uint8_t*,size_t);
} KuHttpClientAsyncIoV1;
typedef struct KuHttpClientAsyncBudgetV1 {
  void* context; uint32_t (*reserve)(void*,size_t); uint32_t (*refund)(void*,size_t);
} KuHttpClientAsyncBudgetV1;
typedef struct KuHttpClientAsyncInfoV1 {
  KuHttpClientSliceV1 host; uint32_t port,tls,done;
} KuHttpClientAsyncInfoV1;
uint32_t __ku_http_client_async_new_v1(const KuHttpClientInputV1*,KuHttpClientAsyncBudgetV1,KuHttpClientAsyncV1**);
uint32_t __ku_http_client_async_info_v1(KuHttpClientAsyncV1*,KuHttpClientAsyncInfoV1*);
/* 1 read, 2 write, 4 complete, 8 redirect needing a fresh bounded DNS job. */
uint32_t __ku_http_client_async_step_v1(KuHttpClientAsyncV1*,const KuHttpClientAsyncIoV1*,uint32_t);
uint32_t __ku_http_client_async_take_v1(KuHttpClientAsyncV1*,KuHttpClientOutputV1*);
uint32_t __ku_http_client_async_drop_v1(KuHttpClientAsyncV1*);
size_t __ku_http_client_async_park_v1(KuHttpClientAsyncV1*);
/* reuse: 2 does not consume cached; 0/255 consume it exactly once. */
uint32_t __ku_http_client_async_reuse_v1(KuHttpClientAsyncV1*,KuHttpClientAsyncV1*);

#ifdef __cplusplus
}
#endif
#endif
