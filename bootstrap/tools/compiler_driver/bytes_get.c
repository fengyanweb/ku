#include "bytes_abi.h"
#include "c_bytes_index.c"

KuBootstrapResult_int64_t KuBootstrapBytesGet(const KuProjectionString *input, int64_t index) {
    ku_bootstrap_bytes_storage_helpers();
    (void)ku_bytes_len;
    return ku_bytes_get(*input, index);
}
