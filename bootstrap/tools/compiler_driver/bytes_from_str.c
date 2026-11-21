#include "bytes_abi.h"

KuBootstrapResult_KuProjectionString KuBootstrapBytesFromString(const KuProjectionString *input) {
    ku_bootstrap_bytes_storage_helpers();
    return ku_bytes_from_str(*input);
}
