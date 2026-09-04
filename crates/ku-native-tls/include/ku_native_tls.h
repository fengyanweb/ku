#ifndef KU_NATIVE_TLS_H
#define KU_NATIVE_TLS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define KU_TLS_ABI_VERSION 1u

#define KU_TLS_STATUS_OK 0u
#define KU_TLS_STATUS_NULL_POINTER 1u
#define KU_TLS_STATUS_INVALID_ARGUMENT 2u
#define KU_TLS_STATUS_LIMIT_EXCEEDED 3u
#define KU_TLS_STATUS_INVALID_DNS_NAME 4u
#define KU_TLS_STATUS_INVALID_CA 5u
#define KU_TLS_STATUS_TLS_ERROR 6u
#define KU_TLS_STATUS_SESSION_FAILED 7u
#define KU_TLS_STATUS_TRUNCATED 8u
#define KU_TLS_STATUS_WOULD_BLOCK 9u
#define KU_TLS_STATUS_IO_ERROR 10u
#define KU_TLS_STATUS_PANIC 255u

#define KU_TLS_ROOTS_WEBPKI 0u
#define KU_TLS_ROOTS_CUSTOM_PEM 1u

#define KU_TLS_MAX_CA_PEM_BYTES 4194304u
#define KU_TLS_MAX_CA_CERTIFICATES 1024u
#define KU_TLS_MAX_CA_DER_BYTES 65536u
#define KU_TLS_MAX_IO_BYTES 65536u
#define KU_TLS_MAX_HANDSHAKE_BYTES 1048576u
#define KU_TLS_MAX_HANDSHAKE_ITERATIONS 4096u
#define KU_TLS_MAX_SERVER_NAME_BYTES 253u

typedef struct KuTlsConfig KuTlsConfig;
typedef struct KuTlsClientSession KuTlsClientSession;

/* Every non-null handle must originate from this ABI. Its matching drop
 * consumes it exactly once; never access or drop it again. Calls using the same
 * session must be externally serialized. A config may be shared by concurrent
 * client_new calls, but config_drop must not overlap them. Every output slot must be aligned,
 * writable, and disjoint from opaque handles, input buffers, and other outputs.
 * Byte buffers must be valid for their stated sizes. PANIC is terminal for any
 * involved handle. After PANIC from a non-drop call, its matching drop may be
 * attempted once; a drop that returns PANIC must never be retried. */
uint32_t ku_tls_abi_version(void);
uint32_t ku_tls_v1_build_id(const uint8_t **out_data, size_t *out_len);

/* WEBPKI requires a null/zero PEM. CUSTOM_PEM is non-empty and completely
 * replaces WebPKI roots; system roots and disabled verification are absent. */
uint32_t ku_tls_v1_config_new(uint32_t root_mode,
                              const uint8_t *custom_ca_pem,
                              size_t custom_ca_pem_len,
                              KuTlsConfig **out_config);
uint32_t ku_tls_v1_config_drop(KuTlsConfig *config);

uint32_t ku_tls_v1_client_new(const KuTlsConfig *config,
                              const uint8_t *server_name,
                              size_t server_name_len,
                              KuTlsClientSession **out_session);
uint32_t ku_tls_v1_client_drop(KuTlsClientSession *session);

uint32_t ku_tls_v1_client_wants_read(const KuTlsClientSession *session,
                                     uint32_t *out_wants_read);
uint32_t ku_tls_v1_client_wants_write(const KuTlsClientSession *session,
                                      uint32_t *out_wants_write);
uint32_t ku_tls_v1_client_is_handshaking(const KuTlsClientSession *session,
                                         uint32_t *out_is_handshaking);
uint32_t ku_tls_v1_client_peer_closed(const KuTlsClientSession *session,
                                      uint32_t *out_peer_closed);

/* Feed only a non-empty buffer while wants_read is true, then process exactly
 * once after every successful feed. Plaintext writes and drain/read capacities
 * must also be non-zero. Drain may be partial; while wants_write is true,
 * success makes progress. */
uint32_t ku_tls_v1_client_feed_ciphertext(KuTlsClientSession *session,
                                          const uint8_t *ciphertext,
                                          size_t ciphertext_len,
                                          size_t *out_consumed);
uint32_t ku_tls_v1_client_process(KuTlsClientSession *session);
uint32_t ku_tls_v1_client_drain_ciphertext(KuTlsClientSession *session,
                                           uint8_t *output,
                                           size_t output_capacity,
                                           size_t *out_written);
uint32_t ku_tls_v1_client_write_plaintext(KuTlsClientSession *session,
                                          const uint8_t *plaintext,
                                          size_t plaintext_len,
                                          size_t *out_written);
uint32_t ku_tls_v1_client_read_plaintext(KuTlsClientSession *session,
                                         uint8_t *output,
                                         size_t output_capacity,
                                         size_t *out_read);
uint32_t ku_tls_v1_client_send_close_notify(KuTlsClientSession *session);
uint32_t ku_tls_v1_client_notify_eof(KuTlsClientSession *session);

#ifdef __cplusplus
}
#endif

#endif
