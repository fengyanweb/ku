#ifndef KU_TASK_NET_REACTOR_ABI_V1
#define KU_TASK_NET_REACTOR_ABI_V1 1
#define KU_TASK_NET_POLL_CAPACITY 1024u
enum { KU_TASK_NET_READ=1u,KU_TASK_NET_WRITE=2u,KU_TASK_NET_ERROR=4u,KU_TASK_NET_HUP=8u };
typedef struct KuTaskNetPollV1 {
  uintptr_t socket_value;
  size_t slot;
  uint64_t generation;
  uint32_t events;
} KuTaskNetPollV1;
#endif
