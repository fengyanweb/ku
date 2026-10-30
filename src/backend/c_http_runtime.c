#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1
#if !defined(_WIN32)
#include <sys/uio.h>
#endif
#if defined(__linux__)
#include <sys/epoll.h>
#include <sys/resource.h>
#include <dirent.h>
#define KU_HTTP_PERSISTENT_EVENT_LOOP 1
#elif defined(__APPLE__)
#include <sys/event.h>
#define KU_HTTP_PERSISTENT_EVENT_LOOP 1
#endif
#if defined(__APPLE__)
#include <sys/select.h>
#endif
#define KU_HTTP_PAUSED_PEER_PROBE_MS 25u
#define KU_HTTP_MAX_SEGS 64
/* Longest accepted request target, pinned to the interpreter's
   MAX_REQUEST_TARGET_BYTES (src/stdlib/http.rs). A longer target is answered with
   414 before it is copied or routed -- never truncated: truncating would let two
   distinct paths that share a long prefix resolve to the same route. */
#define KU_HTTP_MAX_TARGET 8192
#define KU_HTTP_MAX_TIMEOUT_MS 300000LL
#define KU_HTTP_MAX_HEADER_BYTES 65536LL
#define KU_HTTP_MAX_BODY_BYTES 16777216LL
#define KU_HTTP_MAX_BUFFER_BYTES 1073741824LL
#define KU_HTTP_MAX_CONNECTIONS 32768LL
#define KU_HTTP_MAX_ACTIVE_REQUESTS 16384LL
#define KU_HTTP_MAX_PENDING_REQUESTS 8192LL
#define KU_HTTP_READ_CHUNK 8192
static long ku_http_env_limit(const char*,long,long);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
static void ku_http_task_driver_timing(void*,uint64_t[10]);
static uint32_t ku_http_task_fixed_reserve(void*,size_t);
static uint32_t ku_http_task_fixed_refund(void*,size_t);
static void ku_http_task_driver_budget(void*,uint64_t[9]);
#if defined(KU_HTTP_HAS_TASK_NET)
static size_t ku_http_task_net_capacity(void*);
#endif
#endif

/* Private platform boundary. HTTP parsing/routing above the socket calls is
   byte-for-byte shared by Windows, Linux and macOS. */
#if defined(_WIN32)
typedef SOCKET KuHttpSocket;
#define KU_HTTP_INVALID_SOCKET INVALID_SOCKET
typedef volatile LONG KuHttpAtomicCounter;
static void ku_http_atomic_store(KuHttpAtomicCounter* counter, long value) { InterlockedExchange(counter, value); }
static long ku_http_atomic_increment(KuHttpAtomicCounter* counter) { return (long)InterlockedIncrement(counter); }
static void ku_http_atomic_decrement(KuHttpAtomicCounter* counter) { (void)InterlockedDecrement(counter); }
#else
typedef int KuHttpSocket;
#define KU_HTTP_INVALID_SOCKET (-1)
typedef _Atomic long KuHttpAtomicCounter;
static void ku_http_atomic_store(KuHttpAtomicCounter* counter, long value) { atomic_store_explicit(counter, value, memory_order_relaxed); }
static long ku_http_atomic_increment(KuHttpAtomicCounter* counter) { return atomic_fetch_add_explicit(counter, 1, memory_order_acq_rel) + 1; }
static void ku_http_atomic_decrement(KuHttpAtomicCounter* counter) { (void)atomic_fetch_sub_explicit(counter, 1, memory_order_acq_rel); }
#endif

static unsigned long long ku_http_now_ms(void) { return __ku_handler_now_ms(); }
static void ku_http_socket_close(KuHttpSocket);
/* Linux may wait for an RCU grace period while growing a shared FD table,
   including inside nonblocking accept. Prepare the bounded HTTP connection
   capacity before starting owners, rather than doing that work on a request.
   This is opportunistic: it neither reserves open sockets nor limits other
   libraries/Task networking, which can still grow the process table later. */
static void ku_http_prepare_fd_table(KuHttpSocket listener, long long connections, int owners) {
#if defined(__linux__) && defined(F_DUPFD_CLOEXEC)
  int saved_errno=errno;
  DIR* descriptors=NULL;
  do {
    struct rlimit limit;
    if (listener<0 || connections<=0 || owners<=0 ||
        getrlimit(RLIMIT_NOFILE,&limit) || !limit.rlim_cur) break;
    uint64_t ceiling=(uint64_t)INT_MAX;
    if (limit.rlim_cur!=RLIM_INFINITY && limit.rlim_cur-1<ceiling)
      ceiling=(uint64_t)(limit.rlim_cur-1);
    uint64_t highest=(uint64_t)listener;
    descriptors=opendir("/proc/self/fd");
    if (!descriptors) break;
    struct dirent* entry;
    errno=0;
    while ((entry=readdir(descriptors))!=NULL) {
      const unsigned char* name=(const unsigned char*)entry->d_name;
      uint64_t fd=0;
      int numeric=*name!=0;
      for (;*name && numeric;name++) {
        if (*name<'0' || *name>'9' || fd>((uint64_t)INT_MAX-(*name-'0'))/10u)
          numeric=0;
        else fd=fd*10u+(*name-'0');
      }
      if (numeric && fd>highest) highest=fd;
    }
    int read_error=errno;
    closedir(descriptors); descriptors=NULL;
    if (read_error || highest>=ceiling) break;
    /* Public limits bound these operands; clamp before adding nevertheless. */
    uint64_t room=ceiling-highest;
    uint64_t requested=(uint64_t)connections;
    uint64_t internal=(uint64_t)owners*8u+64u;
    uint64_t extra=requested>room?room:requested;
    extra=internal>room-extra?room:extra+internal;
    int duplicate=fcntl(listener,F_DUPFD_CLOEXEC,(int)(highest+extra));
    if (duplicate>=0) ku_http_socket_close(duplicate);
  } while (0);
  if (descriptors) closedir(descriptors);
  errno=saved_errno;
#else
  (void)listener; (void)connections; (void)owners;
#endif
}
/* CPU consumed by the calling event-owner OS thread only. This excludes
   coordinator/background workers and elapsed time spent sleeping or ready for
   CPU. Read at a bounded frequency, not at each Task dispatch. */
static int ku_http_thread_cpu_ns(uint64_t* output) {
#if defined(_WIN32)
  FILETIME created, exited, kernel, user;
  if (!GetThreadTimes(GetCurrentThread(),&created,&exited,&kernel,&user)) return 0;
  ULARGE_INTEGER system_ticks, user_ticks;
  system_ticks.LowPart=kernel.dwLowDateTime; system_ticks.HighPart=kernel.dwHighDateTime;
  user_ticks.LowPart=user.dwLowDateTime; user_ticks.HighPart=user.dwHighDateTime;
  if (system_ticks.QuadPart>UINT64_MAX-user_ticks.QuadPart) return 0;
  uint64_t ticks=system_ticks.QuadPart+user_ticks.QuadPart;
  if (ticks>UINT64_MAX/100u) return 0;
  *output=ticks*100u;
#else
  struct timespec value;
  if (clock_gettime(CLOCK_THREAD_CPUTIME_ID,&value) || value.tv_sec<0
      || value.tv_nsec<0 || value.tv_nsec>=1000000000L) return 0;
  uint64_t seconds=(uint64_t)value.tv_sec, fraction=(uint64_t)value.tv_nsec;
  if (seconds>UINT64_MAX/UINT64_C(1000000000)) return 0;
  uint64_t whole=seconds*UINT64_C(1000000000);
  if (fraction>UINT64_MAX-whole) return 0;
  *output=whole+fraction;
#endif
  return 1;
}

static int ku_http_socket_last_error(void) {
#if defined(_WIN32)
  return WSAGetLastError();
#else
  return errno;
#endif
}

static int ku_http_socket_error_interrupted(int error) {
#if defined(_WIN32)
  return error == WSAEINTR;
#else
  return error == EINTR;
#endif
}

/* accept(2) may surface already-aborted connections and pending network errors
   even though the listening socket itself is still healthy. A remote peer can
   trigger these, so they must never consume a stop-the-server retry budget. */
static int ku_http_socket_error_accept_peer_transient(int error) {
#if defined(_WIN32)
  return error == WSAEINTR || error == WSAECONNRESET || error == WSAECONNABORTED;
#else
  if (error == EINTR || error == ECONNABORTED) return 1;
#ifdef EPROTO
  if (error == EPROTO) return 1;
#endif
#ifdef ENETDOWN
  if (error == ENETDOWN) return 1;
#endif
#ifdef ENOPROTOOPT
  if (error == ENOPROTOOPT) return 1;
#endif
#ifdef EHOSTDOWN
  if (error == EHOSTDOWN) return 1;
#endif
#ifdef ENONET
  if (error == ENONET) return 1;
#endif
#ifdef EHOSTUNREACH
  if (error == EHOSTUNREACH) return 1;
#endif
#ifdef EOPNOTSUPP
  if (error == EOPNOTSUPP) return 1;
#endif
#ifdef ENETUNREACH
  if (error == ENETUNREACH) return 1;
#endif
  return 0;
#endif
}

/* Local descriptor/memory pressure is different: retry it with a longer bounded
   backoff, then fail rather than leave a resident process spinning forever. */
static int ku_http_socket_error_accept_resource_pressure(int error) {
#if defined(_WIN32)
  return error == WSAEMFILE || error == WSAENOBUFS;
#else
  return error == EMFILE || error == ENFILE || error == ENOBUFS || error == ENOMEM;
#endif
}

static int ku_http_net_init(void) {
#if defined(_WIN32)
  WSADATA data;
  return WSAStartup(MAKEWORD(2, 2), &data) == 0 ? 0 : -1;
#else
  return 0;
#endif
}

static void ku_http_net_cleanup(void) {
#if defined(_WIN32)
  WSACleanup();
#endif
}

static void ku_http_socket_close(KuHttpSocket socket_value) {
  if (socket_value == KU_HTTP_INVALID_SOCKET) return;
#if defined(_WIN32)
  closesocket(socket_value);
#else
  /* close(2) must not be retried after EINTR; the descriptor may already have
     been released and reused by another thread. */
  (void)close(socket_value);
#endif
}

static void ku_http_socket_shutdown_write(KuHttpSocket socket_value) {
#if defined(_WIN32)
  (void)shutdown(socket_value, SD_SEND);
#else
  (void)shutdown(socket_value, SHUT_WR);
#endif
}

static int ku_http_socket_suppress_sigpipe(KuHttpSocket socket_value) {
#if defined(__APPLE__)
  int enabled = 1;
  return setsockopt(socket_value, SOL_SOCKET, SO_NOSIGPIPE, &enabled, sizeof(enabled)) == 0 ? 0 : -1;
#else
  (void)socket_value;
  return 0;
#endif
}

static int ku_http_socket_send(KuHttpSocket socket_value, const char* data, size_t len) {
  size_t chunk = len > (size_t)INT_MAX ? (size_t)INT_MAX : len;
#if defined(_WIN32)
  return send(socket_value, data, (int)chunk, 0);
#elif defined(__APPLE__)
  ssize_t sent = send(socket_value, data, chunk, 0);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#elif defined(MSG_NOSIGNAL)
  ssize_t sent = send(socket_value, data, chunk, MSG_NOSIGNAL);
  return sent > (ssize_t)INT_MAX ? INT_MAX : (int)sent;
#else
#error "std.http POSIX transport requires MSG_NOSIGNAL or SO_NOSIGPIPE"
#endif
}

static int ku_http_socket_recv(KuHttpSocket socket_value, char* data, size_t len) {
  size_t chunk = len > (size_t)INT_MAX ? (size_t)INT_MAX : len;
#if defined(_WIN32)
  return recv(socket_value, data, (int)chunk, 0);
#else
  ssize_t received = recv(socket_value, data, chunk, 0);
  return received > (ssize_t)INT_MAX ? INT_MAX : (int)received;
#endif
}

/* A terminal handler registered at a trie node, one per HTTP method. Parameter
   names belong to the route/handler, not to shared trie nodes: two methods (or
   two routes with a shared param-shaped prefix) may legitimately use different
   names for the same captured segment positions. */
typedef struct { char* method; char** param_names; size_t nparams; void* invoke; void* env; int arity; int returns_result; } KuHttpHandler;
/* Stage 8b: a routing trie node. Each node holds static children (keyed by the
   literal path segment) and at most one `{param}` child. Matching prefers a
   static child over the param child at every segment (with backtracking), which
   mirrors the interpreter's exact-shape-before-param-scan lookup. */
typedef struct KuHttpNode {
  char* seg;                     /* static segment label for this node (NULL at root) */
  struct KuHttpNode** children;  /* static children */
  size_t nchild; size_t cchild;
  struct KuHttpNode* param;      /* single `{param}` child, or NULL */
  KuHttpHandler* handlers;       /* terminal handlers, keyed by method */
  size_t nh; size_t ch;
} KuHttpNode;
/* Stage 8d/8e: the route trie plus the limits read from the `http.server({...})`
   config object (or assigned field-by-field). Defaults mirror the interpreter's
   HttpServerRuntimeLimits (src/stdlib/http.rs:16-25).

   These are `long long`, not `long`: the interpreter stores every limit as i64,
   and `long` is 32-bit on Windows LLP64, which would silently truncate a limit
   above 2^31 (e.g. `max_body_bytes: 3_000_000_000` wrapping negative and
   disabling the 413 check). */
typedef struct KuHttpBufferBudget KuHttpBufferBudget;
struct KuHttpServer {
  KuHttpNode* root;
  int has_native_task_route, has_sync_route;
  KuHttpBufferBudget* buffer_budget;
  long long max_buffer_bytes;
  long long max_connections;
  long long max_active_requests;
  long long max_pending_requests;
  long long handler_timeout_ms;
  long long max_body_bytes;
  long long max_header_bytes;
  long long read_header_timeout_ms;
  long long read_body_timeout_ms;
  long long write_timeout_ms;
  long long idle_timeout_ms;
  uint32_t request_timeout_ms;
};
static void ku_http_normalize_path(const char* in, size_t in_len, char* out, size_t out_cap) {
  size_t oi = 0; size_t i = 0;
  if (out_cap == 0) return;
  out[oi++] = '/';
  while (i < in_len) {
    while (i < in_len && in[i] == '/') i++;
    size_t start = i;
    while (i < in_len && in[i] != '/') i++;
    if (i > start) {
      if (oi > 1 && oi + 1 < out_cap) out[oi++] = '/';
      size_t seg = i - start;
      if (oi + seg >= out_cap) seg = out_cap - 1 - oi;
      memcpy(out + oi, in + start, seg); oi += seg;
    }
  }
  out[oi] = '\0';
}
static KuHttpNode* ku_http_node_new(void) {
  KuHttpNode* n = (KuHttpNode*)calloc(1, sizeof(KuHttpNode));
  if (!n) { fprintf(stderr, "out of memory\n"); exit(1); }
  return n;
}
static KuHttpServer* ku_http_server_new(void) {
  KuHttpServer* s = (KuHttpServer*)calloc(1, sizeof(KuHttpServer));
  if (!s) { fprintf(stderr, "out of memory\n"); exit(1); }
  s->root = ku_http_node_new();
  s->max_connections = 1024;
  s->max_active_requests = 256;
  s->max_pending_requests = 1024;
  s->handler_timeout_ms = 15000;
  s->max_body_bytes = 1000000;
  s->max_buffer_bytes = 64LL * 1024 * 1024;
  s->max_header_bytes = 16 * 1024;
  s->read_header_timeout_ms = 5000;
  s->read_body_timeout_ms = 10000;
  s->write_timeout_ms = 10000;
  s->idle_timeout_ms = 5000;
  s->request_timeout_ms = (uint32_t)ku_http_env_limit("KU_HTTP_REQUEST_TIMEOUT_MS",30000,300000);
  return s;
}
/* Spell a KuValue tag the way the interpreter's Value::type_name() does, so a
   wrong-typed config field reports the same text on both runtimes. */
static const char* ku_http_value_type_name(KuValueTag tag) {
  switch (tag) {
    case KU_INT: return "int";
    case KU_FLOAT: return "float";
    case KU_BOOL: return "bool";
    case KU_STR: return "str";
    case KU_OBJECT: return "object";
    case KU_ARRAY: return "array";
    case KU_FUNCTION: return "function";
    default: return "null";
  }
}
/* Read an int field from the `http.server({...})` config object. Mirrors the
   interpreter's optional_int (src/stdlib/http.rs:418) exactly: absent or null
   yields the default, a positive int yields that value, and a non-positive int
   or a wrong-typed value is a hard error carrying the interpreter's message.
   Silently defaulting instead (the previous behaviour) would let native accept
   configs the interpreter rejects. `http.server(...)` has no Result channel in
   native, so this reports and exits like the runtime's other faults.
   The config object is only READ here; it stays owned by the caller, which
   drops it after `http.server(...)` returns. */
static long long ku_http_cfg_int(KuObject* config, const char* key, long long dflt, long long maximum) {
  if (!config) return dflt;
  KuValue* v = ku_object_get(config, ku_string_static((const uint8_t*)key, strlen(key)));
  if (!v || v->tag == KU_NULL) return dflt;
  if (v->tag != KU_INT) {
    fprintf(stderr, "type error: http.request field '%s' must be int but got %s\n", key, ku_http_value_type_name(v->tag));
    exit(1);
  }
  if (v->as.i <= 0) {
    fprintf(stderr, "http config field '%s' must be a positive int\n", key);
    exit(1);
  }
  if ((long long)v->as.i > maximum) {
    fprintf(stderr, "http config field '%s' must be at most %lld\n", key, maximum);
    exit(1);
  }
  return (long long)v->as.i;
}
static void ku_http_validate_server(KuHttpServer* s) {
  struct KuHttpLimit { const char* name; long long value; long long maximum; } limits[] = {
    { "max_connections", s->max_connections, KU_HTTP_MAX_CONNECTIONS },
    { "max_active_requests", s->max_active_requests, KU_HTTP_MAX_ACTIVE_REQUESTS },
    { "max_pending_requests", s->max_pending_requests, KU_HTTP_MAX_PENDING_REQUESTS },
    { "handler_timeout_ms", s->handler_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "max_body_bytes", s->max_body_bytes, KU_HTTP_MAX_BODY_BYTES },
    { "max_buffer_bytes", s->max_buffer_bytes, KU_HTTP_MAX_BUFFER_BYTES },
    { "max_header_bytes", s->max_header_bytes, KU_HTTP_MAX_HEADER_BYTES },
    { "read_header_timeout_ms", s->read_header_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "read_body_timeout_ms", s->read_body_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "write_timeout_ms", s->write_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS },
    { "idle_timeout_ms", s->idle_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS }
  };
  for (size_t i = 0; i < sizeof(limits) / sizeof(limits[0]); i++) {
    if (limits[i].value <= 0) {
      fprintf(stderr, "http config field '%s' must be a positive int\n", limits[i].name); exit(1);
    }
    if (limits[i].value > limits[i].maximum) {
      fprintf(stderr, "http config field '%s' must be at most %lld\n", limits[i].name, limits[i].maximum); exit(1);
    }
  }
}
static int ku_http_server_config_key_allowed(KuString key) {
  static const char* allowed[] = {
    "read_header_timeout_ms", "read_body_timeout_ms", "write_timeout_ms",
    "idle_timeout_ms", "handler_timeout_ms", "max_body_bytes",
    "max_header_bytes", "max_connections", "max_active_requests",
    "max_pending_requests", "max_buffer_bytes"
  };
  static const size_t lengths[] = { 22, 20, 16, 15, 18, 14, 16, 15, 19, 20, 16 };
  for (size_t i = 0; i < sizeof(allowed) / sizeof(allowed[0]); i++) {
    if (ku_string_equal(key, ku_string_static((const uint8_t*)allowed[i], lengths[i]))) return 1;
  }
  return 0;
}
static void ku_http_validate_server_config_keys(KuObject* config) {
  if (!config) return;
  for (size_t i = 0; i < config->cap; i++) {
    KuEntry* entry = &config->entries[i];
    if (!entry->used || ku_http_server_config_key_allowed(entry->key)) continue;
    fprintf(stderr, "unknown http config field '");
    if (entry->key.len && entry->key.ptr) fwrite(entry->key.ptr, 1, entry->key.len, stderr);
    fprintf(stderr, "'\n");
    exit(1);
  }
}
/* `http.server(config)` / `http.service(config)`: start from the defaults, then
   override each admission-control limit present in the config object. */
static KuHttpServer* ku_http_server_new_cfg(KuObject* config) {
  ku_http_validate_server_config_keys(config);
  KuHttpServer* s = ku_http_server_new();
  s->max_connections = ku_http_cfg_int(config, "max_connections", s->max_connections, KU_HTTP_MAX_CONNECTIONS);
  s->max_active_requests = ku_http_cfg_int(config, "max_active_requests", s->max_active_requests, KU_HTTP_MAX_ACTIVE_REQUESTS);
  s->max_pending_requests = ku_http_cfg_int(config, "max_pending_requests", s->max_pending_requests, KU_HTTP_MAX_PENDING_REQUESTS);
  s->handler_timeout_ms = ku_http_cfg_int(config, "handler_timeout_ms", s->handler_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->max_body_bytes = ku_http_cfg_int(config, "max_body_bytes", s->max_body_bytes, KU_HTTP_MAX_BODY_BYTES);
  s->max_buffer_bytes = ku_http_cfg_int(config, "max_buffer_bytes", s->max_buffer_bytes, KU_HTTP_MAX_BUFFER_BYTES);
  s->max_header_bytes = ku_http_cfg_int(config, "max_header_bytes", s->max_header_bytes, KU_HTTP_MAX_HEADER_BYTES);
  s->read_header_timeout_ms = ku_http_cfg_int(config, "read_header_timeout_ms", s->read_header_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->read_body_timeout_ms = ku_http_cfg_int(config, "read_body_timeout_ms", s->read_body_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->write_timeout_ms = ku_http_cfg_int(config, "write_timeout_ms", s->write_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  s->idle_timeout_ms = ku_http_cfg_int(config, "idle_timeout_ms", s->idle_timeout_ms, KU_HTTP_MAX_TIMEOUT_MS);
  ku_http_validate_server(s);
  return s;
}
static KuHttpNode* ku_http_node_child(KuHttpNode* node, const char* seg, size_t seg_len) {
  for (size_t i = 0; i < node->nchild; i++) {
    KuHttpNode* c = node->children[i];
    if (strlen(c->seg) == seg_len && memcmp(c->seg, seg, seg_len) == 0) return c;
  }
  return NULL;
}
static KuHttpNode* ku_http_node_add_static(KuHttpNode* node, const char* seg, size_t seg_len) {
  KuHttpNode* existing = ku_http_node_child(node, seg, seg_len);
  if (existing) return existing;
  if (node->nchild + 1 > node->cchild) {
    size_t nc = node->cchild ? node->cchild * 2 : 4;
    node->children = (KuHttpNode**)realloc(node->children, nc * sizeof(KuHttpNode*));
    if (!node->children) { fprintf(stderr, "out of memory\n"); exit(1); }
    node->cchild = nc;
  }
  KuHttpNode* child = ku_http_node_new();
  child->seg = (char*)malloc(seg_len + 1);
  if (!child->seg) { fprintf(stderr, "out of memory\n"); exit(1); }
  memcpy(child->seg, seg, seg_len); child->seg[seg_len] = '\0';
  node->children[node->nchild++] = child;
  return child;
}
static KuHttpNode* ku_http_node_add_param(KuHttpNode* node) {
  if (!node->param) node->param = ku_http_node_new();
  return node->param;
}
static int ku_http_is_tchar(unsigned char c) {
  return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
         c == '!' || c == '#' || c == '$' || c == '%' || c == '&' || c == '\'' ||
         c == '*' || c == '+' || c == '-' || c == '.' || c == '^' || c == '_' ||
         c == '`' || c == '|' || c == '~';
}
static int ku_http_is_hex(unsigned char c) {
  return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'F') || (c >= 'a' && c <= 'f');
}
static int ku_http_is_uri_pchar(unsigned char c) {
  return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
         c == '-' || c == '.' || c == '_' || c == '~' || c == '!' || c == '$' ||
         c == '&' || c == '\'' || c == '(' || c == ')' || c == '*' || c == '+' ||
         c == ',' || c == ';' || c == '=' || c == ':' || c == '@';
}
static int ku_http_valid_pchar_sequence(const uint8_t* ptr, size_t len) {
  size_t i = 0;
  while (i < len) {
    if (ptr[i] == '%') {
      if (i + 2 >= len || !ku_http_is_hex(ptr[i + 1]) || !ku_http_is_hex(ptr[i + 2])) return 0;
      i += 3;
    } else if (ku_http_is_uri_pchar(ptr[i])) {
      i++;
    } else {
      return 0;
    }
  }
  return 1;
}
static int ku_http_utf8_valid(const uint8_t* data, size_t len) {
  if (len != 0 && !data) return 0;
  size_t i = 0;
  while (i < len) {
    while (len - i >= sizeof(uint64_t)) {
      uint64_t word;
      memcpy(&word, data + i, sizeof(word));
      if (word & UINT64_C(0x8080808080808080)) break;
      i += sizeof(word);
    }
    if (i == len) break;
    uint8_t c = data[i];
    if (c <= 0x7f) { i++; continue; }
    if (c >= 0xc2 && c <= 0xdf) {
      if (i + 1 >= len || (data[i + 1] & 0xc0) != 0x80) return 0;
      i += 2; continue;
    }
    if (c == 0xe0) {
      if (i + 2 >= len || data[i + 1] < 0xa0 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if ((c >= 0xe1 && c <= 0xec) || (c >= 0xee && c <= 0xef)) {
      if (i + 2 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xed) {
      if (i + 2 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x9f || (data[i + 2] & 0xc0) != 0x80) return 0;
      i += 3; continue;
    }
    if (c == 0xf0) {
      if (i + 3 >= len || data[i + 1] < 0x90 || data[i + 1] > 0xbf || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c >= 0xf1 && c <= 0xf3) {
      if (i + 3 >= len || (data[i + 1] & 0xc0) != 0x80 || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    if (c == 0xf4) {
      if (i + 3 >= len || data[i + 1] < 0x80 || data[i + 1] > 0x8f || (data[i + 2] & 0xc0) != 0x80 || (data[i + 3] & 0xc0) != 0x80) return 0;
      i += 4; continue;
    }
    return 0;
  }
  return 1;
}
/* RFC 9112 origin-form with RFC 3986 pchar/query characters. This rejects
   whitespace, controls, backslashes, fragments and malformed percent escapes
   before any C-string operation or routing decision. */
static int ku_http_valid_origin_form(const uint8_t* ptr, size_t len) {
  if (!ptr || len == 0 || ptr[0] != '/') return 0;
  int in_query = 0;
  size_t i = 0;
  while (i < len) {
    uint8_t c = ptr[i];
    if (c == '?' && !in_query) { in_query = 1; i++; continue; }
    if (c == '/' || (in_query && c == '?')) { i++; continue; }
    if (c == '%') {
      if (i + 2 >= len || !ku_http_is_hex(ptr[i + 1]) || !ku_http_is_hex(ptr[i + 2])) return 0;
      i += 3;
      continue;
    }
    if (!ku_http_is_uri_pchar(c)) return 0;
    i++;
  }
  return 1;
}
static int ku_http_valid_route_param(const uint8_t* ptr, size_t len) {
  if (len == 0 || !((ptr[0] >= 'A' && ptr[0] <= 'Z') ||
                    (ptr[0] >= 'a' && ptr[0] <= 'z') || ptr[0] == '_')) return 0;
  for (size_t i = 1; i < len; i++) {
    uint8_t c = ptr[i];
    if (!((c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') ||
          (c >= 'a' && c <= 'z') || c == '_')) return 0;
  }
  return 1;
}
static int ku_http_valid_route_path(const uint8_t* ptr, size_t len, size_t* segment_count) {
  if (!ptr || len == 0 || ptr[0] != '/') return 0;
  size_t count = 0, i = 0;
  while (i < len) {
    while (i < len && ptr[i] == '/') i++;
    size_t start = i;
    while (i < len && ptr[i] != '/') i++;
    size_t seg_len = i - start;
    if (seg_len == 0) continue;
    count++;
    const uint8_t* seg = ptr + start;
    if (seg[0] == '{' || seg[seg_len - 1] == '}') {
      if (seg_len < 3 || seg[0] != '{' || seg[seg_len - 1] != '}' ||
          !ku_http_valid_route_param(seg + 1, seg_len - 2)) return 0;
    } else {
      for (size_t k = 0; k < seg_len; k++) if (seg[k] == ':') return 0;
      if (!ku_http_valid_pchar_sequence(seg, seg_len)) return 0;
    }
  }
  if (segment_count) *segment_count = count;
  return 1;
}
static KuHttpServer* ku_http_server_add_route(KuHttpServer* s, KuString method, KuString path, void* invoke, void* env, int arity, int returns_result) {
  if (method.len == 0 || method.len > 32) {
    fprintf(stderr, "http route method must be a valid HTTP token of at most 32 bytes\n"); exit(1);
  }
  for (size_t i = 0; i < method.len; i++) if (!ku_http_is_tchar(((const uint8_t*)method.ptr)[i])) {
    fprintf(stderr, "http route method must be a valid HTTP token of at most 32 bytes\n"); exit(1);
  }
  if (path.len > (size_t)KU_HTTP_MAX_TARGET) {
    fprintf(stderr, "http route path must be at most %d bytes\n", KU_HTTP_MAX_TARGET); exit(1);
  }
  size_t route_segments = 0;
  if (!ku_http_valid_route_path((const uint8_t*)path.ptr, path.len, &route_segments)) {
    fprintf(stderr, "invalid http route path\n"); exit(1);
  }
  if (route_segments > (size_t)KU_HTTP_MAX_SEGS) {
    fprintf(stderr, "http route path must contain at most %d segments\n", KU_HTTP_MAX_SEGS); exit(1);
  }
  /* Normalize into a buffer sized to the validated path, never a fixed one. */
  size_t pcap = path.len + 2;
  char* pbuf = (char*)malloc(pcap);
  if (!pbuf) { fprintf(stderr, "out of memory\n"); exit(1); }
  ku_http_normalize_path((const char*)path.ptr, path.len, pbuf, pcap);
  size_t plen = strlen(pbuf);
  KuHttpNode* node = s->root;
  const char* route_param_names[KU_HTTP_MAX_SEGS];
  size_t route_param_name_lens[KU_HTTP_MAX_SEGS];
  size_t route_param_count = 0;
  size_t i = 0;
  while (i < plen) {
    while (i < plen && pbuf[i] == '/') i++;
    size_t start = i;
    while (i < plen && pbuf[i] != '/') i++;
    size_t seg_len = i - start;
    if (seg_len == 0) continue;
    const char* seg = pbuf + start;
    if (seg_len >= 2 && seg[0] == '{' && seg[seg_len - 1] == '}') {
      size_t param_len = seg_len - 2;
      for (size_t p = 0; p < route_param_count; p++) {
        if (route_param_name_lens[p] == param_len &&
            memcmp(route_param_names[p], seg + 1, param_len) == 0) {
          fputs("duplicate http route param '", stderr);
          fwrite(seg + 1, 1, param_len, stderr);
          fputs("'\n", stderr);
          exit(1);
        }
      }
      route_param_names[route_param_count] = seg + 1;
      route_param_name_lens[route_param_count] = param_len;
      route_param_count++;
      node = ku_http_node_add_param(node);
    } else {
      node = ku_http_node_add_static(node, seg, seg_len);
    }
  }
  for (size_t i = 0; i < node->nh; i++) {
    size_t registered_len = strlen(node->handlers[i].method);
    if (registered_len == method.len &&
        memcmp(node->handlers[i].method, method.ptr, method.len) == 0) {
      fprintf(stderr, "duplicate http route\n"); exit(1);
    }
  }
  if (node->nh + 1 > node->ch) {
    size_t nc = node->ch ? node->ch * 2 : 2;
    node->handlers = (KuHttpHandler*)realloc(node->handlers, nc * sizeof(KuHttpHandler));
    if (!node->handlers) { fprintf(stderr, "out of memory\n"); exit(1); }
    node->ch = nc;
  }
  KuHttpHandler* h = &node->handlers[node->nh++];
  h->method = ku_string_to_cstr(method);
  h->param_names = NULL;
  h->nparams = route_param_count;
  if (route_param_count > 0) {
    h->param_names = (char**)calloc(route_param_count, sizeof(char*));
    if (!h->param_names) { fprintf(stderr, "out of memory\n"); exit(1); }
    for (size_t p = 0; p < route_param_count; p++) {
      h->param_names[p] = (char*)malloc(route_param_name_lens[p] + 1);
      if (!h->param_names[p]) { fprintf(stderr, "out of memory\n"); exit(1); }
      memcpy(h->param_names[p], route_param_names[p], route_param_name_lens[p]);
      h->param_names[p][route_param_name_lens[p]] = '\0';
    }
  }
  /* The route table outlives the closure value passed by the generated caller.
     Retain its environment so scope cleanup cannot leave the handler dangling;
     ku_http_node_free releases this retained ownership exactly once. */
  if (env) ((KuEnvHeader*)env)->retain(env);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  void* factory = ku_http_async_factory(invoke);
  if (factory) { invoke = factory; returns_result = 2; }
#endif
  h->invoke = invoke; h->env = env; h->arity = arity; h->returns_result = returns_result;
  if (h->returns_result == 2) s->has_native_task_route = 1;
  else s->has_sync_route = 1;
  ku_string_drop(&method); ku_string_drop(&path);
  free(pbuf);
  return s;
}
static KuHttpHandler* ku_http_node_handler(KuHttpNode* node, const char* method) {
  for (size_t i = 0; i < node->nh; i++)
    if (strcmp(node->handlers[i].method, method) == 0) return &node->handlers[i];
  return NULL;
}
static void ku_http_node_free(KuHttpNode* node) {
  if (!node) return;
  for (size_t i = 0; i < node->nchild; i++) ku_http_node_free(node->children[i]);
  free(node->children);
  ku_http_node_free(node->param);
  free(node->seg);
  for (size_t i = 0; i < node->nh; i++) {
    free(node->handlers[i].method);
    for (size_t p = 0; p < node->handlers[i].nparams; p++) free(node->handlers[i].param_names[p]);
    free(node->handlers[i].param_names);
    if (node->handlers[i].env) ((KuEnvHeader*)node->handlers[i].env)->release(node->handlers[i].env);
  }
  free(node->handlers);
  free(node);
}
static void ku_http_server_free(KuHttpServer* s) {
  if (!s) return;
  ku_http_node_free(s->root);
  free(s);
}
static const char* ku_http_status_text(int64_t status) {
  switch (status) {
    case 200: return "OK"; case 201: return "Created"; case 202: return "Accepted"; case 204: return "No Content";
    case 301: return "Moved Permanently"; case 302: return "Found"; case 303: return "See Other"; case 304: return "Not Modified";
    case 307: return "Temporary Redirect"; case 308: return "Permanent Redirect";
    case 400: return "Bad Request"; case 401: return "Unauthorized"; case 403: return "Forbidden"; case 404: return "Not Found";
    case 405: return "Method Not Allowed"; case 408: return "Request Timeout"; case 413: return "Content Too Large";
    case 414: return "URI Too Long"; case 417: return "Expectation Failed";
    case 431: return "Request Header Fields Too Large"; case 500: return "Internal Server Error";
    case 501: return "Not Implemented"; case 502: return "Bad Gateway"; case 503: return "Service Unavailable"; case 504: return "Gateway Timeout";
    default: return "Unknown";
  }
}
static KuString ku_http_string_copy(const char* ptr, size_t len) {
  if (len == 0) return (KuString){0};
  uint8_t* data = (uint8_t*)malloc(len);
  if (!data) { fprintf(stderr, "out of memory\n"); exit(1); }
  memcpy(data, ptr, len);
  return (KuString){ data, len, len, KU_STRING_OWNED };
}
/* Cold failure witnesses are fixed storage. No error strings, request bodies,
 * pointers or credentials are copied into diagnostics. Result error names only
 * classify the transport decision; they do not assert an unforgeable origin. */
enum {
  KU_HTTP_ERROR_UNKNOWN, KU_HTTP_ERROR_HANDLER_RESULT, KU_HTTP_ERROR_HANDLER_500,
  KU_HTTP_ERROR_TASK_LIMIT, KU_HTTP_ERROR_TASK_CLOSED, KU_HTTP_ERROR_TASK_OOM,
  KU_HTTP_ERROR_DRIVER, KU_HTTP_ERROR_INVALID_RESPONSE, KU_HTTP_ERROR_ALLOCATION,
  KU_HTTP_ERROR_PROJECTION, KU_HTTP_ERROR_QUEUE, KU_HTTP_ERROR_DEADLINE,
  KU_HTTP_ERROR_HANDLER_EXIT, KU_HTTP_ERROR_PARSER_INVARIANT, KU_HTTP_ERROR_PEER_RESET, KU_HTTP_ERROR_COUNT
};
typedef struct KuHttpFailure {
  uint64_t task_slot,task_generation,function_id;
  uint32_t reason,driver_status,stage;
} KuHttpFailure;
static KU_THREAD_LOCAL KuHttpFailure ku_http_failure_current;
static int ku_http_error_name(KuString value,const char* text) {
  size_t n=strlen(text); return value.len==n && (!n || !memcmp(value.ptr,text,n));
}
static uint32_t ku_http_result_failure(KuError* error) {
  if(ku_http_error_name(error->domain,"task")) {
    if(ku_http_error_name(error->code,"too_many_tasks")) return KU_HTTP_ERROR_TASK_LIMIT;
    if(ku_http_error_name(error->code,"runtime_stopped")) return KU_HTTP_ERROR_TASK_CLOSED;
    if(ku_http_error_name(error->code,"out_of_memory") || ku_http_error_name(error->code,"memory_limit")) return KU_HTTP_ERROR_TASK_OOM;
  }
  return KU_HTTP_ERROR_HANDLER_RESULT;
}
static KuStruct___ku_http_response ku_http_response_from_result(KuResult_struct___ku_http_response r) {
  if (r.ok) { if(r.value.status==500) ku_http_failure_current.reason=KU_HTTP_ERROR_HANDLER_500; KuStruct___ku_http_response v = r.value; r.value = (KuStruct___ku_http_response){0}; return v; }
  ku_http_failure_current.reason=ku_http_result_failure(&r.error);
  KuStruct___ku_http_response v;
  v.status = 500;
  v.content_type = ku_string_static((const uint8_t*)"text/plain; charset=utf-8", 25);
  v.body = ku_string_static((const uint8_t*)"Internal Server Error", 21);
  v.location = (KuString){0};
  ku_error_drop(&r.error);
  return v;
}
static int ku_http_field_value_safe(const uint8_t* ptr, size_t len) {
  for (size_t i = 0; i < len; i++) {
    uint8_t c = ptr[i];
    if (c != '\t' && (c < 0x20 || c == 0x7f)) return 0;
  }
  return 1;
}
/* `req.params`/`req.query`/`req.headers` `.field` read: an owned KuString copy of
   the string value, or an empty string when the key is absent (the interpreter's
   StringMap `.field` yields str; absent-key aborts there but is never exercised
   by the routing tests, so native returns "" instead of crashing). */
static KuString ku_http_map_get(KuObject* o, KuString key) {
  KuValue* v = ku_object_get(o, key);
  if (v && v->tag == KU_STR) return ku_string_clone(v->as.s);
  return ku_string_static((const uint8_t*)"", 0);
}
/* `?k=v&k2=v2` -> KuObject{str->str}. Empty parts are skipped; a non-empty
   `=value` part keeps its empty key, and a key without `=` maps to "", matching
   the interpreter's split_path_query. */
static size_t ku_http_map_capacity(size_t fields) {
  size_t cap=8;
  while (fields+1 >= cap-cap/4) cap*=2;
  return cap;
}
static size_t ku_http_query_fields(const char* target,size_t path_len) {
  const char* at=target+path_len;
  if (*at!='?') return 0;
  at++; size_t fields=0;
  while (*at) {
    const char* end=at; while (*end && *end!='&') end++;
    if (end>at) fields++;
    at=*end ? end+1 : end;
  }
  return fields;
}
static KuObject* ku_http_parse_query(const char* target, size_t path_len) {
  KuObject* q = ku_object_new(ku_http_map_capacity(ku_http_query_fields(target,path_len)));
  const char* p = target + path_len;
  if (*p != '?') return q;
  p++;
  while (*p) {
    const char* amp = p; while (*amp && *amp != '&') amp++;
    const char* eq = p; while (eq < amp && *eq != '=') eq++;
    size_t klen = (size_t)(eq - p);
    const char* vstart; size_t vlen;
    if (eq < amp) { vstart = eq + 1; vlen = (size_t)(amp - (eq + 1)); }
    else { vstart = amp; vlen = 0; }
    if (amp > p) {
      KuString key = ku_http_string_copy(p, klen);
      KuString val = ku_http_string_copy(vstart, vlen);
      ku_object_set(q, key, ku_v_str(val));
    }
    if (*amp == '&') p = amp + 1; else break;
  }
  return q;
}
/* Request header lines -> KuObject{lowercased-name -> trimmed-value}, matching the
   interpreter (header names lowercased, names and values trimmed). */
static KuObject* ku_http_parse_headers(const char* buf, int header_end,size_t capacity) {
  KuObject* h = ku_object_new(capacity);
  int i = 0;
  while (i + 1 < header_end && !(buf[i] == '\r' && buf[i + 1] == '\n')) i++;
  if (i + 1 < header_end) i += 2; else return h;
  while (i < header_end) {
    int ls = i;
    while (i + 1 < header_end && !(buf[i] == '\r' && buf[i + 1] == '\n')) i++;
    int le = (i + 1 < header_end) ? i : header_end;
    if (i + 1 < header_end) i += 2; else i = header_end;
    if (le <= ls) continue;
    int colon = ls; while (colon < le && buf[colon] != ':') colon++;
    if (colon >= le) continue;
    int ns = ls, ne = colon;
    while (ns < ne && (buf[ns] == ' ' || buf[ns] == '\t')) ns++;
    while (ne > ns && (buf[ne - 1] == ' ' || buf[ne - 1] == '\t')) ne--;
    int vs = colon + 1, ve = le;
    while (vs < ve && (buf[vs] == ' ' || buf[vs] == '\t')) vs++;
    while (ve > vs && (buf[ve - 1] == ' ' || buf[ve - 1] == '\t')) ve--;
    size_t nlen = (size_t)(ne - ns);
    if (nlen == 0) continue;
    KuString key = ku_http_string_copy(buf + ns, nlen);
    for (size_t k = 0; k < key.len; k++) { uint8_t c = ((uint8_t*)key.ptr)[k]; if (c >= 'A' && c <= 'Z') ((uint8_t*)key.ptr)[k] = (uint8_t)(c + 32); }
    KuString val = ku_http_string_copy(buf + vs, (size_t)(ve - vs));
    ku_object_set(h, key, ku_v_str(val));
  }
  return h;
}
/* Trie match with static-before-param priority and backtracking. It records only
   captured segment VALUES while walking. Parameter NAMES are attached to the
   final method-specific handler and are applied after a handler is selected;
   shared trie nodes therefore cannot leak one route's names into another route.
   The static-first-with-backtracking walk reproduces the interpreter's
   exact-shape-then-param-scan result (e.g. `/user/me` beats `/user/{id}`). */
static KuHttpHandler* ku_http_trie_match(KuHttpNode* node, const char** segs, size_t* seglens, size_t nseg, size_t i, const char* method, const char** param_values, size_t* param_value_lens, size_t* param_count) {
  if (i == nseg) return ku_http_node_handler(node, method);
  KuHttpNode* sc = ku_http_node_child(node, segs[i], seglens[i]);
  if (sc) {
    size_t saved = *param_count;
    KuHttpHandler* r = ku_http_trie_match(sc, segs, seglens, nseg, i + 1, method, param_values, param_value_lens, param_count);
    if (r) return r;
    *param_count = saved;
  }
  if (node->param) {
    size_t saved = *param_count;
    if (saved >= KU_HTTP_MAX_SEGS) return NULL;
    param_values[saved] = segs[i];
    param_value_lens[saved] = seglens[i];
    *param_count = saved + 1;
    KuHttpHandler* r = ku_http_trie_match(node->param, segs, seglens, nseg, i + 1, method, param_values, param_value_lens, param_count);
    if (r) return r;
    *param_count = saved;
  }
  return NULL;
}
/* True when the path matches some route under any method (405-vs-404 decision). */
static int ku_http_trie_path_exists(KuHttpNode* node, const char** segs, size_t* seglens, size_t nseg, size_t i) {
  if (i == nseg) return node->nh > 0 ? 1 : 0;
  KuHttpNode* sc = ku_http_node_child(node, segs[i], seglens[i]);
  if (sc && ku_http_trie_path_exists(sc, segs, seglens, nseg, i + 1)) return 1;
  if (node->param && ku_http_trie_path_exists(node->param, segs, seglens, nseg, i + 1)) return 1;
  return 0;
}
static int ku_http_name_eq(const char* ptr, size_t len, const char* expected) {
  size_t n = strlen(expected);
  if (len != n) return 0;
  for (size_t i = 0; i < len; i++) {
    unsigned char c = (unsigned char)ptr[i];
    if (c >= 'A' && c <= 'Z') c = (unsigned char)(c + 32);
    if (c != (unsigned char)expected[i]) return 0;
  }
  return 1;
}
static unsigned long long ku_http_deadline_ms(uint32_t timeout_ms) {
  unsigned long long now = ku_http_now_ms();
  if (~0ULL - now < (unsigned long long)timeout_ms) return ~0ULL;
  return now + (unsigned long long)timeout_ms;
}
/* A request owns this compact selection together with its target bytes. Route
 * callable/name allocations are immutable until server teardown; copying the
 * callable fields avoids borrowing a reallocatable handler-array element.
 * No pointer below refers to the parser's header buffer or stack. */
typedef struct KuHttpRouteToken {
  void* invoke; void* env; char** param_names;
  uint32_t status;
  uint16_t param_count;
  uint8_t arity,returns_result,fields_mask;
  uint16_t control_offset,control_length,rid_offset,rid_length;
  uint8_t rid_truncated;
  uint16_t ranges[];
} KuHttpRouteToken;
static int ku_http_select_route(KuHttpServer* server,const char* method,
    const char* target,size_t path_len,KuHttpHandler** route,
    const char** values,size_t* lengths,size_t* count) {
  *route=NULL;*count=0;
  if(!server->root)return 404;
  const char* segments[KU_HTTP_MAX_SEGS];size_t sizes[KU_HTTP_MAX_SEGS],n=0;
  for(size_t i=0;i<path_len;) {
    while(i<path_len && target[i]=='/')i++;
    size_t start=i;while(i<path_len && target[i]!='/')i++;
    if(i==start)continue;
    if(n==KU_HTTP_MAX_SEGS)return 414;
    segments[n]=target+start;sizes[n++]=i-start;
  }
  *count=0;
  *route=ku_http_trie_match(server->root,segments,sizes,n,0,method,values,lengths,count);
  if(*route)return *count==(*route)->nparams?200:500;
  return ku_http_trie_path_exists(server->root,segments,sizes,n,0)?405:404;
}
/* Shared-I/O request storage. No field borrows a connection buffer or worker
   stack. Once queued, the job owns this input through actual handler return. */
static int ku_http_buffer_reserve(KuHttpBufferBudget* budget, size_t bytes);
static void ku_http_buffer_refund(KuHttpBufferBudget* budget, size_t bytes);
typedef struct {
  KuHttpBufferBudget* budget; size_t body_charge, metadata_charge;
  char method[33];
  char* target;
  size_t path_len;
  KuObject* headers;
  char* body;
  size_t body_len;
  int keep_alive;
  KuHttpRouteToken* route;
} KuHttpInput;
static void ku_http_input_drop(KuHttpInput* input) {
  if (!input) return;
  if (input->headers) ku_object_drop(input->headers);
  free(input->route ? (void*)input->route : (void*)input->target);
  free(input->body);
  ku_http_buffer_refund(input->budget, input->body_charge);
  ku_http_buffer_refund(input->budget, input->metadata_charge);
  *input = (KuHttpInput){0};
}
static KuString ku_http_input_take_body(KuHttpInput* input) {
  KuString body = { (uint8_t*)input->body, input->body_len, input->body_len, KU_STRING_OWNED };
  input->body = NULL;
  /* Keep body_charge/body_len until input_drop. The unchanged projection
     reservation covers this owner after that refund, through response admission. */
  return body;
}
static KuStruct___ku_http_response ku_http_status_response(int64_t status, const char* message) {
  KuStruct___ku_http_response response = {0};
  response.status = status;
  response.content_type = ku_string_static((const uint8_t*)"text/plain; charset=utf-8", 25);
  response.body = ku_string_static((const uint8_t*)message, strlen(message));
  return response;
}
static KuString ku_http_input_control_token(const KuHttpInput* input) {
  if(input->route)return (KuString){(uint8_t*)input->target+input->route->control_offset,
      input->route->control_length,0,KU_STRING_STATIC};
  KuValue* value=input->headers?ku_object_get(input->headers,
      ku_string_static((const uint8_t*)"x-ku-control-token",18)):NULL;
  return value && value->tag==KU_STR?value->as.s:(KuString){0};
}
static int ku_http_input_select(KuHttpServer* server,const KuHttpInput* input,
    KuHttpHandler* selected,const char** values,size_t* lengths,size_t* count) {
  *selected=(KuHttpHandler){0};*count=0;
  if(input->route) {
    const KuHttpRouteToken* token=input->route;
    if(token->status!=200u)return (int)token->status;
    if(token->param_count>KU_HTTP_MAX_SEGS || token->arity>1u || token->returns_result>2u)return 500;
    selected->invoke=token->invoke;selected->env=token->env;selected->param_names=token->param_names;
    selected->nparams=token->param_count;selected->arity=token->arity;
    selected->returns_result=token->returns_result;*count=token->param_count;
    for(size_t p=0;p<*count;p++) {
      size_t offset=token->ranges[p*2u],length=token->ranges[p*2u+1u];
      if(offset>input->path_len || length>input->path_len-offset)return 500;
      values[p]=input->target+offset;lengths[p]=length;
    }
    return 200;
  }
  /* Compatibility for native consumers constructing an input directly. */
  KuHttpHandler* route=NULL;
  int status=ku_http_select_route(server,input->method,input->target,input->path_len,
      &route,values,lengths,count);
  if(route)*selected=*route;
  return status;
}
/* Pure header interpretation, shared by every socket readiness turn. Framing
   and its byte-limit/CRLF precedence have already been checked by the owner.
   The caller drops input on every error, including partial construction. */
static int ku_http_parse_complete_header(KuHttpServer* server, const char* hdr, size_t hlen, KuHttpInput* input) {
  long long max_body = server->max_body_bytes >= 0 ? server->max_body_bytes : 1000000;
  if (hlen == 0) { return 400; }
  /* The interpreter decodes the complete header block as UTF-8 before parsing.
     Enforce the same boundary here so invalid field values never become KuString
     instances or reach a handler. The body prefix is validated separately after
     Content-Length is applied. */
  if (!ku_http_utf8_valid((const uint8_t*)hdr, hlen)) { return 400; }
  size_t fl_end = hlen;
  for (size_t i = 0; i + 1 < hlen; i++) { if (hdr[i] == '\r' && hdr[i + 1] == '\n') { fl_end = i; break; } }
  size_t sp1 = fl_end, sp2 = fl_end;
  for (size_t i = 0; i < fl_end; i++) if (hdr[i] == ' ') { sp1 = i; break; }
  if (sp1 < fl_end) for (size_t i = sp1 + 1; i < fl_end; i++) if (hdr[i] == ' ') { sp2 = i; break; }
  if (sp1 == 0 || sp1 >= fl_end || sp2 <= sp1 + 1 || sp2 + 1 >= fl_end) { return 400; }
  for (size_t i = sp2 + 1; i < fl_end; i++) if (hdr[i] == ' ') { return 400; }
  size_t method_len = sp1;
  size_t target_start = sp1 + 1, target_len = sp2 - target_start;
  size_t version_start = sp2 + 1, version_len = fl_end - version_start;
  if (method_len > 32) { return 400; }
  for (size_t i = 0; i < method_len; i++) if (!ku_http_is_tchar((unsigned char)hdr[i])) { return 400; }
  if (version_len != 8 || memcmp(hdr + version_start, "HTTP/1.1", 8) != 0) { return 400; }
  if (!ku_http_valid_origin_form((const uint8_t*)hdr + target_start, target_len)) {
    return 400;
  }
  int host_count = 0, content_length_count = 0;
  unsigned long long content_length = 0;
  int content_length_overflow = 0;
  size_t header_fields=0;
  size_t control_start=0,control_length=0,rid_start=0,rid_length=0;
  input->keep_alive = 1;
  { size_t i = (fl_end < hlen) ? fl_end + 2 : hlen;
    while (i < hlen) {
      size_t ls = i;
      size_t le = hlen;
      for (size_t k = i; k + 1 < hlen; k++) { if (hdr[k] == '\r' && hdr[k + 1] == '\n') { le = k; break; } }
      if (le > ls) {
        header_fields++;
        if (hdr[ls] == ' ' || hdr[ls] == '\t') { return 400; }
        size_t colon = le;
        for (size_t k = ls; k < le; k++) if (hdr[k] == ':') { colon = k; break; }
        if (colon == ls || colon >= le) { return 400; }
        for (size_t k = ls; k < colon; k++) if (!ku_http_is_tchar((unsigned char)hdr[k])) { return 400; }
        size_t vs = colon + 1, ve = le;
        for (size_t k = vs; k < ve; k++) {
          unsigned char c = (unsigned char)hdr[k];
          if (c != '\t' && (c < 0x20 || c == 0x7f)) { return 400; }
        }
        while (vs < ve && (hdr[vs] == ' ' || hdr[vs] == '\t')) vs++;
        while (ve > vs && (hdr[ve - 1] == ' ' || hdr[ve - 1] == '\t')) ve--;
        if(ku_http_name_eq(hdr+ls,colon-ls,"x-ku-control-token")) {
          control_start=vs;control_length=ve-vs;
        }
        if(ku_http_name_eq(hdr+ls,colon-ls,"x-request-id")) {
          rid_start=vs;rid_length=ve-vs;
        }
        if (ku_http_name_eq(hdr + ls, colon - ls, "host")) {
          host_count++;
          if (host_count != 1 || vs == ve) { return 400; }
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "connection")) {
          /* Every duplicate field participates; a later keep-alive can never
             undo close. Match whole comma-separated, OWS-trimmed tokens. */
          for (size_t start = vs; start < ve;) {
            size_t end = start;
            while (end < ve && hdr[end] != ',') end++;
            size_t left = start, right = end;
            while (left < right && (hdr[left] == ' ' || hdr[left] == '\t')) left++;
            while (right > left && (hdr[right - 1] == ' ' || hdr[right - 1] == '\t')) right--;
            if (ku_http_name_eq(hdr + left, right - left, "close")) input->keep_alive = 0;
            start = end < ve ? end + 1 : ve;
          }
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "transfer-encoding")) {
          return 400;
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "expect")) {
          return 417;
        } else if (ku_http_name_eq(hdr + ls, colon - ls, "content-length")) {
          content_length_count++;
          if (content_length_count != 1 || vs == ve) { return 400; }
          for (size_t k = vs; k < ve; k++) {
            unsigned char byte = (unsigned char)hdr[k];
            if (byte < '0' || byte > '9') { return 400; }
            if (!content_length_overflow) {
              unsigned long long digit = (unsigned long long)(byte - '0');
              if (content_length > (~0ULL - digit) / 10ULL) content_length_overflow = 1;
              else content_length = content_length * 10ULL + digit;
            }
          }
        }
      }
      if (le >= hlen) break;
      i = le + 2;
    }
  }
  if (host_count != 1) { return 400; }
  memcpy(input->method, hdr, method_len); input->method[method_len] = '\0';
  /* The target is length-checked BEFORE it is copied and before any routing, so an
     over-long target can never be truncated into a shorter one that matches a real
     route, and never reaches a handler. */
  if (target_len > (size_t)KU_HTTP_MAX_TARGET) { return 414; }
  /* All header string allocations, including overwritten duplicate values,
     fit within the raw block length. Reserve before target/map construction;
     the pre-sized map header and buckets are included. No map growth or
     uncharged old/new rehash overlap occurs during header construction. */
  size_t path_len=0;
  while(path_len<target_len && hdr[target_start+path_len]!='?')path_len++;
  const char* param_values[KU_HTTP_MAX_SEGS];size_t param_lengths[KU_HTTP_MAX_SEGS],param_count=0;
  KuHttpHandler* selected=NULL;
  int route_status=ku_http_select_route(server,input->method,hdr+target_start,path_len,
      &selected,param_values,param_lengths,&param_count);
  unsigned int fields_mask=selected?ku_http_handler_request_fields(selected->invoke):63u;
  int public_headers=!server->root || (selected && selected->arity==1 && (fields_mask&1u));
  size_t header_capacity=public_headers?ku_http_map_capacity(header_fields):0;
  size_t control_bytes=control_length<=128u?control_length:0;
  size_t rid_bytes=rid_length<48u?rid_length:48u;
  size_t token_bytes=sizeof(KuHttpRouteToken)+param_count*2u*sizeof(uint16_t);
  size_t private_bytes=token_bytes+target_len+1u+control_bytes+rid_bytes;
  size_t metadata_bound = private_bytes+(public_headers?
      hlen+sizeof(KuObject)+header_capacity*sizeof(KuEntry):0);
  if (!ku_http_buffer_reserve(input->budget, metadata_bound)) return 503;
  input->metadata_charge = metadata_bound;
  KuHttpRouteToken* token=(KuHttpRouteToken*)malloc(private_bytes);
  if(!token)return 500;
  memset(token,0,sizeof(*token));input->route=token;
  token->status=(uint32_t)route_status;token->param_count=(uint16_t)param_count;
  token->fields_mask=(uint8_t)fields_mask;
  if(selected) {
    token->invoke=selected->invoke;token->env=selected->env;token->param_names=selected->param_names;
    token->arity=(uint8_t)selected->arity;token->returns_result=(uint8_t)selected->returns_result;
  }
  for(size_t p=0;p<param_count;p++) {
    token->ranges[2u*p]=(uint16_t)(param_values[p]-(hdr+target_start));
    token->ranges[2u*p+1u]=(uint16_t)param_lengths[p];
  }
  char* target=(char*)token+token_bytes;
  input->target = target;
  memcpy(target, hdr + target_start, target_len); target[target_len] = '\0';
  token->control_offset=(uint16_t)(target_len+1u);
  token->control_length=(uint16_t)(control_length<=128u?control_length:129u);
  if(control_bytes)memcpy(target+token->control_offset,hdr+control_start,control_bytes);
  token->rid_offset=(uint16_t)(target_len+1u+control_bytes);token->rid_length=(uint16_t)rid_bytes;
  token->rid_truncated=(uint8_t)(rid_length>rid_bytes);
  if(rid_bytes)memcpy(target+token->rid_offset,hdr+rid_start,rid_bytes);
  /* Header map (lowercased names, trimmed values) matching the interpreter. */
  KuObject* headers = public_headers?ku_http_parse_headers(hdr,(int)hlen,header_capacity):NULL;
  input->headers = headers;
  size_t metadata_bytes = private_bytes+(headers?sizeof(KuObject)+headers->cap*sizeof(KuEntry):0);
  for (size_t slot=0; headers && slot<headers->cap; slot++) {
    KuEntry* entry=&headers->entries[slot];
    if (!entry->used) continue;
    if (entry->key.storage==KU_STRING_OWNED) metadata_bytes+=entry->key.capacity;
    if (entry->value.tag==KU_STR && entry->value.as.s.storage==KU_STRING_OWNED)
      metadata_bytes+=entry->value.as.s.capacity;
  }
  if (metadata_bytes > input->metadata_charge) { return 500; }
  ku_http_buffer_refund(input->budget, input->metadata_charge-metadata_bytes);
  input->metadata_charge=metadata_bytes;
  /* The first validation pass already accumulated the strict decimal value.
     Report numeric overflow here, retaining the original precedence of field,
     target-length and metadata-admission errors before it. Do not hash the
     public header map and validate the same bytes a second time. */
  if (content_length_overflow) { return 400; }
  if (content_length > (unsigned long long)max_body) { return 413; }
  input->path_len = path_len;
  size_t request_segments = 0;
  { size_t i = 0;
    while (i < path_len) {
      while (i < path_len && target[i] == '/') i++;
      size_t start = i;
      while (i < path_len && target[i] != '/') i++;
      if (i > start && ++request_segments > (size_t)KU_HTTP_MAX_SEGS) {
        return 414;
      }
    }
  }
  if(route_status==414)return route_status;
  input->body_len = (size_t)content_length;
  if (input->body_len) {
    if (!ku_http_buffer_reserve(input->budget, input->body_len)) return 503;
    input->body_charge = input->body_len;
    input->body = (char*)malloc(input->body_len);
    if (!input->body) return 500; /* input_drop refunds after freeing the owner. */
  }
  return 0;
}
typedef struct KuHttpJob KuHttpJob;
/* A native consumer without a parser token keeps the complete public ABI.
   Only a compiler-proven synchronous thunk or Task factory carries a reduced
   mask. Validation and framed body ownership remain independent. */
static unsigned int ku_http_input_fields(const KuHttpInput* input) {
  return input->route?input->route->fields_mask:63u;
}
static size_t ku_http_projection_bytes(KuHttpInput* input,size_t parameters,size_t parameter_names) {
  unsigned int fields=ku_http_input_fields(input);
  size_t target=strlen(input->target),bytes=sizeof(KuStruct___ku_http_request);
  /* Bound every allocated map/string independently. Input/header metadata is
     already charged; body transfer retains this reservation across input drop. */
  if(fields&2u)bytes+=sizeof(KuObject)+ku_http_map_capacity(parameters)*sizeof(KuEntry)
      +parameter_names+input->path_len;
  if(fields&4u)bytes+=sizeof(KuObject)
      +ku_http_map_capacity(ku_http_query_fields(input->target,input->path_len))*sizeof(KuEntry)+target*2u;
  if(fields&8u)bytes+=strlen(input->method);
  if(fields&16u)bytes+=input->path_len;
  if(fields&32u)bytes+=input->body_len;
  return bytes;
}
static int ku_http_job_projection(KuHttpJob*,size_t);
static void ku_http_job_begin(KuHttpJob* job, unsigned long long timeout_ms);
/* Only a handler worker calls this function. All synchronous exit/deadline TLS
   remains on that same execution thread; socket ownership never enters it. */
static KuStruct___ku_http_response ku_http_execute_request(KuHttpServer* server, KuHttpInput* input, KuHttpJob* job) {
  const char* method = input->method;
  const char* target = input->target;
  size_t path_len = input->path_len;
  const char* param_values[KU_HTTP_MAX_SEGS];size_t param_value_lens[KU_HTTP_MAX_SEGS],param_count=0;
  KuHttpHandler selected={0};
  int route_status=ku_http_input_select(server,input,&selected,param_values,param_value_lens,&param_count);
  KuHttpHandler* route=route_status==200?&selected:NULL;
  if(route_status==414)return ku_http_status_response(414,"URI Too Long");
  if(route_status==500) {
    ku_http_failure_current.reason=KU_HTTP_ERROR_PARSER_INVARIANT;
    return ku_http_status_response(500,"Internal Server Error");
  }
  if (route) {
    KuStruct___ku_http_response resp = (KuStruct___ku_http_response){0};
    int handler_timed_out = 0;
    KuSyncExitSignal handler_signal = {0};
    __ku_sync_reset();
    if (route->arity == 1) {
      size_t parameter_names=0;
      for (size_t i=0;i<route->nparams;i++) parameter_names+=strlen(route->param_names[i]);
      if (!ku_http_job_projection(job,ku_http_projection_bytes(input,route->nparams,parameter_names)))
        return ku_http_status_response(503,"Service Unavailable");
      unsigned int fields=ku_http_input_fields(input);
      KuStruct___ku_http_request req = {0};
      if(fields&2u) {
        req.params = ku_object_new(ku_http_map_capacity(route->nparams));
        for (size_t p = 0; p < route->nparams; p++) {
          KuString key = ku_http_string_copy(route->param_names[p], strlen(route->param_names[p]));
          KuString val = ku_http_string_copy(param_values[p], param_value_lens[p]);
          ku_object_set(req.params, key, ku_v_str(val));
        }
      }
      if(fields&8u)req.method = ku_http_string_copy(method, strlen(method));
      if(fields&16u)req.path = ku_http_string_copy(target, path_len);
      if(fields&32u) { req.body = ku_http_input_take_body(input); }
      if(fields&4u)req.query = ku_http_parse_query(target, path_len);
      if(fields&1u) {req.headers = input->headers; input->headers = NULL;}
      if (route->returns_result) {
        ku_http_job_begin(job, (unsigned long long)server->handler_timeout_ms);
        KuResult_struct___ku_http_response rr = ((KuResult_struct___ku_http_response(*)(void*, KuStruct___ku_http_request))route->invoke)(route->env, req);
        handler_timed_out = __ku_sync_finish_request(&handler_signal);
        if (handler_timed_out || handler_signal.kind != KU_SYNC_EXIT_NONE) ku_result_drop_struct___ku_http_response(&rr);
        else resp = ku_http_response_from_result(rr);
      } else {
        ku_http_job_begin(job, (unsigned long long)server->handler_timeout_ms);
        resp = ((KuStruct___ku_http_response(*)(void*, KuStruct___ku_http_request))route->invoke)(route->env, req);
        handler_timed_out = __ku_sync_finish_request(&handler_signal);
      }
    } else {
      if (route->returns_result) {
        ku_http_job_begin(job, (unsigned long long)server->handler_timeout_ms);
        KuResult_struct___ku_http_response rr = ((KuResult_struct___ku_http_response(*)(void*))route->invoke)(route->env);
        handler_timed_out = __ku_sync_finish_request(&handler_signal);
        if (handler_timed_out || handler_signal.kind != KU_SYNC_EXIT_NONE) ku_result_drop_struct___ku_http_response(&rr);
        else resp = ku_http_response_from_result(rr);
      } else {
        ku_http_job_begin(job, (unsigned long long)server->handler_timeout_ms);
        resp = ((KuStruct___ku_http_response(*)(void*))route->invoke)(route->env);
        handler_timed_out = __ku_sync_finish_request(&handler_signal);
      }
    }
    if (handler_timed_out || handler_signal.kind != KU_SYNC_EXIT_NONE) {
      /* The execution worker owns the response until publication. A plain
         handler may complete after its original deadline; drop that response
         before selecting 504/500. Failed Result transport was dropped above. */
      if (!route->returns_result) ku_drop_struct___ku_http_response(&resp);
      ku_http_failure_current.reason=handler_timed_out ? KU_HTTP_ERROR_DEADLINE : KU_HTTP_ERROR_HANDLER_EXIT;
      return handler_timed_out
        ? ku_http_status_response(504, "Gateway Timeout")
        : ku_http_status_response(500, "Internal Server Error");
    } else {
      return resp;
    }
  } else {
    if (route_status==405) {
      return ku_http_status_response(405, "Method Not Allowed");
    } else {
      return ku_http_status_response(404, "Not Found");
    }
  }
}
static KuResult_null ku_http_listen_err(KuHttpServer* server, const char* message) {
  ku_http_server_free(server);
  return (KuResult_null){ false, 0, ku_error_make(ku_string_static((const uint8_t*)"http", 4), ku_string_static((const uint8_t*)"listen_failed", 13), ku_string_static((const uint8_t*)message, strlen(message))) };
}
/* C bounds stable/lazily touched connection records. A request lease is held
   from first-byte admission through wire completion AND physical job/Task
   retirement. Idle transports do not own A. Raw transports and existing
   request identities share one bounded P FIFO and the existing 503 owner. */
#define KU_HTTP_WORKER_CAP 64
#define KU_HTTP_ACCEPT_PEER_BACKOFF_CAP_MS 8
#define KU_HTTP_ACCEPT_RESOURCE_RETRY_CAP 8
#define KU_HTTP_ACCEPT_RESOURCE_BACKOFF_STEP_MS 10
static KuHttpAtomicCounter ku_http_active_conns;

#if defined(_WIN32)
typedef CRITICAL_SECTION KuHttpMutex;
typedef CONDITION_VARIABLE KuHttpCondition;
typedef WSAPOLLFD KuHttpPollFd;
#define KU_HTTP_POLL_READ POLLRDNORM
#define KU_HTTP_POLL_WRITE POLLWRNORM
static int ku_http_mutex_init(KuHttpMutex* mutex) { return InitializeCriticalSectionAndSpinCount(mutex, 0) != 0; }
static void ku_http_mutex_destroy(KuHttpMutex* mutex) { DeleteCriticalSection(mutex); }
static void ku_http_mutex_lock(KuHttpMutex* mutex) { EnterCriticalSection(mutex); }
static void ku_http_mutex_unlock(KuHttpMutex* mutex) { LeaveCriticalSection(mutex); }
static int ku_http_sync_init(KuHttpMutex* mutex, KuHttpCondition* condition) {
  if (!ku_http_mutex_init(mutex)) return 0;
  InitializeConditionVariable(condition);
  return 1;
}
static void ku_http_sync_destroy(KuHttpMutex* mutex, KuHttpCondition* condition) {
  (void)condition; ku_http_mutex_destroy(mutex);
}
static void ku_http_condition_wait(KuHttpCondition* condition, KuHttpMutex* mutex) {
  if (!SleepConditionVariableCS(condition, mutex, INFINITE)) {
    fputs("http queue condition wait failed\n", stderr); exit(1);
  }
}
static void ku_http_condition_signal(KuHttpCondition* condition) { WakeConditionVariable(condition); }
static void ku_http_condition_broadcast(KuHttpCondition* condition) { WakeAllConditionVariable(condition); }
static int ku_http_poll(KuHttpPollFd* items, size_t count, int timeout_ms) {
  return WSAPoll(items, (ULONG)count, timeout_ms);
}
static int ku_http_socket_nonblocking(KuHttpSocket socket_value) {
  u_long enabled = 1;
  return ioctlsocket(socket_value, FIONBIO, &enabled) == 0;
}
static int ku_http_socket_would_block(int error) { return error == WSAEWOULDBLOCK; }
#else
typedef pthread_mutex_t KuHttpMutex;
typedef pthread_cond_t KuHttpCondition;
typedef struct pollfd KuHttpPollFd;
#define KU_HTTP_POLL_READ POLLIN
#define KU_HTTP_POLL_WRITE POLLOUT
static int ku_http_mutex_init(KuHttpMutex* mutex) { return pthread_mutex_init(mutex, NULL) == 0; }
static void ku_http_mutex_destroy(KuHttpMutex* mutex) {
  if (pthread_mutex_destroy(mutex) != 0) { fputs("http mutex destroy failed\n", stderr); exit(1); }
}
static void ku_http_mutex_lock(KuHttpMutex* mutex) {
  if (pthread_mutex_lock(mutex) != 0) { fputs("http queue mutex lock failed\n", stderr); exit(1); }
}
static void ku_http_mutex_unlock(KuHttpMutex* mutex) {
  if (pthread_mutex_unlock(mutex) != 0) { fputs("http queue mutex unlock failed\n", stderr); exit(1); }
}
static int ku_http_sync_init(KuHttpMutex* mutex, KuHttpCondition* condition) {
  if (!ku_http_mutex_init(mutex)) return 0;
  if (pthread_cond_init(condition, NULL) != 0) { ku_http_mutex_destroy(mutex); return 0; }
  return 1;
}
static void ku_http_sync_destroy(KuHttpMutex* mutex, KuHttpCondition* condition) {
  if (pthread_cond_destroy(condition) != 0) { fputs("http condition destroy failed\n", stderr); exit(1); }
  ku_http_mutex_destroy(mutex);
}
static void ku_http_condition_wait(KuHttpCondition* condition, KuHttpMutex* mutex) {
  if (pthread_cond_wait(condition, mutex) != 0) {
    fputs("http queue condition wait failed\n", stderr); exit(1);
  }
}
static void ku_http_condition_signal(KuHttpCondition* condition) {
  if (pthread_cond_signal(condition) != 0) { fputs("http queue condition signal failed\n", stderr); exit(1); }
}
static void ku_http_condition_broadcast(KuHttpCondition* condition) {
  if (pthread_cond_broadcast(condition) != 0) { fputs("http queue condition broadcast failed\n", stderr); exit(1); }
}
static int ku_http_poll(KuHttpPollFd* items, size_t count, int timeout_ms) {
#if defined(__APPLE__)
  /* These private interests contain sockets only. Empty poll(0) calls dominate
     ready-Task turns on Darwin. Use select only to prove that this turn has no
     events; poll still supplies every positive/error result and all unsupported
     or blocking cases. No I/O turn
     or Task quantum is skipped, and no persistent kernel registration is kept. */
  if (timeout_ms == 0 && items && count) {
    fd_set reads, writes, exceptions;
    FD_ZERO(&reads); FD_ZERO(&writes); FD_ZERO(&exceptions);
    int nfds = 0, supported = 1;
    for (size_t i = 0; i < count; i++) {
      int fd = items[i].fd;
      short events = items[i].events;
      if (fd < 0) continue;
      if (fd >= FD_SETSIZE || !(events & (POLLIN | POLLOUT))
          || (events & ~(POLLIN | POLLOUT))) { supported = 0; break; }
      if (events & POLLIN) FD_SET(fd, &reads);
      if (events & POLLOUT) FD_SET(fd, &writes);
      FD_SET(fd, &exceptions);
      if (fd >= nfds) nfds = fd + 1;
    }
    if (supported) {
      int previous_errno = errno;
      struct timeval zero = {0, 0};
      int ready = select(nfds, &reads, &writes, &exceptions, &zero);
      errno = previous_errno;
      if (ready == 0) {
        for (size_t i = 0; i < count; i++) items[i].revents = 0;
        return 0;
      }
    }
  }
#endif
  return poll(items, (nfds_t)count, timeout_ms);
}
static int ku_http_socket_nonblocking(KuHttpSocket socket_value) {
  int flags = fcntl(socket_value, F_GETFL, 0);
  return flags >= 0 && fcntl(socket_value, F_SETFL, flags | O_NONBLOCK) == 0;
}
static int ku_http_socket_would_block(int error) { return error == EAGAIN || error == EWOULDBLOCK; }
#endif

/* POSIX owners use a nonblocking byte pipe: a one-byte write is atomic and
   EAGAIN means data is already buffered for poll. Windows keeps its existing
   nonblocking loopback datagrams without coalescing; UDP send pressure alone
   does not establish that its receiver is readable. */
typedef struct {
  KuHttpSocket read_socket; KuHttpSocket write_socket;
#if !defined(_WIN32)
  KuAtomicRefcount pending;
#endif
} KuHttpWake;
#if !defined(_WIN32)
static size_t ku_http_wake_pending_exchange(KuHttpWake* wake,size_t value) {
  return atomic_exchange_explicit(&wake->pending,value,memory_order_acq_rel);
}
#endif
static void ku_http_wake_close(KuHttpWake* wake) {
  ku_http_socket_close(wake->read_socket);
  ku_http_socket_close(wake->write_socket);
  wake->read_socket = wake->write_socket = KU_HTTP_INVALID_SOCKET;
}
static int ku_http_wake_init(KuHttpWake* wake) {
  wake->read_socket = wake->write_socket = KU_HTTP_INVALID_SOCKET;
#if !defined(_WIN32)
  atomic_init(&wake->pending,(size_t)0);
  int descriptors[2];
  if (pipe(descriptors) != 0) return 0;
  wake->read_socket=descriptors[0]; wake->write_socket=descriptors[1];
  if (!ku_http_socket_nonblocking(wake->read_socket) || !ku_http_socket_nonblocking(wake->write_socket)) goto failed;
  for (size_t i=0;i<2;i++) {
    int flags=fcntl(descriptors[i],F_GETFD,0);
    if (flags<0 || fcntl(descriptors[i],F_SETFD,flags|FD_CLOEXEC)<0) goto failed;
  }
#else
  wake->read_socket = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
  if (wake->read_socket == KU_HTTP_INVALID_SOCKET) return 0;
  struct sockaddr_in address; memset(&address, 0, sizeof(address));
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  address.sin_port = 0;
  if (bind(wake->read_socket, (struct sockaddr*)&address, sizeof(address)) != 0) goto failed;
  int address_length = (int)sizeof(address);
  if (getsockname(wake->read_socket, (struct sockaddr*)&address, &address_length) != 0) goto failed;
  wake->write_socket = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
  if (wake->write_socket == KU_HTTP_INVALID_SOCKET) goto failed;
  if (!ku_http_socket_nonblocking(wake->read_socket) || !ku_http_socket_nonblocking(wake->write_socket)) goto failed;
  if (connect(wake->write_socket, (struct sockaddr*)&address, sizeof(address)) != 0) goto failed;
#endif
  return 1;
failed:
  ku_http_wake_close(wake);
  return 0;
}
static void ku_http_wake_notify(KuHttpWake* wake) {
#if !defined(_WIN32)
  /* The consumer clears with acquire before its next work scan. A producer
     after that clear installs a fresh byte; earlier publications join the scan.
     This eight-byte marker is owner-stack storage, not a Task heap charge. */
  if (ku_http_wake_pending_exchange(wake,1)) return;
#endif
  const char byte = 1;
  for (int attempt = 0; attempt < 8; attempt++) {
#if defined(_WIN32)
    int sent = ku_http_socket_send(wake->write_socket, &byte, 1);
#else
    ssize_t sent = write(wake->write_socket, &byte, 1);
#endif
    if (sent == 1) return;
    int error = ku_http_socket_last_error();
    if (sent < 0 && ku_http_socket_would_block(error)) return;
    if (sent < 0 && ku_http_socket_error_interrupted(error)) continue;
    fputs("http reactor wake failed\n", stderr); exit(1);
  }
  fputs("http reactor wake repeatedly interrupted\n", stderr); exit(1);
}
static void ku_http_wake_drain(KuHttpWake* wake) {
  char bytes[64];
  for (int turn = 0; turn < 256; turn++) {
#if defined(_WIN32)
    int received = ku_http_socket_recv(wake->read_socket, bytes, sizeof(bytes));
    if (received >= 0) continue;
#else
    ssize_t received = read(wake->read_socket, bytes, sizeof(bytes));
    if (received > 0) continue;
    if (received == 0) { fputs("http reactor wake pipe closed\n",stderr); exit(1); }
#endif
    int error = ku_http_socket_last_error();
    if (ku_http_socket_would_block(error)) break;
    if (ku_http_socket_error_interrupted(error)) continue;
    fputs("http reactor wake read failed\n", stderr); exit(1);
  }
#if !defined(_WIN32)
  /* All three callers scan published work before their next poll. At the
     bounded drain limit remaining pipe bytes also keep the descriptor ready. */
  (void)ku_http_wake_pending_exchange(wake,0);
#endif
}

/* Payload/request-map quota, independent from Task/frame budgets. Mutex
   protects worker retirement against socket-owner admission. Fixed runtime
   slots, arbitrary user Ku heap and external library allocations are separate;
   pre-existing owned response backing is admitted at publication. */
struct KuHttpBufferBudget {
  KuHttpMutex lock;
  size_t limit, current, peak, rejected;
};
static int ku_http_buffer_reserve(KuHttpBufferBudget* budget, size_t bytes) {
  if (!bytes) return 1;
  if (!budget) return 0;
  ku_http_mutex_lock(&budget->lock);
  int admitted = bytes <= budget->limit - budget->current;
  if (admitted) {
    budget->current += bytes;
    if (budget->current > budget->peak) budget->peak = budget->current;
  } else if (budget->rejected < SIZE_MAX) budget->rejected++;
  ku_http_mutex_unlock(&budget->lock);
  return admitted;
}
static void ku_http_buffer_refund(KuHttpBufferBudget* budget, size_t bytes) {
  if (!bytes) return;
  if (!budget) { fputs("http buffer budget owner missing\n",stderr); exit(1); }
  ku_http_mutex_lock(&budget->lock);
  if (bytes > budget->current) { fputs("http buffer budget underflow\n",stderr); exit(1); }
  budget->current -= bytes;
  ku_http_mutex_unlock(&budget->lock);
}
static size_t ku_http_buffer_admit_response(KuHttpBufferBudget* budget, KuStruct___ku_http_response* response, KuHttpFailure* failure) {
  size_t response_charge = 0;
  KuString response_strings[3] = { response->body, response->content_type, response->location };
  int response_fits = 1;
  for (int index=0; index<3; index++) if (response_strings[index].storage == KU_STRING_OWNED) {
    size_t bytes = response_strings[index].capacity;
    if (bytes > SIZE_MAX - response_charge) { response_fits = 0; break; }
    response_charge += bytes;
  }
  if (!response_fits || !ku_http_buffer_reserve(budget, response_charge)) {
    if (failure) { failure->reason=KU_HTTP_ERROR_PROJECTION; failure->stage=4; }
    ku_drop_struct___ku_http_response(response);
    *response = ku_http_status_response(503, "Service Unavailable");
    response_charge = 0;
  }
  return response_charge;
}
typedef struct KuHttpQueue {
  KuHttpJob** items;
  int cap; int head; int tail; int count; int closed; int abort;
  size_t resident, running, queued_bytes, peak_queued_bytes;
  KuHttpMutex lock;
  KuHttpCondition not_empty;
  KuHttpWake* wake;
  int owner_work_pending; /* Only the matching event owner reads/writes this. */
  unsigned int owner_inline_remaining;
  unsigned int published_events, wait_events;
  unsigned long long begin_deadline, wait_until;
} KuHttpQueue;
enum { KU_HTTP_QUEUE_CLAIM=1u, KU_HTTP_QUEUE_BEGIN=2u,
       KU_HTTP_QUEUE_COMPLETE=4u, KU_HTTP_QUEUE_RETIRE=8u };
static int ku_http_queue_is_current_owner(const KuHttpQueue* queue);
static void ku_http_queue_notify(KuHttpQueue* queue) {
  if(ku_http_queue_is_current_owner(queue)) queue->owner_work_pending=1;
  else ku_http_wake_notify(queue->wake);
}
/* Publication and wait registration share queue->lock. CLAIM only changes
 * telemetry: popping a job never releases its A lease or physical resident
 * slot. BEGIN matters if it can tighten the owner's existing timer. COMPLETE
 * and physical RETIRE remain distinct conditions; neither substitutes for the
 * other, and the final worker reference is still required for reuse/drain. */
static void ku_http_queue_publish_locked(KuHttpQueue* queue,unsigned int event,
    unsigned long long deadline) {
  queue->published_events|=event;
  if(event==KU_HTTP_QUEUE_BEGIN && deadline<queue->begin_deadline)
    queue->begin_deadline=deadline;
  if((queue->wait_events&event)
      && (event!=KU_HTTP_QUEUE_BEGIN || deadline<queue->wait_until)) {
    queue->wait_events=0;
    ku_http_queue_notify(queue);
  }
}
static void ku_http_queue_publish(KuHttpQueue* queue,unsigned int event,
    unsigned long long deadline) {
  if(ku_http_queue_is_current_owner(queue)) {queue->owner_work_pending=1;return;}
  ku_http_mutex_lock(&queue->lock);
  ku_http_queue_publish_locked(queue,event,deadline);
  ku_http_mutex_unlock(&queue->lock);
}
/* Reset before the owner scans job/retirement state, not before sleeping.
 * A publication during that scan remains recorded. Registration rechecks it
 * under the publisher's lock, so a later producer either sees the armed wait
 * and writes the wake channel, or makes this owner take a nonblocking turn. */
static void ku_http_queue_observation_begin_locked(KuHttpQueue* queue) {
  queue->wait_events=0;queue->published_events=0;queue->begin_deadline=~0ULL;
}
static int ku_http_queue_arm_wait(KuHttpQueue* queue,unsigned long long deadline) {
  ku_http_mutex_lock(&queue->lock);
  int published=(queue->published_events&(KU_HTTP_QUEUE_COMPLETE|KU_HTTP_QUEUE_RETIRE))!=0
      || ((queue->published_events&KU_HTTP_QUEUE_BEGIN) && queue->begin_deadline<deadline);
  queue->wait_until=deadline;
  queue->wait_events=published ? 0u
      : KU_HTTP_QUEUE_BEGIN|KU_HTTP_QUEUE_COMPLETE|KU_HTTP_QUEUE_RETIRE;
  ku_http_mutex_unlock(&queue->lock);return published;
}
static int ku_http_queue_take_owner_work(KuHttpQueue* queue) {
  int pending=queue->owner_work_pending;queue->owner_work_pending=0;return pending;
}
struct KuHttpJob {
  KuAtomicRefcount owners;
  KuHttpMutex lock;
  KuHttpQueue* queue;
  KuHttpBufferBudget* budget; size_t response_charge, projection_charge;
  KuHttpInput input;
  KuStruct___ku_http_response response;
  unsigned long long handler_deadline, request_deadline;
  KuHttpFailure failure;
  int claimed; int started; int completed; int cancelled;
};
static int ku_http_job_projection(KuHttpJob* job,size_t bytes) {
  if (!ku_http_buffer_reserve(job->budget,bytes)) return 0;
  job->projection_charge=bytes; return 1;
}
static int ku_http_queue_init(KuHttpQueue* queue, int cap, KuHttpWake* wake) {
  memset(queue, 0, sizeof(*queue));
  queue->cap = cap;
  queue->wake = wake;
  queue->items = (KuHttpJob**)calloc((size_t)cap, sizeof(KuHttpJob*));
  if (!queue->items) return 0;
  if (!ku_http_sync_init(&queue->lock, &queue->not_empty)) {
    free(queue->items); queue->items = NULL; return 0;
  }
  return 1;
}
static KuHttpJob* ku_http_job_new(KuHttpQueue* queue, KuHttpInput* input) {
  KuHttpJob* job = (KuHttpJob*)calloc(1, sizeof(KuHttpJob));
  if (!job) return NULL;
  if (!ku_http_mutex_init(&job->lock)) { free(job); return NULL; }
  ku_atomic_refcount_init(&job->owners);
  job->queue = queue;
  job->budget = input->budget;
  job->input = *input; *input = (KuHttpInput){0};
  ku_http_mutex_lock(&queue->lock);
  queue->resident++;
  ku_http_mutex_unlock(&queue->lock);
  return job;
}
static void ku_http_job_release(KuHttpJob* job) {
  if (!job) return;
  KuHttpQueue* queue = job->queue;
  if (ku_refcount_release(&job->owners, "http job")) {
    ku_http_input_drop(&job->input);
    ku_drop_struct___ku_http_response(&job->response);
    ku_http_buffer_refund(job->budget, job->response_charge);
    ku_http_mutex_destroy(&job->lock);
    free(job);
    ku_http_mutex_lock(&queue->lock);
    if (!queue->resident) { fputs("http job retirement underflow\n", stderr); exit(1); }
    queue->resident--;
    ku_http_mutex_unlock(&queue->lock);
  }
  /* Release publication precedes wake, including the non-final worker owner.
     ClosedWaitingJob can now see the actual remaining owner count. */
  ku_http_queue_publish(queue,KU_HTTP_QUEUE_RETIRE,0);
}
static int ku_http_job_only_owner(KuHttpJob* job) {
  /* The calling connection already owns a reference. The only additional
   * retain is queue_push, completed by this owner before worker publication;
   * workers cannot create a new owner after retiring their reference. Acquire
   * observes that release and its preceding cleanup, without a write-back RMW.
   * The shared/general reference-count implementation is unchanged. */
#if defined(_MSC_VER)
  return (size_t)_InterlockedCompareExchange64(&job->owners,0,0)==1u;
#else
  return atomic_load_explicit(&job->owners,memory_order_acquire)==1u;
#endif
}
static void ku_http_job_begin(KuHttpJob* job, unsigned long long timeout_ms) {
  /* Exactly once at actual handler invocation, preserving native's original
     deadline origin. No queue or readiness turn restarts an active deadline. */
  unsigned long long now = ku_http_now_ms();
  unsigned long long remaining = job->request_deadline > now ? job->request_deadline-now : 0;
  if (timeout_ms > remaining) timeout_ms = remaining;
  __ku_handler_timeout_begin(timeout_ms);
  ku_http_mutex_lock(&job->lock);
  if (job->started) { fputs("http handler deadline restarted\n", stderr); exit(1); }
  job->handler_deadline = __ku_handler_deadline;
  job->started = 1;
  ku_http_mutex_unlock(&job->lock);
  ku_http_queue_publish(job->queue,KU_HTTP_QUEUE_BEGIN,job->handler_deadline);
}
static void ku_http_job_run(KuHttpServer* server, KuHttpJob* job) {
  ku_http_mutex_lock(&job->lock);
  int cancelled = job->cancelled || ku_http_now_ms() >= job->request_deadline;
  if (cancelled) { job->cancelled = 1; job->completed = 1; }
  ku_http_mutex_unlock(&job->lock);
  if (cancelled) { ku_http_input_drop(&job->input); ku_http_queue_publish(job->queue,KU_HTTP_QUEUE_COMPLETE,0); return; }
  ku_http_failure_current=(KuHttpFailure){0};
  KuStruct___ku_http_response response = ku_http_execute_request(server, &job->input, job);
  KuHttpFailure failure=ku_http_failure_current;
  failure.stage=2;
  if(response.status==500 && !failure.reason) failure.reason=KU_HTTP_ERROR_HANDLER_500;
  ku_http_input_drop(&job->input);
  size_t response_charge = ku_http_buffer_admit_response(job->budget, &response, &failure);
  ku_http_buffer_refund(job->budget,job->projection_charge); job->projection_charge=0;
  ku_http_mutex_lock(&job->lock);
  job->response_charge = response_charge;
  job->failure = failure;
  job->response = response;
  job->completed = 1;
  ku_http_mutex_unlock(&job->lock);
  ku_http_queue_publish(job->queue,KU_HTTP_QUEUE_COMPLETE,0);
}
/* Only compiler-proven bounded work enters here, once, before any worker
 * publication. Save every synchronous execution TLS field: a local request
 * must not inherit or contaminate the event owner's surrounding native frame. */
static void ku_http_job_run_owner(KuHttpServer* server,KuHttpJob* job) {
  long depth=__ku_call_depth;
  unsigned long long deadline=__ku_handler_deadline,cleanup=__ku_handler_cleanup_deadline;
  int timed_out=__ku_handler_timed_out;unsigned long unwind=__ku_handler_unwind_depth;
  KuSyncExitSignal signal=__ku_sync_return_signal;KuHttpFailure failure=ku_http_failure_current;
  __ku_call_depth=0;__ku_handler_deadline=0;__ku_handler_cleanup_deadline=0;
  __ku_handler_timed_out=0;__ku_handler_unwind_depth=0;__ku_sync_reset();
  ku_http_job_run(server,job);
  __ku_call_depth=depth;__ku_handler_deadline=deadline;__ku_handler_cleanup_deadline=cleanup;
  __ku_handler_timed_out=timed_out;__ku_handler_unwind_depth=unwind;
  __ku_sync_return_signal=signal;ku_http_failure_current=failure;
}
static int ku_http_queue_push(KuHttpQueue* queue, KuHttpJob* job) {
  int accepted = 0;
  ku_http_mutex_lock(&queue->lock);
  if (!queue->closed) {
    /* Each queued job already owns one of cap live slots. A second pending
       credit is neither acquired nor needed; a full queue here is a bug. */
    if (queue->count >= queue->cap) { fputs("http job queue capacity invariant failed\n", stderr); exit(1); }
    ku_refcount_retain(&job->owners, "http job");
    queue->items[queue->tail] = job;
    queue->tail = (queue->tail + 1) % queue->cap;
    queue->count++;
    queue->queued_bytes+=job->input.body_charge+job->input.metadata_charge;
    if (queue->queued_bytes>queue->peak_queued_bytes) queue->peak_queued_bytes=queue->queued_bytes;
    ku_http_condition_signal(&queue->not_empty);
    accepted = 1;
  }
  ku_http_mutex_unlock(&queue->lock);
  return accepted;
}
static KuHttpJob* ku_http_queue_pop(KuHttpQueue* queue) {
  KuHttpJob* job = NULL;
  ku_http_mutex_lock(&queue->lock);
  while (queue->count == 0 && !queue->closed) ku_http_condition_wait(&queue->not_empty, &queue->lock);
  if (queue->count && !queue->abort) {
    job = queue->items[queue->head];
    queue->items[queue->head] = NULL;
    queue->head = (queue->head + 1) % queue->cap;
    queue->count--;
    queue->queued_bytes-=job->input.body_charge+job->input.metadata_charge;
    ku_http_mutex_lock(&job->lock);
    job->claimed = 1; queue->running++;
    ku_http_mutex_unlock(&job->lock);
  }
  if (job) ku_http_queue_publish_locked(queue,KU_HTTP_QUEUE_CLAIM,0);
  ku_http_mutex_unlock(&queue->lock);
  return job;
}

static void ku_http_queue_cancel_job(KuHttpJob* job) {
  KuHttpQueue* queue = job->queue;
  int removed = 0;
  ku_http_mutex_lock(&queue->lock);
  for (int i=0; i<queue->count; i++) {
    int at = (queue->head+i)%queue->cap;
    if (queue->items[at] != job) continue;
    for (int j=i; j+1<queue->count; j++)
      queue->items[(queue->head+j)%queue->cap] = queue->items[(queue->head+j+1)%queue->cap];
    queue->tail = (queue->tail+queue->cap-1)%queue->cap;
    queue->items[queue->tail] = NULL; queue->count--;
    queue->queued_bytes-=job->input.body_charge+job->input.metadata_charge; removed = 1; break;
  }
  ku_http_mutex_lock(&job->lock);
  job->cancelled = 1;
  if (removed) job->completed = 1;
  ku_http_mutex_unlock(&job->lock);
  ku_http_mutex_unlock(&queue->lock);
  if (removed) { ku_http_input_drop(&job->input); ku_http_job_release(job); }
}
static void ku_http_queue_close(KuHttpQueue* queue, int drain) {
  ku_http_mutex_lock(&queue->lock);
  queue->closed = 1;
  if (!drain) queue->abort = 1;
  ku_http_condition_broadcast(&queue->not_empty);
  ku_http_mutex_unlock(&queue->lock);
  if (!drain) for (;;) {
    ku_http_mutex_lock(&queue->lock);
    KuHttpJob* job = NULL;
    if (queue->count) {
      job = queue->items[queue->head];
      queue->items[queue->head] = NULL;
      queue->head = (queue->head + 1) % queue->cap;
      queue->count--;
      queue->queued_bytes-=job->input.body_charge+job->input.metadata_charge;
    }
    ku_http_mutex_unlock(&queue->lock);
    if (!job) break;
    ku_http_input_drop(&job->input);
    ku_http_mutex_lock(&job->lock);
    job->cancelled = 1; job->completed = 1;
    ku_http_mutex_unlock(&job->lock);
    ku_http_job_release(job);
  }
}
static size_t ku_http_queue_resident(KuHttpQueue* queue) {
  ku_http_mutex_lock(&queue->lock);
  size_t resident = queue->resident;
  ku_http_mutex_unlock(&queue->lock);
  return resident;
}
static void ku_http_queue_free(KuHttpQueue* queue) {
  if (queue->count || queue->resident) { fputs("http queue freed before actual retirement\n", stderr); exit(1); }
  ku_http_sync_destroy(&queue->lock, &queue->not_empty);
  free(queue->items); queue->items = NULL;
}
typedef struct { KuHttpServer* server; KuHttpQueue* queue; } KuHttpWorkerCtx;
#if defined(_WIN32)
static unsigned __stdcall ku_http_worker(void* argument) {
#else
static void* ku_http_worker(void* argument) {
#endif
  KuHttpWorkerCtx* context = (KuHttpWorkerCtx*)argument;
  for (;;) {
    KuHttpJob* job = ku_http_queue_pop(context->queue);
    if (!job) break;
    ku_http_job_run(context->server, job);
    ku_http_mutex_lock(&context->queue->lock);
    if (!context->queue->running) _Exit(78);
    context->queue->running--;
    ku_http_mutex_unlock(&context->queue->lock);
    ku_http_job_release(job);
  }
#if defined(KU_NATIVE_RUNTIME_MYSQL)
  /* Every request/input/temporary owned value was retired on this worker. */
  ku_mysql_thread_shutdown();
#endif
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

typedef struct { const char* bytes; size_t length; } KuHttpPart;
typedef struct {
  KuStruct___ku_http_response response;
  size_t charge;
  /* Status is normalized to 100..599 before formatting. The longest private
     reason is 31 bytes, so the complete status line including NUL needs 47. */
  char first[64]; char last[128];
  KuHttpPart parts[9];
  int count; int part;
  size_t offset;
} KuHttpOutput;
/* One bounded syscall publishes adjacent status/header/body slices together.
   Sending the short status line separately can stall subsequent bytes until
   a delayed ACK on a persistent TCP connection. These vectors borrow the
   response owner's storage only for this synchronous nonblocking call. */
static int ku_http_output_send(KuHttpSocket socket_value,const KuHttpOutput* output,size_t limit) {
#if defined(_WIN32)
  WSABUF vectors[9];
#else
  struct iovec vectors[9];
#endif
  size_t count=0, total=0;
  if (limit>(size_t)65536) limit=(size_t)65536;
  for (int part=output->part; part<output->count && count<9 && total<limit; part++) {
    size_t offset=part==output->part ? output->offset : 0;
    size_t length=output->parts[part].length-offset;
    if (length>limit-total) length=limit-total;
    if (!length) continue;
#if defined(_WIN32)
    vectors[count].buf=(CHAR*)(output->parts[part].bytes+offset);
    vectors[count].len=(ULONG)length;
#else
    vectors[count].iov_base=(void*)(output->parts[part].bytes+offset);
    vectors[count].iov_len=length;
#endif
    total+=length; count++;
  }
  if (!count) return 0;
#if defined(_WIN32)
  DWORD sent=0;
  if (WSASend(socket_value,vectors,(DWORD)count,&sent,0,NULL,NULL)==SOCKET_ERROR) return -1;
  return (int)sent;
#else
  struct msghdr message={0};
  message.msg_iov=vectors; message.msg_iovlen=count;
#if defined(__APPLE__)
  /* Accepted sockets already use SO_NOSIGPIPE. */
  ssize_t sent=sendmsg(socket_value,&message,0);
#elif defined(MSG_NOSIGNAL)
  ssize_t sent=sendmsg(socket_value,&message,MSG_NOSIGNAL);
#else
#error "std.http POSIX transport requires MSG_NOSIGNAL or SO_NOSIGPIPE"
#endif
  return (int)sent; /* The bounded vector sum is at most 65536. */
#endif
}
enum {
  KU_HTTP_IO_FREE = 0, KU_HTTP_IO_HEADER = 1, KU_HTTP_IO_BODY = 2,
  KU_HTTP_IO_JOB = 3, KU_HTTP_IO_WRITE = 4, KU_HTTP_IO_CLOSED_JOB = 5,
  KU_HTTP_IO_REJECT = 6, KU_HTTP_IO_REUSE_WAIT = 7,
  KU_HTTP_IO_ASYNC = 8, KU_HTTP_IO_CLOSED_ASYNC = 9,
  KU_HTTP_IO_REQUEST_WAIT = 10
};
typedef struct KuHttpActiveSet {
  struct KuHttpConnection* connections;
  int* indices;
  int count,capacity,free_head,high_water;
  size_t sockets;
  int request_limit,request_active,request_peak;
  struct KuHttpPending* pending;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  struct KuHttpReactor* reactor;
#endif
} KuHttpActiveSet;
typedef struct KuHttpConnection {
  KuHttpSocket socket_value;
  KuHttpBufferBudget* budget;
  KuHttpActiveSet* active_set;
  int active_position,free_next;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  struct KuHttpReactor* reactor;
  int work_position,idle_position;
#endif
  int phase; int poll_index; int management;
  int request_lease,request_queued,pending_index,header_started;
  unsigned long long request_wait_probe_deadline;
  uint64_t connection_slot,connection_generation,request_sequence;
  unsigned char request_id[48]; uint32_t request_id_length,request_id_truncated;
  KuHttpFailure failure;
  unsigned long long deadline, request_deadline;
  char* header; size_t capacity; size_t buffered;
  size_t body_received;
  char* carry; size_t carry_length;
  int keep_alive; int reused;
  KuHttpInput input;
  KuHttpJob* job;
  void* async_task;
  int async_cancelled, peer_read_paused;
  unsigned long long async_deadline, async_cleanup_deadline;
#if defined(__APPLE__)
  unsigned long long peer_error_probe_deadline;
#endif
  KuHttpOutput output;
  size_t rejected_bytes; size_t rejected_seen; unsigned char rejected_window[4];
} KuHttpConnection;
typedef struct { int slot; uint64_t generation; } KuHttpPollConnection;
static int ku_http_request_prepare(KuHttpConnection*,KuHttpServer*);
static void ku_http_request_cancel_pending(KuHttpConnection*);
static int ku_http_request_lease_take(KuHttpConnection* connection) {
  KuHttpActiveSet* active=connection->active_set;
  if(!active || connection->management || connection->request_lease) return 1;
  if(active->request_active>=active->request_limit) return 0;
  connection->request_lease=1;active->request_active++;
  if(active->request_active>active->request_peak) active->request_peak=active->request_active;
  return 1;
}
static void ku_http_request_lease_release(KuHttpConnection* connection) {
  if(!connection->request_lease) return;
  KuHttpActiveSet* active=connection->active_set;
  if(!active || active->request_active<=0) {fputs("http request lease refund invariant failed\n",stderr);exit(1);}
  connection->request_lease=0;active->request_active--;
}
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
static void ku_http_reactor_admit(KuHttpActiveSet*,KuHttpConnection*);
static void ku_http_reactor_retire(KuHttpConnection*);
static void ku_http_reactor_forget(KuHttpConnection*);
#endif
/* Owner-thread-only indices borrow fixed connection storage. Neither the
   dense table nor the free chain owns a socket, Task, job, or lease. */
static int ku_http_free_slot(const KuHttpActiveSet* active) {
  if(active->free_head>=0) return active->free_head;
  return active->high_water<active->capacity ? active->high_water : -1;
}
static void ku_http_active_begin(KuHttpActiveSet* active,KuHttpConnection* connection) {
  if(!active) return; /* Dedicated management/rejection storage. */
  uintptr_t first=(uintptr_t)active->connections,at=(uintptr_t)connection;
  if(at<first || at-first>=(size_t)active->capacity*sizeof(*connection)
      || (at-first)%sizeof(*connection) || active->count>=active->capacity) {
    fputs("http active-slot admission invariant failed\n",stderr); exit(1);
  }
  int slot=(int)((at-first)/sizeof(*connection));
  if(active->free_head>=0) {
    if(slot!=active->free_head || slot>=active->high_water
        || connection->active_set!=active || connection->active_position!=-1) {
      fputs("http recycled-slot admission invariant failed\n",stderr); exit(1);
    }
    active->free_head=connection->free_next;
  } else {
    if(slot!=active->high_water) {
      fputs("http fresh-slot admission invariant failed\n",stderr); exit(1);
    }
    /* calloc reserves the same stable full-capacity backing and budget. Only
       admitted slots are written: untouched entries never own a descriptor,
       job or Task and are never included in drain/poll traversal. */
    connection->socket_value=KU_HTTP_INVALID_SOCKET;
    connection->active_set=active; connection->free_next=-1;
    active->high_water++;
  }
  connection->active_position=active->count;
  active->indices[active->count++]=slot;
  active->sockets++;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  ku_http_reactor_admit(active,connection);
#endif
}
static void ku_http_active_retire(KuHttpConnection* connection) {
  KuHttpActiveSet* active=connection->active_set;
  if(!active || connection->active_position<0) return;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  ku_http_reactor_retire(connection);
#endif
  int slot=(int)(connection-active->connections),position=connection->active_position;
  if(position>=active->count || active->indices[position]!=slot) {
    fputs("http active-slot retirement invariant failed\n",stderr); exit(1);
  }
  int last=active->indices[--active->count];
  if(position<active->count) {
    active->indices[position]=last;
    active->connections[last].active_position=position;
  }
  connection->active_position=-1;
  connection->free_next=active->free_head;
  active->free_head=slot;
}
static KuHttpConnection* ku_http_polled_connection(KuHttpActiveSet* active,
    const KuHttpPollConnection* interest,int poll_index,KuHttpSocket socket_value) {
  if(interest->slot<0 || interest->slot>=active->high_water) return NULL;
  KuHttpConnection* connection=&active->connections[interest->slot];
  if(connection->active_set!=active || connection->active_position<0 || connection->connection_generation!=interest->generation
      || connection->poll_index!=poll_index || connection->socket_value!=socket_value) return NULL;
  return connection;
}
enum {
  KU_HTTP_CLOSE_IDLE_PRESSURE, KU_HTTP_CLOSE_RESPONSE_POLICY,
  KU_HTTP_CLOSE_WRITE_DEADLINE, KU_HTTP_CLOSE_PEER_WRITE,
  KU_HTTP_CLOSE_QUEUE_NOT_ADMITTED, KU_HTTP_CLOSE_DRAIN_IDLE,
  KU_HTTP_CLOSE_REASON_COUNT
};
static void ku_http_count_transport_close(KuHttpConnection*,int);
static void ku_http_connection_identity(KuHttpConnection*);
static void ku_http_record_failure(KuHttpConnection*,int);
static void ku_http_output_part(KuHttpOutput* output, const char* bytes, size_t length) {
  if (!length) return;
  if (output->count >= 9) { fputs("http response segment bound exceeded\n", stderr); exit(1); }
  output->parts[output->count++] = (KuHttpPart){bytes, length};
}
static int ku_http_async_retire(KuHttpConnection*);
static void ku_http_count_response(int);
static void ku_http_count_admitted(void);
static void ku_http_connection_close(KuHttpConnection* connection) {
  ku_http_request_cancel_pending(connection);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  /* Delete the old full-generation registration before close can recycle fd. */
  ku_http_reactor_forget(connection);
#endif
#if defined(__APPLE__)
  connection->peer_error_probe_deadline=0;
#endif
  if (connection->socket_value != KU_HTTP_INVALID_SOCKET) {
    if(connection->active_set) {
      if(!connection->active_set->sockets) { fputs("http active socket accounting failed\n",stderr); exit(1); }
      connection->active_set->sockets--;
    }
    ku_http_socket_shutdown_write(connection->socket_value);
    ku_http_socket_close(connection->socket_value);
    connection->socket_value = KU_HTTP_INVALID_SOCKET;
    if (!connection->management) ku_http_atomic_decrement(&ku_http_active_conns);
  }
  free(connection->header); connection->header = NULL;
  ku_http_buffer_refund(connection->budget, connection->capacity); connection->capacity = 0;
  free(connection->carry); connection->carry = NULL;
  ku_http_buffer_refund(connection->budget, connection->carry_length); connection->carry_length = 0;
  ku_http_input_drop(&connection->input);
  ku_drop_struct___ku_http_response(&connection->output.response);
  ku_http_buffer_refund(connection->budget, connection->output.charge); connection->output.charge = 0;
  if (connection->job) ku_http_queue_cancel_job(connection->job);
  if (connection->async_task && !ku_http_async_retire(connection)) {
    connection->phase = KU_HTTP_IO_CLOSED_ASYNC;
    return;
  }
  if (connection->job && !ku_http_job_only_owner(connection->job)) {
    /* Keep the active slot charged until the queue/worker's real reference
       retires. No completed or disconnected request can create an unbounded
       tail of jobs while this physical socket/slot would otherwise be reused. */
    connection->phase = KU_HTTP_IO_CLOSED_JOB;
    connection->deadline = 0;
    return;
  }
  if (connection->job) ku_http_job_release(connection->job);
  /* No queued/worker reference and no Task handle survives this boundary. */
  ku_http_request_lease_release(connection);
  ku_http_active_retire(connection);
  KuHttpActiveSet* active=connection->active_set;
  int free_next=connection->free_next;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  struct KuHttpReactor* reactor=connection->reactor;
#endif
  memset(connection, 0, sizeof(*connection));
  connection->socket_value = KU_HTTP_INVALID_SOCKET;
  connection->active_set=active; connection->active_position=-1; connection->free_next=free_next;
  connection->pending_index=-1;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  connection->reactor=reactor; connection->work_position=-1; connection->idle_position=-1;
#endif
}
static void ku_http_connection_response(KuHttpConnection* connection, KuHttpServer* server, KuStruct___ku_http_response response, size_t response_charge, int admitted) {
  ku_http_request_cancel_pending(connection);
#if defined(__APPLE__)
  connection->peer_error_probe_deadline=0;
#endif
  free(connection->header); connection->header = NULL;
  ku_http_buffer_refund(connection->budget, connection->capacity); connection->capacity = 0;
  ku_http_input_drop(&connection->input);
  ku_drop_struct___ku_http_response(&connection->output.response);
  ku_http_buffer_refund(connection->budget, connection->output.charge); connection->output.charge = 0;
  memset(&connection->output, 0, sizeof(connection->output));
  if (response.status < 100 || response.status > 599 ||
      !ku_http_field_value_safe(response.content_type.ptr, response.content_type.len) ||
      !ku_http_field_value_safe(response.location.ptr, response.location.len)) {
    ku_drop_struct___ku_http_response(&response);
    ku_http_buffer_refund(connection->budget, response_charge); response_charge = 0; admitted = 1;
    response = ku_http_status_response(500, "Internal Server Error");
    connection->failure.reason=KU_HTTP_ERROR_INVALID_RESPONSE;
    connection->failure.stage=4;
  }
  if (!admitted) response_charge = ku_http_buffer_admit_response(connection->budget, &response, &connection->failure);
  connection->output.charge = response_charge;
  connection->keep_alive = connection->keep_alive && response.status >= 200 && response.status < 400;
  KuHttpOutput* output = &connection->output;
  output->response = response;
  ku_http_count_response((int)response.status);
  if(response.status>=500) ku_http_record_failure(connection,(int)response.status);
  size_t body_length = ((response.status >= 100 && response.status < 200) || response.status == 204 || response.status == 304)
    ? 0 : response.body.len;
  int first_length = snprintf(output->first, sizeof(output->first), "HTTP/1.1 %lld %s\r\n", (long long)response.status, ku_http_status_text(response.status));
  if (first_length < 0 || (size_t)first_length >= sizeof(output->first)) {
    ku_http_connection_close(connection); return;
  }
  ku_http_output_part(output, output->first, (size_t)first_length);
  if (response.content_type.len) {
    ku_http_output_part(output, "content-type: ", 14);
    ku_http_output_part(output, (const char*)response.content_type.ptr, response.content_type.len);
    ku_http_output_part(output, "\r\n", 2);
  }
  if (response.location.len) {
    ku_http_output_part(output, "location: ", 10);
    ku_http_output_part(output, (const char*)response.location.ptr, response.location.len);
    ku_http_output_part(output, "\r\n", 2);
  }
  int last_length = ((response.status >= 100 && response.status < 200) || response.status == 204 || response.status == 304)
    ? snprintf(output->last, sizeof(output->last), "connection: %s\r\n\r\n", connection->keep_alive ? "keep-alive" : "close")
    : snprintf(output->last, sizeof(output->last), "content-length: %llu\r\nconnection: %s\r\n\r\n", (unsigned long long)body_length, connection->keep_alive ? "keep-alive" : "close");
  if (last_length < 0 || (size_t)last_length >= sizeof(output->last)) {
    ku_http_connection_close(connection); return;
  }
  ku_http_output_part(output, output->last, (size_t)last_length);
  ku_http_output_part(output, (const char*)response.body.ptr, body_length);
  connection->phase = KU_HTTP_IO_WRITE;
  connection->deadline = ku_http_deadline_ms((uint32_t)(server->write_timeout_ms > 0 ? server->write_timeout_ms : 10000));
}
static void ku_http_connection_status(KuHttpConnection* connection, KuHttpServer* server, int status) {
  connection->keep_alive = 0;
  if(!connection->failure.reason && (status==504 || status==408)) connection->failure.reason=KU_HTTP_ERROR_DEADLINE;
  ku_http_connection_response(connection, server, ku_http_status_response(status, ku_http_status_text(status)), 0, 0);
}
static void ku_http_connection_start(KuHttpConnection* connection, KuHttpActiveSet* active, KuHttpSocket socket_value, KuHttpServer* server) {
  ku_http_active_begin(active,connection);
  int position=connection->active_position;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  int work_position=active?connection->work_position:-1;
#endif
  memset(connection, 0, sizeof(*connection));
  connection->active_set=active; connection->active_position=position;
  connection->pending_index=-1;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  connection->reactor=active?active->reactor:NULL;
  connection->work_position=work_position; connection->idle_position=-1;
#endif
  connection->socket_value = socket_value;
  connection->budget = server->buffer_budget;
  connection->phase = KU_HTTP_IO_HEADER;
  ku_http_connection_identity(connection);
  connection->request_sequence=1;
  connection->request_deadline = ku_http_deadline_ms(server->request_timeout_ms);
  connection->deadline = ku_http_deadline_ms((uint32_t)(server->idle_timeout_ms > 0 ? server->idle_timeout_ms : 5000));
}
/* A response may be published before the worker releases its real reference.
   Reuse the physical slot only after that owner retires, exactly as close does. */
static void ku_http_connection_next_request(KuHttpConnection* connection, KuHttpServer* server) {
  if (!connection->keep_alive) {
    ku_http_count_transport_close(connection,KU_HTTP_CLOSE_RESPONSE_POLICY);
    ku_http_connection_close(connection); return;
  }
  if (connection->job && !ku_http_job_only_owner(connection->job)) {
    connection->phase = KU_HTTP_IO_REUSE_WAIT; connection->deadline = 0; return;
  }
  if (connection->job) { ku_http_job_release(connection->job); connection->job = NULL; }
  ku_http_request_lease_release(connection);
  ku_drop_struct___ku_http_response(&connection->output.response);
  ku_http_buffer_refund(connection->budget, connection->output.charge); connection->output.charge = 0;
  memset(&connection->output, 0, sizeof(connection->output));
  connection->keep_alive = 0; connection->reused = 1;
  if(connection->request_sequence==UINT64_MAX) {ku_http_connection_close(connection);return;}
  connection->request_sequence++;
  connection->header_started=0;connection->request_wait_probe_deadline=0;
  connection->request_id_length=0; connection->request_id_truncated=0;
  connection->peer_read_paused=0;
#if defined(__APPLE__)
  connection->peer_error_probe_deadline=0;
#endif
  connection->failure=(KuHttpFailure){0};
  connection->request_deadline = 0;
  connection->buffered = 0; connection->body_received = 0;
  connection->phase = KU_HTTP_IO_HEADER;
  connection->deadline = ku_http_deadline_ms((uint32_t)(server->idle_timeout_ms > 0 ? server->idle_timeout_ms : 5000));
}
static int ku_http_control_request(KuHttpConnection*, KuHttpServer*);
static void ku_http_count_request(void);
static void ku_http_count_response(int);
static void ku_http_count_admitted(void);
static int ku_http_async_submit(KuHttpConnection*, KuHttpServer*);
static int ku_http_async_observe(KuHttpConnection*, KuHttpServer*);
static int ku_http_reused_request_admit(void);
static void ku_http_connection_submit(KuHttpConnection* connection, KuHttpServer* server, KuHttpQueue* queue, int workers) {
  if (ku_http_control_request(connection, server)) return;
  if (connection->management) { ku_http_connection_status(connection,server,404); return; }
  ku_http_count_request();
  if(connection->input.route) {
    KuHttpRouteToken* token=connection->input.route;
    connection->request_id_length=token->rid_length;
    connection->request_id_truncated=token->rid_truncated;
    if(token->rid_length)memcpy(connection->request_id,connection->input.target+token->rid_offset,token->rid_length);
  } else {
    KuValue* rid=connection->input.headers?ku_object_get(connection->input.headers,
        ku_string_static((const uint8_t*)"x-request-id",12)):NULL;
    if(rid && rid->tag==KU_STR) {
      size_t n=rid->as.s.len;
      if(n>sizeof(connection->request_id)){n=sizeof(connection->request_id);connection->request_id_truncated=1;}
      memcpy(connection->request_id,rid->as.s.ptr,n);connection->request_id_length=(uint32_t)n;
    }
  }
  if (!ku_http_utf8_valid((const uint8_t*)connection->input.body, connection->input.body_len)) {
    ku_http_connection_status(connection, server, 400); return;
  }
  if (ku_http_async_submit(connection, server)) return;
  KuHttpJob* job = ku_http_job_new(queue, &connection->input);
  if (!job) { connection->failure.reason=KU_HTTP_ERROR_ALLOCATION; ku_http_connection_status(connection, server, 500); return; }
  job->request_deadline = connection->request_deadline;
  connection->job = job;
  connection->phase = KU_HTTP_IO_JOB;
  connection->deadline = 0;
  if(ku_http_queue_is_current_owner(queue) && queue->owner_inline_remaining
      && job->input.route && job->input.route->status==200u
      && ku_http_handler_is_short(job->input.route->invoke)) {
    queue->owner_inline_remaining--;
    ku_http_job_run_owner(server,job);return;
  }
  if (!workers || !ku_http_queue_push(queue, job)) {
    ku_http_count_transport_close(connection,KU_HTTP_CLOSE_QUEUE_NOT_ADMITTED);
    ku_http_connection_close(connection);
  } else ku_http_count_admitted();
}
static void ku_http_connection_read_header(KuHttpConnection* connection, KuHttpServer* server, KuHttpQueue* queue, int workers) {
  if (ku_http_now_ms() >= connection->deadline) { ku_http_connection_status(connection, server, 408); return; }
  if (!ku_http_request_prepare(connection,server)) return;
  /* Empty C-only sockets do not allocate input backing or consume A. */
  if (!connection->header) {
    if (!ku_http_buffer_reserve(connection->budget,1024u)) {connection->failure.reason=KU_HTTP_ERROR_PROJECTION;ku_http_connection_status(connection,server,503);return;}
    connection->capacity=1024u;connection->header=(char*)malloc(connection->capacity);
    if (!connection->header) {connection->failure.reason=KU_HTTP_ERROR_ALLOCATION;ku_http_connection_status(connection,server,500);return;}
  }
  char incoming[KU_HTTP_READ_CHUNK];
  int received;
  if (connection->carry_length) {
    if (connection->carry_length > sizeof(incoming)) { connection->failure.reason=KU_HTTP_ERROR_PARSER_INVARIANT; ku_http_connection_status(connection, server, 500); return; }
    received = (int)connection->carry_length;
    memcpy(incoming, connection->carry, connection->carry_length);
    free(connection->carry); connection->carry = NULL;
  ku_http_buffer_refund(connection->budget, connection->carry_length); connection->carry_length = 0;
  } else received = ku_http_socket_recv(connection->socket_value, incoming, sizeof(incoming));
  if (received < 0) {
    int error = ku_http_socket_last_error();
    if (ku_http_socket_would_block(error) || ku_http_socket_error_interrupted(error)) return;
    ku_http_connection_status(connection, server, 408); return;
  }
  if (!received) {
    if (connection->reused && !connection->buffered) ku_http_connection_close(connection);
    else ku_http_connection_status(connection, server, 400);
    return;
  }
  /* Linearize the first bytes of a new persistent request against drain,
     including events returned by a poll that began before drain was set.
     An already partial request retains its original admission and deadline. */
  if (connection->reused && !connection->buffered && !connection->request_deadline
      && !ku_http_reused_request_admit()) { ku_http_connection_close(connection); return; }
  size_t scan_from = connection->buffered;
  if ((size_t)received > SIZE_MAX - scan_from) { connection->failure.reason=KU_HTTP_ERROR_PARSER_INVARIANT; ku_http_connection_status(connection, server, 500); return; }
  size_t needed = scan_from + (size_t)received;
  if (needed > connection->capacity) {
    size_t capacity = connection->capacity;
    while (capacity < needed) {
      if (capacity > SIZE_MAX / 2) { capacity = needed; break; }
      capacity *= 2;
    }
    /* Reserve the full new backing while the old allocation may coexist. */
    if (!ku_http_buffer_reserve(connection->budget, capacity)) { connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection, server, 503); return; }
    char* grown = (char*)realloc(connection->header, capacity);
    if (!grown) {
      ku_http_buffer_refund(connection->budget, capacity);
      connection->failure.reason=KU_HTTP_ERROR_ALLOCATION;
      ku_http_connection_status(connection, server, 500); return;
    }
    ku_http_buffer_refund(connection->budget, connection->capacity);
    connection->header = grown; connection->capacity = capacity;
  }
  memcpy(connection->header + scan_from, incoming, (size_t)received);
  connection->buffered = needed;
  if (!scan_from && !connection->request_deadline) connection->request_deadline = ku_http_deadline_ms(server->request_timeout_ms);
  if (!scan_from && !connection->header_started) {
    connection->header_started=1;
    connection->deadline = ku_http_deadline_ms((uint32_t)(server->read_header_timeout_ms > 0 ? server->read_header_timeout_ms : 5000));
  }
  long long max_header = server->max_header_bytes > 0 ? server->max_header_bytes : (16 * 1024);
  for (size_t index = scan_from; index < needed; index++) {
    const char* header = connection->header;
    char byte = header[index];
    /* Preserve the original byte max_header+1 => 431 precedence even when
       that same byte would also violate a CRLF framing rule. */
    if ((long long)(index + 1) > max_header) { ku_http_connection_status(connection, server, 431); return; }
    if (byte == '\n' && (index < 1 || header[index - 1] != '\r')) { ku_http_connection_status(connection, server, 400); return; }
    if (index >= 1 && header[index - 1] == '\r' && byte != '\n') { ku_http_connection_status(connection, server, 400); return; }
    if (index < 3 || header[index - 3] != '\r' || header[index - 2] != '\n' || header[index - 1] != '\r' || byte != '\n') continue;
    size_t header_length = index - 3;
    connection->input.budget = connection->budget;
    int status = ku_http_parse_complete_header(server, header, header_length, &connection->input);
    if (status) { if(status==500) connection->failure.reason=KU_HTTP_ERROR_ALLOCATION; else if(status==503) connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection, server, status); return; }
    connection->deadline = ku_http_deadline_ms((uint32_t)(server->read_body_timeout_ms > 0 ? server->read_body_timeout_ms : 10000));
    connection->keep_alive = connection->input.keep_alive;
    size_t prefix = needed - (index + 1);
    if (prefix > connection->input.body_len) prefix = connection->input.body_len;
    size_t extra = needed - (index + 1) - prefix;
    if (extra && connection->keep_alive) {
      /* At most this one finite recv chunk can be ahead of the current body. */
      if (extra > KU_HTTP_READ_CHUNK) { connection->failure.reason=KU_HTTP_ERROR_PARSER_INVARIANT; ku_http_connection_status(connection, server, 500); return; }
      if (!ku_http_buffer_reserve(connection->budget, extra)) { connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection, server, 503); return; }
      connection->carry = (char*)malloc(extra);
      if (!connection->carry) {
        ku_http_buffer_refund(connection->budget, extra);
        connection->failure.reason=KU_HTTP_ERROR_ALLOCATION;
        ku_http_connection_status(connection, server, 500); return;
      }
      memcpy(connection->carry, header + index + 1 + prefix, extra);
      connection->carry_length = extra;
    }
    if (prefix) memcpy(connection->input.body, header + index + 1, prefix);
    connection->body_received = prefix;
    free(connection->header); connection->header = NULL;
  ku_http_buffer_refund(connection->budget, connection->capacity); connection->capacity = 0;
    if (prefix == connection->input.body_len) ku_http_connection_submit(connection, server, queue, workers);
    else connection->phase = KU_HTTP_IO_BODY;
    return;
  }
}
static void ku_http_connection_read_body(KuHttpConnection* connection, KuHttpServer* server, KuHttpQueue* queue, int workers) {
  if (ku_http_now_ms() >= connection->deadline) { ku_http_connection_status(connection, server, 408); return; }
  size_t remaining = connection->input.body_len - connection->body_received;
  if (remaining > (size_t)65536) remaining = (size_t)65536;
  int received = ku_http_socket_recv(connection->socket_value, connection->input.body + connection->body_received, remaining);
  if (received < 0) {
    int error = ku_http_socket_last_error();
    if (ku_http_socket_would_block(error) || ku_http_socket_error_interrupted(error)) return;
    ku_http_connection_status(connection, server, 408); return;
  }
  if (!received) { ku_http_connection_status(connection, server, 408); return; }
  connection->body_received += (size_t)received;
  if (connection->body_received == connection->input.body_len) ku_http_connection_submit(connection, server, queue, workers);
}
static void ku_http_connection_write_limited(KuHttpConnection* connection, KuHttpServer* server,
    int turn_limit,size_t byte_limit) {
  KuHttpOutput* output = &connection->output;
  size_t bytes = 0;
  /* Each readiness turn is finite even for a peer that remains writable and a
     very large Ku response. Neither partial sends nor EINTR refresh write D. */
  for (int turn = 0; turn < turn_limit && bytes < byte_limit; turn++) {
    if (ku_http_now_ms() >= connection->deadline) {
      ku_http_count_transport_close(connection,KU_HTTP_CLOSE_WRITE_DEADLINE);
      ku_http_connection_close(connection); return;
    }
    if (output->part >= output->count) { ku_http_connection_next_request(connection, server); return; }
    size_t remaining=byte_limit-bytes;
    int sent=ku_http_output_send(connection->socket_value,output,remaining);
    if (sent < 0) {
      int error = ku_http_socket_last_error();
      if (ku_http_socket_error_interrupted(error)) continue;
      if (ku_http_socket_would_block(error)) return;
      ku_http_count_transport_close(connection,KU_HTTP_CLOSE_PEER_WRITE);
      ku_http_connection_close(connection); return;
    }
    if (!sent) {
      ku_http_count_transport_close(connection,KU_HTTP_CLOSE_PEER_WRITE);
      ku_http_connection_close(connection); return;
    }
    bytes += (size_t)sent;
    size_t advanced=(size_t)sent;
    while (advanced && output->part<output->count) {
      size_t left=output->parts[output->part].length-output->offset;
      if (advanced<left) { output->offset+=advanced; advanced=0; }
      else { advanced-=left; output->part++; output->offset=0; }
    }
  }
  if (output->part >= output->count) ku_http_connection_next_request(connection, server);
}
static void ku_http_connection_write(KuHttpConnection* connection, KuHttpServer* server) {
  ku_http_connection_write_limited(connection,server,32,(size_t)262144);
}
static void ku_http_connection_observe_job(KuHttpConnection* connection, KuHttpServer* server) {
  if (ku_http_async_observe(connection, server)) return;
  if (connection->phase == KU_HTTP_IO_REUSE_WAIT) {
    ku_http_connection_next_request(connection, server); return;
  }
  if (connection->phase == KU_HTTP_IO_CLOSED_JOB) {
    if (ku_http_job_only_owner(connection->job)) ku_http_connection_close(connection);
    return;
  }
  if (connection->phase != KU_HTTP_IO_JOB) return;
  KuHttpJob* job = connection->job;
  KuStruct___ku_http_response response = {0};
  size_t response_charge = 0;
  ku_http_mutex_lock(&job->lock);
  int completed = job->completed;
  int cancelled = job->cancelled;
  if (completed && !cancelled) {
    response = job->response; response_charge = job->response_charge;
    connection->failure=job->failure;
    job->response = (KuStruct___ku_http_response){0}; job->response_charge = 0;
  }
  ku_http_mutex_unlock(&job->lock);
  if (!completed && ku_http_now_ms() >= job->request_deadline) {
    ku_http_queue_cancel_job(job);
    ku_http_connection_status(connection,server,504);
    return;
  }
  if (!completed) return;
  if (cancelled) ku_http_connection_close(connection);
  else ku_http_connection_response(connection, server, response, response_charge, 1);
}
static void ku_http_reject_start(KuHttpConnection* connection, KuHttpSocket socket_value) {
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  struct KuHttpReactor* reactor=connection->reactor;
#endif
  memset(connection, 0, sizeof(*connection));
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  connection->reactor=reactor;connection->work_position=-1;connection->idle_position=-1;
#endif
  connection->socket_value = socket_value;
  connection->phase = KU_HTTP_IO_REJECT;
  connection->failure.reason = KU_HTTP_ERROR_QUEUE;
  connection->deadline = ku_http_deadline_ms(50);
}
static void ku_http_reject_read(KuHttpConnection* connection, KuHttpServer* server) {
  char incoming[1024];
  for (int turn = 0; turn < 8; turn++) {
    if (ku_http_now_ms() >= connection->deadline) { ku_http_connection_status(connection, server, 503); return; }
    int received = ku_http_socket_recv(connection->socket_value, incoming, sizeof(incoming));
    if (received < 0) {
      int error = ku_http_socket_last_error();
      if (ku_http_socket_would_block(error)) return;
      if (ku_http_socket_error_interrupted(error)) continue;
      ku_http_connection_status(connection, server, 503); return;
    }
    if (!received) { ku_http_connection_status(connection, server, 503); return; }
    connection->rejected_bytes += (size_t)received;
    long long max_header = server->max_header_bytes > 0 ? server->max_header_bytes : (16 * 1024);
    int done = connection->rejected_bytes >= (size_t)max_header;
    for (int index = 0; index < received && !done; index++) {
      unsigned char* window = connection->rejected_window;
      window[0] = window[1]; window[1] = window[2]; window[2] = window[3]; window[3] = (unsigned char)incoming[index];
      connection->rejected_seen++;
      if (connection->rejected_seen >= 4 && window[0] == '\r' && window[1] == '\n' && window[2] == '\r' && window[3] == '\n') done = 1;
      else if (connection->rejected_seen >= 2 && window[2] == '\n' && window[3] == '\n') done = 1;
    }
    if (done) { ku_http_connection_status(connection, server, 503); return; }
  }
}
/* One indexed P FIFO owns either a raw accepted fd or one existing request
 * identity. Existing-connection nodes never own/close that connection's fd.
 * Doubly linked fixed nodes allow middle cancellation to refund P once. */
enum { KU_HTTP_PENDING_FREE=0,KU_HTTP_PENDING_RAW=1,KU_HTTP_PENDING_REQUEST=2 };
typedef struct {
  int kind,previous,next,slot,poll_index,probe_only;
  KuHttpSocket socket_value;
  uint64_t queue_generation,connection_generation,request_sequence;
  unsigned long long deadline,probe_deadline;
} KuHttpPendingItem;
typedef struct KuHttpPending {
  KuHttpPendingItem* items;
  int cap,head,tail,free_head,count,raw_count;
  uint64_t generation;
  KuHttpActiveSet* active;
} KuHttpPending;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
static void ku_http_reactor_pending_forget(KuHttpActiveSet*,int);
static void ku_http_reactor_refresh(KuHttpConnection*);
#endif
static void ku_http_connection_observe_peer(KuHttpConnection*,short,int);
static void ku_http_pending_init(KuHttpPending* pending,KuHttpActiveSet* active) {
  pending->head=pending->tail=-1;pending->free_head=pending->cap?0:-1;pending->active=active;
  active->pending=pending;
  for(int i=0;i<pending->cap;i++) pending->items[i].next=i+1<pending->cap?i+1:-1;
}
static int ku_http_pending_append(KuHttpPending* pending) {
  if(pending->count>=pending->cap || pending->free_head<0 || pending->generation==UINT64_MAX) return -1;
  int index=pending->free_head;KuHttpPendingItem* item=&pending->items[index];
  pending->free_head=item->next;
  *item=(KuHttpPendingItem){.previous=pending->tail,.next=-1,.slot=-1,.poll_index=-1,
      .socket_value=KU_HTTP_INVALID_SOCKET,.queue_generation=++pending->generation};
  if(pending->tail>=0) pending->items[pending->tail].next=index;else pending->head=index;
  pending->tail=index;pending->count++;return index;
}
static void ku_http_pending_unlink(KuHttpPending* pending,int index) {
  if(index<0 || index>=pending->cap || pending->items[index].kind==KU_HTTP_PENDING_FREE) {
    fputs("http pending identity refund invariant failed\n",stderr);exit(1);
  }
  KuHttpPendingItem* item=&pending->items[index];
  if(item->kind==KU_HTTP_PENDING_RAW) {
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    ku_http_reactor_pending_forget(pending->active,index);
#endif
    pending->raw_count--;
  }
  if(item->previous>=0) pending->items[item->previous].next=item->next;else pending->head=item->next;
  if(item->next>=0) pending->items[item->next].previous=item->previous;else pending->tail=item->previous;
  *item=(KuHttpPendingItem){.kind=KU_HTTP_PENDING_FREE,.next=pending->free_head};
  pending->free_head=index;pending->count--;
}
static KuHttpConnection* ku_http_pending_connection(KuHttpPending* pending,int index) {
  if(index<0 || index>=pending->cap) return NULL;
  const KuHttpPendingItem* item=&pending->items[index];
  if(item->kind!=KU_HTTP_PENDING_REQUEST || item->slot<0 || item->slot>=pending->active->high_water) return NULL;
  KuHttpConnection* connection=&pending->active->connections[item->slot];
  if(connection->active_set!=pending->active || connection->active_position<0
      || !connection->request_queued || connection->pending_index!=index
      || connection->connection_generation!=item->connection_generation
      || connection->request_sequence!=item->request_sequence) return NULL;
  return connection;
}
static void ku_http_request_cancel_pending(KuHttpConnection* connection) {
  if(!connection->request_queued) return;
  KuHttpPending* pending=connection->active_set?connection->active_set->pending:NULL;
  if(pending && ku_http_pending_connection(pending,connection->pending_index)==connection)
    ku_http_pending_unlink(pending,connection->pending_index);
  connection->request_queued=0;connection->pending_index=-1;connection->request_wait_probe_deadline=0;
}
static void ku_http_pending_bind_request(KuHttpPending* pending,int index,KuHttpConnection* connection) {
  KuHttpPendingItem* item=&pending->items[index];
  item->kind=KU_HTTP_PENDING_REQUEST;item->socket_value=KU_HTTP_INVALID_SOCKET;
  item->slot=(int)(connection-pending->active->connections);
  item->connection_generation=connection->connection_generation;item->request_sequence=connection->request_sequence;
  connection->request_queued=1;connection->pending_index=index;connection->phase=KU_HTTP_IO_REQUEST_WAIT;
#if defined(__APPLE__)
  connection->request_wait_probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
#endif
}
static int ku_http_pending_push_request(KuHttpPending* pending,KuHttpConnection* connection) {
  if(connection->request_queued) return 1;
  int index=ku_http_pending_append(pending);if(index<0) return 0;
  pending->items[index].deadline=connection->request_deadline<connection->deadline?connection->request_deadline:connection->deadline;
  ku_http_pending_bind_request(pending,index,connection);return 1;
}
static int ku_http_pending_push_raw(KuHttpPending* pending,KuHttpSocket fd,unsigned long long deadline) {
  int index=ku_http_pending_append(pending);if(index<0) return 0;
  KuHttpPendingItem* item=&pending->items[index];
  item->kind=KU_HTTP_PENDING_RAW;item->socket_value=fd;item->deadline=deadline;pending->raw_count++;
#if defined(__APPLE__)
  item->probe_only=1;item->probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
#endif
  return 1;
}
/* A single nonconsuming observation establishes the first-byte deadline.
 * Queuing and later A transfer never refresh either original absolute D. */
static int ku_http_request_observe_start(KuHttpConnection* connection,KuHttpServer* server) {
  int available=connection->carry_length!=0;
  if(!available) {
    char byte;int count=recv(connection->socket_value,&byte,1,MSG_PEEK);
    if(count<0) {
      int error=ku_http_socket_last_error();
      if(ku_http_socket_would_block(error) || ku_http_socket_error_interrupted(error)) return 0;
      ku_http_connection_close(connection);return 0;
    }
    if(!count) {
      if(connection->reused) ku_http_connection_close(connection);
      else ku_http_connection_status(connection,server,400);
      return 0;
    }
  }
  if(!connection->header_started) {
    connection->header_started=1;
    if(!connection->request_deadline) connection->request_deadline=ku_http_deadline_ms(server->request_timeout_ms);
    connection->deadline=ku_http_deadline_ms((uint32_t)(server->read_header_timeout_ms>0?server->read_header_timeout_ms:5000));
    if(connection->request_deadline<connection->deadline) connection->deadline=connection->request_deadline;
  }
  return 1;
}
static int ku_http_request_prepare(KuHttpConnection* connection,KuHttpServer* server) {
  if(connection->management || !connection->active_set || connection->request_lease) return 1;
  if(connection->request_queued) return 0;
  if(connection->reused && !ku_http_reused_request_admit()) {ku_http_connection_close(connection);return 0;}
  if(!ku_http_request_observe_start(connection,server)) return 0;
  KuHttpPending* pending=connection->active_set->pending;
  if(!pending->count && ku_http_request_lease_take(connection)) return 1;
  if(!ku_http_pending_push_request(pending,connection)) {
    connection->failure.reason=KU_HTTP_ERROR_QUEUE;ku_http_connection_status(connection,server,503);
  }
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  ku_http_reactor_refresh(connection);
#endif
  return 0;
}
static void ku_http_pending_raw_close(KuHttpPending* pending,int index) {
  KuHttpSocket fd=pending->items[index].socket_value;
  ku_http_pending_unlink(pending,index);ku_http_socket_close(fd);ku_http_atomic_decrement(&ku_http_active_conns);
}
static int ku_http_pending_peer_error(KuHttpSocket fd) {
  int error=0;
#if defined(_WIN32)
  int length=(int)sizeof(error);
  if(getsockopt(fd,SOL_SOCKET,SO_ERROR,(char*)&error,&length)) error=ku_http_socket_last_error();
#else
  socklen_t length=(socklen_t)sizeof(error);
  if(getsockopt(fd,SOL_SOCKET,SO_ERROR,&error,&length)) error=ku_http_socket_last_error();
#endif
  return error && !ku_http_socket_would_block(error) && !ku_http_socket_error_interrupted(error);
}
static unsigned long long ku_http_pending_maintain(KuHttpPending* pending,KuHttpServer* server,unsigned long long now) {
  unsigned long long nearest=UINT64_MAX;
  for(int index=pending->head;index>=0;) {
    KuHttpPendingItem item=pending->items[index];int next=item.next;
    if(item.kind==KU_HTTP_PENDING_REQUEST) {
      KuHttpConnection* connection=ku_http_pending_connection(pending,index);
      if(!connection) {ku_http_pending_unlink(pending,index);index=next;continue;}
      if(now>=item.deadline) {ku_http_connection_status(connection,server,408);index=next;continue;}
      if(connection->request_wait_probe_deadline && now>=connection->request_wait_probe_deadline) {
        connection->request_wait_probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
        ku_http_connection_observe_peer(connection,0,1);
        if(!connection->request_queued) {index=next;continue;}
      }
      if(connection->request_wait_probe_deadline && connection->request_wait_probe_deadline<nearest)
        nearest=connection->request_wait_probe_deadline;
    } else if(item.kind==KU_HTTP_PENDING_RAW) {
      if(now>=item.deadline || (item.probe_deadline && now>=item.probe_deadline && ku_http_pending_peer_error(item.socket_value))) {
        ku_http_pending_raw_close(pending,index);index=next;continue;
      }
      if(item.probe_deadline && now>=item.probe_deadline)
        pending->items[index].probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
      if(pending->items[index].probe_deadline && pending->items[index].probe_deadline<nearest)
        nearest=pending->items[index].probe_deadline;
    }
    if(item.deadline<nearest) nearest=item.deadline;
    index=next;
  }
  return nearest;
}
static int ku_http_pending_activate(KuHttpPending* pending,KuHttpActiveSet* active,KuHttpServer* server) {
  int revisit=0;
  for(int turn=0;turn<64 && pending->count;turn++) {
    int index=pending->head;KuHttpPendingItem item=pending->items[index];
    if(item.kind==KU_HTTP_PENDING_RAW) {
      int slot=ku_http_free_slot(active);if(slot<0) break;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
      ku_http_reactor_pending_forget(active,index);
#endif
      if(ku_http_now_ms()>=item.deadline) {ku_http_pending_raw_close(pending,index);continue;}
      pending->raw_count--;
      ku_http_connection_start(&active->connections[slot],active,item.socket_value,server);
      KuHttpConnection* connection=&active->connections[slot];
      connection->request_deadline=item.deadline;
      if(item.deadline<connection->deadline) connection->deadline=item.deadline;
      ku_http_pending_bind_request(pending,index,connection);
      connection->phase=KU_HTTP_IO_HEADER;
      if(!ku_http_request_observe_start(connection,server)) {
        if(connection->request_queued) ku_http_request_cancel_pending(connection);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
        ku_http_reactor_refresh(connection);
#endif
        revisit=1;continue;
      }
      connection->phase=KU_HTTP_IO_REQUEST_WAIT;
      if(connection->deadline<pending->items[index].deadline) pending->items[index].deadline=connection->deadline;
    }
    KuHttpConnection* connection=ku_http_pending_connection(pending,index);
    if(!connection) {ku_http_pending_unlink(pending,index);continue;}
    unsigned long long now=ku_http_now_ms();
    if(now>=pending->items[index].deadline) {ku_http_connection_status(connection,server,408);revisit=1;continue;}
    if(!ku_http_request_lease_take(connection)) break;
    ku_http_pending_unlink(pending,index);
    connection->request_queued=0;connection->pending_index=-1;connection->request_wait_probe_deadline=0;
    connection->phase=KU_HTTP_IO_HEADER;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    ku_http_reactor_refresh(connection);
#endif
    if(connection->carry_length) revisit=1;
  }
  return revisit;
}
/* Waiting requests never subscribe to READ: queued bytes are retained without
 * level-triggered spinning. A legal FIN is kept; after HUP use bounded error
 * probes so a later RST cannot hide behind the retained pipeline bytes. */
static void ku_http_pending_ready(KuHttpPending* pending,int index,short events) {
  KuHttpPendingItem* item=&pending->items[index];
  if(item->kind!=KU_HTTP_PENDING_RAW || !events) return;
  if((events&POLLNVAL) || ku_http_pending_peer_error(item->socket_value)) {
    ku_http_pending_raw_close(pending,index);return;
  }
  if(events&(POLLERR|POLLHUP)) {
    char byte;int received=recv(item->socket_value,&byte,1,MSG_PEEK);
    if(!received || (received<0 && !ku_http_socket_would_block(ku_http_socket_last_error())
        && !ku_http_socket_error_interrupted(ku_http_socket_last_error()))) {
      ku_http_pending_raw_close(pending,index);return;
    }
    item->probe_only=1;item->probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    ku_http_reactor_pending_forget(pending->active,index);
#endif
  }
}
static void ku_http_pending_close(KuHttpPending* pending) {
  while(pending->count) {
    int index=pending->head;
    if(pending->items[index].kind==KU_HTTP_PENDING_RAW) ku_http_pending_raw_close(pending,index);
    else {
      KuHttpConnection* connection=ku_http_pending_connection(pending,index);
      if(connection) ku_http_connection_close(connection);else ku_http_pending_unlink(pending,index);
    }
  }
}
static void ku_http_poll_add(KuHttpPollFd* items, int* count, KuHttpSocket socket_value, short events) {
  KuHttpPollFd* item = &items[(*count)++];
  memset(item, 0, sizeof(*item));
  item->fd = socket_value; item->events = events;
}

static int ku_http_parse_ipv4(const char* text, unsigned long* network_order) {
  unsigned long octets[4] = {0, 0, 0, 0};
  size_t part = 0;
  const char* p = text;
  while (part < 4) {
    if (*p < '0' || *p > '9') return 0;
    unsigned long value = 0;
    size_t digits = 0;
    while (*p >= '0' && *p <= '9') {
      value = value * 10UL + (unsigned long)(*p - '0');
      if (value > 255UL || ++digits > 3) return 0;
      p++;
    }
    octets[part++] = value;
    if (part < 4) {
      if (*p != '.') return 0;
      p++;
    }
  }
  if (*p != '\0') return 0;
  unsigned long host_order = (octets[0] << 24) | (octets[1] << 16) |
                             (octets[2] << 8) | octets[3];
  *network_order = htonl(host_order);
  return 1;
}

#define KU_HTTP_MAX_LOOPS 32
/* All loop storage survives until every socket/job owner and OS thread retires.
 * Only the caller owns/closes the shared nonblocking listener. The current
 * accept turn belongs to one single-threaded owner until it publishes success;
 * reads, writes, parser state and completion never move between loops. */
typedef struct KuHttpLoopGroup KuHttpLoopGroup;
typedef struct KuHttpLoopOwner {
  KuHttpLoopGroup* group;
  int index, slots, request_limit, pending, workers, ready, exited, drained;
  unsigned long long heartbeat, accepted, requests, admitted, success, rejected, timed_out, other_responses, peak_loop_delay_ms;
  KuHttpConnection* connection_base; uint64_t connection_generation;
  uint64_t cpu_ns, cpu_sample_ms, cpu_clock_errors; /* Published under group->lock. */
  unsigned long long transport_closes[KU_HTTP_CLOSE_REASON_COUNT];
  size_t resident, queued, running, connections, pending_count, queued_bytes, peak_queued_bytes;
  size_t active_requests,peak_active_requests;
  KuHttpWake* wake;
  int task_work_pending; /* Only this event owner reads/writes Task self-work. */
#if defined(_WIN32)
  HANDLE thread;
#else
  pthread_t thread;
#endif
} KuHttpLoopOwner;
struct KuHttpLoopGroup {
  KuHttpServer* server; KuHttpSocket listener;
  KuHttpMutex lock; KuHttpWake wake;
  int count, created, draining, started, accept_turn, tasks_closing, drain_notified;
  char control_token[129];
  void* tasks;
  unsigned long long accepted, max_requests, drain_deadline;
  uint32_t drain_ms;
  const char* failure;
  uint64_t error_counts[KU_HTTP_ERROR_COUNT],error_sample_count,error_samples_dropped,http_500_total,unknown_500;
  struct {
    KuHttpFailure failure;
    uint64_t owner,request_sequence,connection_slot,connection_generation,at_ms;
    unsigned char request_id[48]; uint32_t request_id_length,request_id_truncated;
    int phase,status;
  } error_samples[8];
  KuHttpLoopOwner loops[KU_HTTP_MAX_LOOPS];
};
/* Called only by this owner's event thread with group->lock held. A failed or
   backwards clock sample keeps the last valid CPU counter and marks the error. */
static void ku_http_owner_cpu_sample(KuHttpLoopOwner* owner,uint64_t cpu_ns,uint64_t sampled_ms,int valid) {
  if (!valid || cpu_ns<owner->cpu_ns) {
    if (owner->cpu_clock_errors!=UINT64_MAX) owner->cpu_clock_errors++;
    return;
  }
  owner->cpu_ns=cpu_ns; owner->cpu_sample_ms=sampled_ms;
}
static KuHttpLoopGroup* ku_http_live_group;
static KU_THREAD_LOCAL KuHttpLoopOwner* ku_http_current_loop;
static int ku_http_queue_is_current_owner(const KuHttpQueue* queue) {
  return ku_http_current_loop && ku_http_current_loop->wake==queue->wake;
}
static void ku_http_connection_identity(KuHttpConnection* connection) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  connection->connection_slot=UINT64_MAX;
  if(!owner) return;
  uintptr_t first=(uintptr_t)owner->connection_base,at=(uintptr_t)connection;
  if(first && at>=first && at-first<(size_t)owner->slots*sizeof(*connection)
      && !((at-first)%sizeof(*connection))) connection->connection_slot=(at-first)/sizeof(*connection);
  if(owner->connection_generation!=UINT64_MAX) owner->connection_generation++;
  connection->connection_generation=owner->connection_generation;
}
static void ku_http_record_failure(KuHttpConnection* connection,int status) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  if(!owner || connection->management) return;
  uint32_t reason=connection->failure.reason;
  if(reason>=KU_HTTP_ERROR_COUNT) reason=KU_HTTP_ERROR_UNKNOWN;
  KuHttpLoopGroup* group=owner->group;
  ku_http_mutex_lock(&group->lock);
  if(group->error_counts[reason]!=UINT64_MAX) group->error_counts[reason]++;
  if(status==500) {
    if(group->http_500_total!=UINT64_MAX) group->http_500_total++;
    if(reason==KU_HTTP_ERROR_UNKNOWN && group->unknown_500!=UINT64_MAX) group->unknown_500++;
  }
  /* Do not let ordinary 503/deadline pressure consume the bounded evidence
     for internal failures. A new 500 may replace an older peer-reset sample. */
  size_t sample=8u;
  int eligible=status==500 || reason==KU_HTTP_ERROR_PEER_RESET;
  if(eligible && group->error_sample_count<8u) sample=(size_t)group->error_sample_count++;
  else if(status==500) for(size_t i=0;i<8u;i++) if(group->error_samples[i].status!=500) {
    sample=i; if(group->error_samples_dropped!=UINT64_MAX) group->error_samples_dropped++; break;
  }
  if(sample<8u) {
    size_t i=sample;
    group->error_samples[i].failure=connection->failure;
    group->error_samples[i].failure.reason=reason;
    group->error_samples[i].owner=(uint64_t)owner->index;
    group->error_samples[i].request_sequence=connection->request_sequence;
    group->error_samples[i].connection_slot=connection->connection_slot;
    group->error_samples[i].connection_generation=connection->connection_generation;
    group->error_samples[i].at_ms=ku_http_now_ms();
    group->error_samples[i].phase=connection->phase; group->error_samples[i].status=status;
    memcpy(group->error_samples[i].request_id,connection->request_id,sizeof(connection->request_id));
    group->error_samples[i].request_id_length=connection->request_id_length;
    group->error_samples[i].request_id_truncated=connection->request_id_truncated;
  } else if(eligible && group->error_samples_dropped!=UINT64_MAX) group->error_samples_dropped++;
  ku_http_mutex_unlock(&group->lock);
}
/* Branch witnesses, not request IDs or proof that a client observed this close.
   Counts are fixed storage and publication never performs diagnostic I/O.
   response_policy includes explicit Connection:close, error responses and
   pressure's response-boundary reuse decision; it is not synonymous with 503. */
static void ku_http_count_transport_close(KuHttpConnection* connection,int reason) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  if (!owner || connection->socket_value==KU_HTTP_INVALID_SOCKET
      || reason<0 || reason>=KU_HTTP_CLOSE_REASON_COUNT) return;
  ku_http_mutex_lock(&owner->group->lock);
  if (owner->transport_closes[reason]!=~0ULL) owner->transport_closes[reason]++;
  ku_http_mutex_unlock(&owner->group->lock);
}
static int ku_http_reused_request_admit(void) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  if (!owner) return 1;
  ku_http_mutex_lock(&owner->group->lock);
  int admitted=!owner->group->draining && !owner->group->failure;
  ku_http_mutex_unlock(&owner->group->lock);
  return admitted;
}
static void ku_http_count_request(void) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  if (!owner) return;
  ku_http_mutex_lock(&owner->group->lock); owner->requests++; ku_http_mutex_unlock(&owner->group->lock);
}
static void ku_http_count_admitted(void) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  if (!owner) return;
  ku_http_mutex_lock(&owner->group->lock); owner->admitted++; ku_http_mutex_unlock(&owner->group->lock);
}
static void ku_http_count_response(int status) {
  KuHttpLoopOwner* owner=ku_http_current_loop;
  if (!owner) return;
  ku_http_mutex_lock(&owner->group->lock);
  if (status>=200 && status<400) owner->success++;
  else if (status==503) owner->rejected++;
  else if (status==408 || status==504) owner->timed_out++;
  else owner->other_responses++;
  ku_http_mutex_unlock(&owner->group->lock);
}
static long ku_http_env_limit(const char* name, long fallback, long maximum) {
  const char* value = getenv(name);
  if (!value) return fallback;
  unsigned long result = 0;
  if (!*value) goto invalid;
  for (const char* p = value; *p; p++) {
    if (*p < '0' || *p > '9' || result > ((unsigned long)maximum - (unsigned long)(*p-'0')) / 10) goto invalid;
    result = result * 10 + (unsigned long)(*p-'0');
  }
  if (!result || result > (unsigned long)maximum) goto invalid;
  return (long)result;
invalid:
  fprintf(stderr, "%s must be in 1..%ld\n", name, maximum); exit(1);
}
static void ku_http_group_notify(KuHttpLoopGroup* group) {
  /* The lock protects wake-pointer lifetime against loop teardown. notify does
     not acquire this mutex and sends at most one nonblocking datagram. */
  ku_http_mutex_lock(&group->lock);
  for (int i=0; i<group->created; i++) if (group->loops[i].wake) ku_http_wake_notify(group->loops[i].wake);
  ku_http_mutex_unlock(&group->lock);
  ku_http_wake_notify(&group->wake);
}


#if defined(KU_HTTP_HAS_TASK_RUNTIME)
static int ku_http_task_take_owner_work(KuHttpLoopOwner* owner) {
  int pending=owner->task_work_pending;owner->task_work_pending=0;return pending;
}
static void ku_http_task_event_wake(void* context, size_t owner) {
  KuHttpLoopGroup* group = (KuHttpLoopGroup*)context;
  ku_http_mutex_lock(&group->lock);
  if (owner < (size_t)group->created && group->loops[owner].wake) {
    KuHttpLoopOwner* target=&group->loops[owner];
    if(ku_http_current_loop==target && ku_http_current_loop->group==group
        && ku_http_current_loop->index==(int)owner) target->task_work_pending=1;
    else ku_http_wake_notify(target->wake);
  }
  int notify_coordinator=group->draining || group->tasks_closing;
  ku_http_mutex_unlock(&group->lock);
  if (notify_coordinator) ku_http_wake_notify(&group->wake);
}
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
static uint32_t ku_http_task_start_failure_reason(uint32_t status);
static size_t ku_http_task_response_admit(void* context,KuStruct___ku_http_response* response,KuHttpFailure* failure) {
  KuHttpLoopGroup* group=(KuHttpLoopGroup*)context;
  return ku_http_buffer_admit_response(group->server->buffer_budget,response,failure);
}
#endif
static int ku_http_async_retire(KuHttpConnection* connection) {
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  if (!connection->async_task) return 1;
  connection->async_cancelled = 1;
  if (!connection->async_cleanup_deadline) connection->async_cleanup_deadline = ku_http_deadline_ms(KU_HANDLER_CLEANUP_GRACE_MS);
  KuStruct___ku_http_response unused = {0};
  size_t unused_charge=0;
  (void)ku_http_task_poll(ku_http_live_group->tasks,&connection->async_task,1,connection->async_cleanup_deadline,&unused,&unused_charge);
  ku_http_buffer_refund(connection->budget,unused_charge);
  ku_drop_struct___ku_http_response(&unused);
  return connection->async_task == NULL;
#else
  (void)connection; return 1;
#endif
}
static int ku_http_async_submit(KuHttpConnection* connection, KuHttpServer* server) {
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  KuHttpInput* input = &connection->input;
  const char* method = input->method;
  const char* target = input->target;
  size_t path_len = input->path_len;
  const char* body_buf = input->body;
  size_t body_len = input->body_len;
  const char* param_values[KU_HTTP_MAX_SEGS];size_t param_value_lens[KU_HTTP_MAX_SEGS],param_count=0;
  KuHttpHandler selected={0};
  int route_status=ku_http_input_select(server,input,&selected,param_values,param_value_lens,&param_count);
  if(route_status!=200 || selected.returns_result!=2)return 0;
  KuHttpHandler* route=&selected;
  KuStruct___ku_http_request req = {0};
  size_t projection_charge=0;
  if (route->arity == 1) {
    size_t parameter_names=0;
    for (size_t i=0;i<route->nparams;i++) parameter_names+=strlen(route->param_names[i]);
    projection_charge=ku_http_projection_bytes(input,route->nparams,parameter_names);
    if (!ku_http_buffer_reserve(connection->budget,projection_charge)) { connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection,server,503); return 1; }
    unsigned int fields=ku_http_input_fields(input);
    if(fields&2u) {
      req.params = ku_object_new(ku_http_map_capacity(route->nparams));
      for (size_t i=0; i<route->nparams; i++)
        ku_object_set(req.params,ku_http_string_copy(route->param_names[i],strlen(route->param_names[i])),
          ku_v_str(ku_http_string_copy(param_values[i],param_value_lens[i])));
    }
    if(fields&8u)req.method = ku_http_string_copy(method,strlen(method));
    if(fields&16u)req.path = ku_http_string_copy(target,path_len);
    if(fields&32u)req.body = ku_http_string_copy(body_buf ? body_buf : "",body_len);
    if(fields&4u)req.query = ku_http_parse_query(target,path_len);
    if(fields&1u) {req.headers = input->headers; input->headers = NULL;}
  }
  KuHttpLoopOwner* owner = ku_http_current_loop;
  /* One original handler deadline is carried through every PG wait/reset.
     Computing it before factory admission cannot renew it at first query. */
  connection->async_deadline = ku_http_deadline_ms((uint32_t)server->handler_timeout_ms);
  if (connection->request_deadline && connection->request_deadline < connection->async_deadline) connection->async_deadline = connection->request_deadline;
  uint32_t status = ((uint32_t(*)(void*,size_t,void*,uint64_t,KuStruct___ku_http_request*,void**))route->invoke)(
      owner->group->tasks,(size_t)owner->index,route->env,connection->async_deadline,&req,&connection->async_task);
  ku_drop_struct___ku_http_request(&req);
  ku_http_buffer_refund(connection->budget,projection_charge);
  ku_http_input_drop(input);
  if (status) {
    /* This transport is emitted before the private Task ABI declarations.
       The bridge classifies its own status constants below. */
    uint32_t reason=ku_http_task_start_failure_reason(status);
    connection->failure=(KuHttpFailure){.reason=reason,.driver_status=status,.stage=1};
    ku_http_connection_status(connection,server,503); return 1;
  }
  ku_http_count_admitted();
  connection->phase = KU_HTTP_IO_ASYNC;
  return 1;
#else
  (void)connection; (void)server; return 0;
#endif
}
static int ku_http_async_observe(KuHttpConnection* connection, KuHttpServer* server) {
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  if (!connection->async_task) return 0;
  if (connection->phase == KU_HTTP_IO_CLOSED_ASYNC) {
    if (ku_http_async_retire(connection)) ku_http_connection_close(connection);
    return 1;
  }
  if (!connection->async_cancelled && ku_http_now_ms() >= connection->async_deadline) {
    (void)ku_http_async_retire(connection);
    ku_http_connection_status(connection,server,504);
    return 1;
  }
  KuStruct___ku_http_response response = {0};
  size_t response_charge=0;
  uint32_t status = ku_http_task_poll(ku_http_live_group->tasks,&connection->async_task,
      connection->async_cancelled,connection->async_cancelled ? connection->async_cleanup_deadline : connection->async_deadline,&response,&response_charge);
  if (status == 1u) return connection->phase == KU_HTTP_IO_ASYNC;
  connection->failure=ku_http_failure_current;
  connection->failure.stage=3;
  if(status) { connection->failure.reason=KU_HTTP_ERROR_DRIVER; connection->failure.driver_status=status; }
  if (connection->async_cancelled) {
    ku_drop_struct___ku_http_response(&response);
    ku_http_buffer_refund(connection->budget,response_charge);
    return 0; /* Keep an already-published timeout frame intact. */
  }
  if (status) {
    ku_drop_struct___ku_http_response(&response);
    ku_http_buffer_refund(connection->budget,response_charge);
    ku_http_connection_status(connection,server,500);
  } else ku_http_connection_response(connection,server,response,response_charge,1);
  return 1;
#else
  (void)connection; (void)server; return 0;
#endif
}
/* Observe real readiness or an explicit Darwin probe while work owns the response.
 * FIN and queued
 * pipeline bytes are legal: pause READ after peeking either, but keep the
 * socket registered for errors. Buffered bytes can mask a later reset in
 * MSG_PEEK, so inspect SO_ERROR first on error/hangup readiness. A zero error
 * plus FIN is still a legal request write-half-close, not cancellation. The
 * ordinary close path retains the physical Task/job lease until retirement. */
static void ku_http_connection_observe_peer(KuHttpConnection* connection,short events,int force_error_check) {
  char byte;
  int received, error=0;
#if defined(__APPLE__)
  /* Darwin poll(events=0) can hide a reset behind queued bytes. Only these
     paused, live handlers receive a bounded fallback; this is not an OS event
     or a renewal of the request/handler deadline. Retry transient errors on
     the next finite probe rather than spinning on an already-due deadline. */
  if(force_error_check) connection->peer_error_probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
#endif
  if(force_error_check || (events & (POLLERR | POLLHUP))) {
#if defined(_WIN32)
    int length=(int)sizeof(error);
    if(getsockopt(connection->socket_value,SOL_SOCKET,SO_ERROR,(char*)&error,&length))
      error=ku_http_socket_last_error();
#else
    socklen_t length=(socklen_t)sizeof(error);
    if(getsockopt(connection->socket_value,SOL_SOCKET,SO_ERROR,&error,&length))
      error=ku_http_socket_last_error();
#endif
    if(error) {
      if(ku_http_socket_would_block(error) || ku_http_socket_error_interrupted(error)) return;
      goto peer_failed;
    }
  }
  if(force_error_check) return; /* SO_ERROR was read once; future bytes remain untouched. */
  received=recv(connection->socket_value,&byte,1,MSG_PEEK);
  if(received>=0) {
    connection->peer_read_paused=1;
#if defined(__APPLE__)
    connection->peer_error_probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
#endif
    return;
  }
  error=ku_http_socket_last_error();
  if(ku_http_socket_would_block(error) || ku_http_socket_error_interrupted(error)) return;
peer_failed:
  connection->failure.reason=KU_HTTP_ERROR_PEER_RESET;
  connection->failure.stage=5;
  ku_http_record_failure(connection,0); /* No response can be delivered. */
  ku_http_connection_close(connection);
}
static int ku_http_control_request(KuHttpConnection* connection, KuHttpServer* server) {
  KuHttpLoopGroup* group = ku_http_live_group;
  if (!group || !group->control_token[0]) return 0;
  KuHttpInput* input = &connection->input;
  const char* prefix = "/__ku/";
  if (input->path_len < 6 || memcmp(input->target, prefix, 6)) return 0;
  KuString header=ku_http_input_control_token(input);
  size_t token_length = strlen(group->control_token);
  int match = header.len == token_length;
  unsigned char difference = 0;
  if (match) for (size_t i=0; i<token_length; i++) difference |= header.ptr[i] ^ (unsigned char)group->control_token[i];
  if (!match || difference) { ku_http_connection_status(connection, server, 403); return 1; }
  int metrics = !strcmp(input->method,"GET") && input->path_len == 13 && !memcmp(input->target,"/__ku/metrics",13);
  int drain = !strcmp(input->method,"POST") && input->path_len == 11 && !memcmp(input->target,"/__ku/drain",11);
  int startup = !strcmp(input->method,"GET") && input->path_len == 20 && !memcmp(input->target,"/__ku/health/startup",20);
  int ready = !strcmp(input->method,"GET") && input->path_len == 18 && !memcmp(input->target,"/__ku/health/ready",18);
  int live = !strcmp(input->method,"GET") && input->path_len == 17 && !memcmp(input->target,"/__ku/health/live",17);
  if (!drain && !startup && !ready && !live && !metrics) { ku_http_connection_status(connection,server,404); return 1; }
  if (metrics) {
    /* Fixed bound includes 32 owners and every counter at full uint64 width;
       it remains inside the reserved management channel's 64 KiB quota. */
    char text[57344]; size_t used = 0;
    size_t management_capacity=sizeof(text);
#if defined(KU_PG_DIAGNOSTICS)
    /* Preserve an explicit truncated PG witness even when other metrics fill
     * their allowed output. This reserve does not enlarge the 64 KiB channel. */
    management_capacity-=64u;
#endif
    ku_http_mutex_lock(&group->lock);
    int n = snprintf(text,management_capacity,"{\"limits\":{\"max_active_requests\":%lld,\"max_connections\":%lld,\"max_pending_requests\":%lld},\"loops\":[",
      (long long)server->max_active_requests,(long long)server->max_connections,(long long)server->max_pending_requests);
    if (n < 0) _Exit(79);
    used = (size_t)n;
    for (int i=0; i<group->created; i++) {
      KuHttpLoopOwner* owner = &group->loops[i];
      uint64_t sampled_now=ku_http_now_ms();
      uint64_t sample_age=sampled_now>=owner->cpu_sample_ms ? sampled_now-owner->cpu_sample_ms : 0;
      n = snprintf(text+used,management_capacity-used,"%s{\"id\":%d,\"accepted\":%llu,\"requests\":%llu,\"queued\":%llu,\"running\":%llu,\"resident\":%llu,\"connections\":%llu,\"pending\":%llu,\"active_requests\":%llu,\"peak_active_requests\":%llu,\"cpu_ns\":%llu,\"cpu_sample_ms\":%llu,\"cpu_sample_age_ms\":%llu,\"cpu_clock_errors\":%llu}",
        i?",":"",i,owner->accepted,owner->requests,(unsigned long long)owner->queued,(unsigned long long)owner->running,(unsigned long long)owner->resident,(unsigned long long)owner->connections,(unsigned long long)owner->pending_count,
        (unsigned long long)owner->active_requests,(unsigned long long)owner->peak_active_requests,
        (unsigned long long)owner->cpu_ns,(unsigned long long)owner->cpu_sample_ms,(unsigned long long)sample_age,(unsigned long long)owner->cpu_clock_errors);
      if (n<0 || (size_t)n>=management_capacity-used) _Exit(79);
      used+=(size_t)n;
    }
    n=snprintf(text+used,management_capacity-used,"],\"accepted\":%llu,\"draining\":%s",group->accepted,group->draining?"true":"false");
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    ku_http_mutex_unlock(&group->lock);
    n=snprintf(text+used,management_capacity-used,",\"outcomes\":[");
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    ku_http_mutex_lock(&group->lock);
    for (int i=0;i<group->created;i++) {
      KuHttpLoopOwner* owner=&group->loops[i];
      n=snprintf(text+used,management_capacity-used,"%s{\"id\":%d,\"admitted\":%llu,\"success_responses\":%llu,\"rejected_responses\":%llu,\"timeout_responses\":%llu,\"other_responses\":%llu,\"peak_loop_delay_ms\":%llu,\"queued_bytes\":%llu,\"peak_queued_bytes\":%llu}",i?",":"",i,
        owner->admitted,owner->success,owner->rejected,owner->timed_out,owner->other_responses,owner->peak_loop_delay_ms,(unsigned long long)owner->queued_bytes,(unsigned long long)owner->peak_queued_bytes);
      if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    }
    ku_http_mutex_unlock(&group->lock);
    text[used++]=']';
    n=snprintf(text+used,management_capacity-used,",\"transport_closes\":[");
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    ku_http_mutex_lock(&group->lock);
    for (int i=0;i<group->created;i++) {
      unsigned long long* counts=group->loops[i].transport_closes;
      n=snprintf(text+used,management_capacity-used,"%s{\"id\":%d,\"idle_pressure\":%llu,\"response_policy\":%llu,\"write_deadline\":%llu,\"peer_write_error\":%llu,\"queue_not_admitted\":%llu,\"drain_idle\":%llu}",
          i?",":"",i,counts[0],counts[1],counts[2],counts[3],counts[4],counts[5]);
      if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    }
    ku_http_mutex_unlock(&group->lock);
    text[used++]=']';
    {
      static const char* reasons[KU_HTTP_ERROR_COUNT]={"unknown","handler_result","handler_500","task_limit","task_closed","task_oom","driver_invariant","invalid_response","allocation","projection_budget","queue_admission","deadline","handler_exit","parser_invariant","peer_reset"};
      n=snprintf(text+used,management_capacity-used,",\"errors\":{\"sample_limit\":8,\"request_id_encoding\":\"hex\",\"counts\":{");
      if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
      ku_http_mutex_lock(&group->lock);
      for(size_t i=0;i<KU_HTTP_ERROR_COUNT;i++) {
        n=snprintf(text+used,management_capacity-used,"%s\"%s\":%llu",i?",":"",reasons[i],(unsigned long long)group->error_counts[i]);
        if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
      }
      n=snprintf(text+used,management_capacity-used,"},\"sample_policy\":\"500_and_peer_reset_500_priority\",\"dropped_samples\":%llu,\"http_500_total\":%llu,\"unknown_500\":%llu,\"samples\":[",(unsigned long long)group->error_samples_dropped,(unsigned long long)group->http_500_total,(unsigned long long)group->unknown_500);
      if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
      for(size_t i=0;i<group->error_sample_count;i++) {
        char rid[97]; static const char hex[]="0123456789abcdef";
        for(size_t j=0;j<group->error_samples[i].request_id_length;j++) {
          unsigned char c=group->error_samples[i].request_id[j]; rid[j*2]=hex[c>>4]; rid[j*2+1]=hex[c&15];
        }
        rid[group->error_samples[i].request_id_length*2]=0;
        KuHttpFailure* f=&group->error_samples[i].failure;
        n=snprintf(text+used,management_capacity-used,"%s{\"reason\":\"%s\",\"owner\":%llu,\"request_id\":\"%s\",\"request_id_truncated\":%s,\"request_sequence\":%llu,\"connection_slot\":%llu,\"connection_generation\":%llu,\"task_slot\":%llu,\"task_generation\":%llu,\"function_id\":%llu,\"phase\":%d,\"stage\":%u,\"driver_status\":%u,\"http_status\":%d,\"at_ms\":%llu}",i?",":"",reasons[f->reason],(unsigned long long)group->error_samples[i].owner,rid,group->error_samples[i].request_id_truncated?"true":"false",(unsigned long long)group->error_samples[i].request_sequence,(unsigned long long)group->error_samples[i].connection_slot,(unsigned long long)group->error_samples[i].connection_generation,(unsigned long long)f->task_slot,(unsigned long long)f->task_generation,(unsigned long long)f->function_id,group->error_samples[i].phase,f->stage,f->driver_status,group->error_samples[i].status,(unsigned long long)group->error_samples[i].at_ms);
        if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
      }
      ku_http_mutex_unlock(&group->lock);
      n=snprintf(text+used,management_capacity-used,"]}");
      if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    }
    ku_http_mutex_lock(&group->server->buffer_budget->lock);
    n=snprintf(text+used,management_capacity-used,",\"buffer_bytes\":%llu,\"peak_buffer_bytes\":%llu,\"buffer_rejections\":%llu",
      (unsigned long long)group->server->buffer_budget->current,(unsigned long long)group->server->buffer_budget->peak,(unsigned long long)group->server->buffer_budget->rejected);
    ku_http_mutex_unlock(&group->server->buffer_budget->lock);
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;

#if defined(KU_FEATURE_DATABASE_BUDGET)
    KuDatabaseBudgetSnapshot db = ku_database_budget_snapshot();
    n=snprintf(text+used,management_capacity-used,",\"database\":{\"reserved_connections\":%llu,\"reserved_waiters\":%llu,\"pending_operations\":%llu,\"pending_bytes\":%llu,\"peak_pending_bytes\":%llu,\"rejections\":%llu,\"unknown_results\":%llu,\"confirmed_cancellations\":%llu,\"pg_client_active_slots\":%llu,\"pg_client_waiters\":%llu,\"pg_client_peak_active_slots\":%llu,\"pg_client_peak_waiters\":%llu,\"pg_client_acquire_timeouts\":%llu,\"pg_client_pool_rejections\":%llu,\"pg_client_discarded_connections\":%llu,\"pg_client_broken_discards\":%llu,\"pg_client_metrics_valid\":%s}",
      (unsigned long long)db.reserved_connections,(unsigned long long)db.reserved_waiters,
      (unsigned long long)db.pending_operations,(unsigned long long)db.pending_bytes,(unsigned long long)db.peak_pending_bytes,
      (unsigned long long)db.rejections,(unsigned long long)db.unknown_results,(unsigned long long)db.confirmed_cancellations,
      (unsigned long long)db.pg_client_active_slots,(unsigned long long)db.pg_client_waiters,
      (unsigned long long)db.pg_client_peak_active_slots,(unsigned long long)db.pg_client_peak_waiters,
      (unsigned long long)db.pg_client_acquire_timeouts,(unsigned long long)db.pg_client_pool_rejections,
      (unsigned long long)db.pg_client_discarded_connections,(unsigned long long)db.pg_client_broken_discards,db.pg_client_metrics_valid ? "true" : "false");
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
#endif
#if defined(KU_FEATURE_SERVICE_OUTPUT)
    KuServiceOutputSnapshot output=ku_service_output_snapshot();
    n=snprintf(text+used,management_capacity-used,",\"output\":{\"queued_records\":%llu,\"queued_bytes\":%llu,\"peak_bytes\":%llu,\"dropped_records\":%llu,\"dropped_bytes\":%llu,\"failures\":%llu,\"written_bytes\":%llu}",
      (unsigned long long)output.queued_records,(unsigned long long)output.queued_bytes,(unsigned long long)output.peak_bytes,
      (unsigned long long)output.dropped_records,(unsigned long long)output.dropped_bytes,(unsigned long long)output.failures,(unsigned long long)output.written_bytes);
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    n=snprintf(text+used,management_capacity-used,",\"tasks\":[");
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    for (int i=0; i<group->count; i++) {
      uint64_t states[8]={0};
      /* Never hold group->lock across a driver API: driver publications call
         event_wake while holding their own mutex. */
      ku_http_task_metrics(group->tasks,(size_t)i,states);
      n=snprintf(text+used,management_capacity-used,"%s{\"id\":%d,\"ready\":%llu,\"waiting\":%llu,\"running\":%llu,\"resident\":%llu,\"timers\":%llu,\"polls\":%llu,\"peak_ready_delay_ms\":%llu,\"exited\":%llu}",i?",":"",i,
        (unsigned long long)states[0],(unsigned long long)states[1],(unsigned long long)states[2],(unsigned long long)states[3],
        (unsigned long long)states[4],(unsigned long long)states[5],(unsigned long long)states[6],(unsigned long long)states[7]);
      if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    }
    text[used++]=']';
    uint64_t driver_budget[9]={0};
    ku_http_task_driver_budget(group->tasks,driver_budget);
    n=snprintf(text+used,management_capacity-used,",\"task_driver\":{\"capacity\":%llu,\"resident\":%llu,\"peak_resident\":%llu,\"fixed_bytes\":%llu,\"reserved_bytes\":%llu,\"peak_reserved_bytes\":%llu,\"byte_limit\":%llu,\"admission_slot_rejections\":%llu,\"admission_byte_rejections\":%llu}",
      (unsigned long long)driver_budget[0],(unsigned long long)driver_budget[1],(unsigned long long)driver_budget[2],
      (unsigned long long)driver_budget[3],(unsigned long long)driver_budget[4],(unsigned long long)driver_budget[5],
      (unsigned long long)driver_budget[6],(unsigned long long)driver_budget[7],(unsigned long long)driver_budget[8]);
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    uint64_t driver_timing[10]={0};
    ku_http_task_driver_timing(group->tasks,driver_timing);
    n=snprintf(text+used,management_capacity-used,",\"task_driver_timing\":{\"enabled\":%s,\"lock_count\":%llu,\"lock_wait_ns\":%llu,\"max_lock_wait_ns\":%llu,\"lock_hold_ns\":%llu,\"max_lock_hold_ns\":%llu,\"condition_count\":%llu,\"condition_sleep_reacquire_ns\":%llu,\"max_condition_sleep_reacquire_ns\":%llu,\"clock_errors\":%llu}",
      driver_timing[0]?"true":"false",(unsigned long long)driver_timing[1],
      (unsigned long long)driver_timing[2],(unsigned long long)driver_timing[3],
      (unsigned long long)driver_timing[4],(unsigned long long)driver_timing[5],
      (unsigned long long)driver_timing[6],(unsigned long long)driver_timing[7],
      (unsigned long long)driver_timing[8],(unsigned long long)driver_timing[9]);
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
    uint64_t blocking_states[16]={0};
    ku_http_task_blocking_metrics(group->tasks,blocking_states);
    n=snprintf(text+used,management_capacity-used,",\"task_blocking\":{\"resident\":%llu,\"queued\":%llu,\"running\":%llu,\"peak_resident\":%llu,\"peak_queued\":%llu,\"peak_running\":%llu,\"accepted\":%llu,\"rejected\":%llu,\"finished\":%llu,\"output_charge_bytes\":%llu,\"workers_created\":%llu,\"workers_exited\":%llu,\"cancelled_still_running\":%llu,\"net_wait\":%llu,\"net_step\":%llu}",
      (unsigned long long)blocking_states[0],(unsigned long long)blocking_states[1],(unsigned long long)blocking_states[2],
      (unsigned long long)blocking_states[3],(unsigned long long)blocking_states[4],(unsigned long long)blocking_states[5],
      (unsigned long long)blocking_states[6],(unsigned long long)blocking_states[7],(unsigned long long)blocking_states[8],
      (unsigned long long)blocking_states[9],(unsigned long long)blocking_states[10],(unsigned long long)blocking_states[11],
      (unsigned long long)blocking_states[12],(unsigned long long)blocking_states[13],(unsigned long long)blocking_states[14]);
    if(n<0 || (size_t)n>=management_capacity-used) _Exit(79); used+=(size_t)n;
#endif
#if defined(KU_PG_DIAGNOSTICS)
    /* PG failures are published once into fixed storage. Export only through
     * this existing authenticated management request; ordinary HTTP never
     * formats or writes diagnostics. Reserve the final JSON delimiter/NUL. */
    {
      size_t pg_bytes=ku_pg_diagnostics_json(text+used+1u,sizeof(text)-used-2u);
      if(pg_bytes) {text[used++]=',';used+=pg_bytes;}
      else {
        n=snprintf(text+used,sizeof(text)-used,",\"pg_acquire_failures\":{\"truncated\":true}");
        if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79);used+=(size_t)n;
      }
    }
#endif
    text[used++]='}'; text[used]=0;
    KuStruct___ku_http_response response = ku_http_status_response(200,"");
    response.body=ku_http_string_copy(text,used);
    response.content_type=ku_string_static((const uint8_t*)"application/json",16);
    connection->keep_alive=0;
    ku_http_connection_response(connection,server,response,0,0);
    return 1;
  }
  ku_http_mutex_lock(&group->lock);
  int healthy = group->started && !group->failure;
  unsigned long long now = ku_http_now_ms();
  for (int i=0; i<group->created; i++) {
    if (!group->loops[i].ready || now < group->loops[i].heartbeat || now-group->loops[i].heartbeat > 2000) healthy = 0;
  }
  if (ready && group->draining) healthy = 0;
  if (drain && !group->draining) { group->draining = 1; group->drain_deadline = ku_http_deadline_ms(group->drain_ms); }
  ku_http_mutex_unlock(&group->lock);
  connection->keep_alive = 0;
  KuStruct___ku_http_response response = ku_http_status_response((drain || healthy) ? 200 : 503, group->control_token);
  ku_http_connection_response(connection, server, response, 0, 1);
  if (drain) ku_http_group_notify(group);
  return 1;
}
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
/* Bounded HTTP Task/I/O batch. These are owner-local bookkeeping fields, not
 * a Task budget or a cached deadline clock. Every turn still calls pump(32),
 * and each logical poll retains its original 64-dispatch/phase protocol.
 * At most eight turns (256 logical polls) may share one immediate OS probe;
 * a fresh monotonic sample must remain in the starting millisecond. Thus a
 * stalled/backwards clock cannot create an unbounded loop. Any ordinary OS
 * poll, urgent I/O, real wait, shutdown or fault ends this batch completely. */
#define KU_HTTP_TASK_IO_BATCH_TURNS 8u
typedef struct {
  unsigned long long started_ms;
  unsigned int turns;
  int probe_clean;
  uint64_t probe_generation;
  int probe_count, probe_accept_turn;
} KuHttpTaskIoBatch;
/* A slice owes one real I/O turn, including when its own ready work did not
 * publish a wake byte. This owner-local obligation survives EINTR and every
 * early maintenance continue; it is not a reusable "no readiness" fact. */
typedef struct { int force_io; uint64_t next_deadline; } KuHttpTaskSliceTurn;
static int ku_http_task_slice_needs_pump(const KuHttpTaskSliceTurn* turn,uint64_t* deadline) {
  if (!turn->force_io) return 1;
  *deadline=turn->next_deadline; return 0;
}
static int ku_http_task_slice_wait_ms(const KuHttpTaskSliceTurn* turn,int wait_ms) {
  return turn->force_io ? 0 : wait_ms;
}
static void ku_http_task_slice_observed_io(KuHttpTaskSliceTurn* turn,int selected) {
  if (selected>=0) turn->force_io=0;
}
/* Rebuilt by this turn's existing poll-interest traversal. No connection or
 * generation fact survives admission, I/O dispatch or the next owner turn. */
typedef struct {
  unsigned long long deadline;
  int all_async;
  int all_batchable, has_idle;
} KuHttpTaskIoEligibility;
static void ku_http_task_io_eligibility_observe(KuHttpTaskIoEligibility* eligibility,
    int eligible,unsigned long long deadline) {
  if (!eligible) eligibility->all_async=0;
  else if (deadline<eligibility->deadline) eligibility->deadline=deadline;
}
/* Isolated candidate: ordinary first-use/partial reads never reuse a probe. */
static int ku_http_task_io_idle_eligible(const KuHttpConnection* connection) {
  return connection->phase==KU_HTTP_IO_HEADER && connection->reused
      && !connection->buffered && !connection->carry_length
      && !connection->job && !connection->async_task
      && !connection->request_deadline
      && connection->deadline!=0 && connection->deadline!=~0ULL
      && connection->socket_value!=KU_HTTP_INVALID_SOCKET;
}
static void ku_http_task_io_eligibility_observe_idle(KuHttpTaskIoEligibility* eligibility,
    int async_eligible,int idle_eligible,unsigned long long deadline) {
  ku_http_task_io_eligibility_observe(eligibility,async_eligible,deadline);
  if (!async_eligible && !idle_eligible) eligibility->all_batchable=0;
  if (idle_eligible) eligibility->has_idle=1;
  if ((async_eligible || idle_eligible) && deadline<eligibility->deadline)
    eligibility->deadline=deadline;
}
static void ku_http_task_io_batch_reset(KuHttpTaskIoBatch* batch) {
  batch->started_ms=0; batch->turns=0; batch->probe_clean=0;
  batch->probe_generation=0; batch->probe_count=0; batch->probe_accept_turn=0;
}
static int ku_http_task_io_batch_probe_matches(const KuHttpTaskIoBatch* batch,
    uint64_t generation,int count,int accept_turn) {
  return batch->probe_clean && generation!=UINT64_MAX && count>0
      && batch->probe_generation==generation && batch->probe_count==count
      && batch->probe_accept_turn==accept_turn;
}
/* Called only after continue() reset the batch before a real OS poll. The
 * pre-poll clock and this turn remain charged: the next begin cannot renew
 * an old zero-readiness observation after a stall or a clock tick. */
static void ku_http_task_io_batch_probe_result(KuHttpTaskIoBatch* batch,
    int wait_ms,int selected,int can_bootstrap,unsigned long long pre_poll_ms,
    uint64_t generation,int count,int accept_turn) {
  if (wait_ms!=0 || selected!=0 || !can_bootstrap || pre_poll_ms==~0ULL
      || generation==UINT64_MAX || count<=0) return;
  batch->started_ms=pre_poll_ms; batch->turns=1; batch->probe_clean=1;
  batch->probe_generation=generation; batch->probe_count=count;
  batch->probe_accept_turn=accept_turn;
}
static void ku_http_task_io_batch_begin(KuHttpTaskIoBatch* batch,unsigned long long now) {
  if (!batch->turns) batch->started_ms=now;
  batch->turns++;
}
static int ku_http_task_io_batch_continue(KuHttpTaskIoBatch* batch,
    unsigned long long now,int ready,int urgent) {
  if (ready && !urgent && now!=~0ULL && batch->started_ms!=~0ULL
      && batch->turns && batch->turns<KU_HTTP_TASK_IO_BATCH_TURNS
      && now>=batch->started_ms && now-batch->started_ms<1u) return 1;
  ku_http_task_io_batch_reset(batch); return 0;
}
/* End bounded HTTP Task/I/O batch. */
static int ku_http_task_slice_interrupted(void* raw) {
#if !defined(_WIN32)
  return atomic_load_explicit(&((KuHttpWake*)raw)->pending,memory_order_acquire)!=0;
#else
  (void)raw; return 1; /* Keep the original UDP wake path until separately covered. */
#endif
}
#endif
/* Persistent owner reactor. Kernel payloads are unique 64-bit registration
 * tokens, never pointers to mutable/recycled slots or truncated generations.
 * Strict idle sockets have one READ registration and one indexed heap item;
 * they are absent from the owner's per-turn work and interest traversal. */
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
#define KU_HTTP_READY_BATCH 256u
typedef struct {
  KuHttpSocket fd;
  uint64_t token,generation;
  short events;
  int registered,poll_index;
  size_t resource_slot;
} KuHttpRegistration;
typedef struct { uint64_t token; size_t entry; } KuHttpRegistrationHash;
typedef struct {
  size_t entry;
  uint64_t token,generation;
  KuHttpSocket fd;
  short events;
} KuHttpReadyEvent;
typedef struct KuHttpReactor {
  int descriptor,fault,slots,work_count,idle_count,idle_initial_count,draining;
  size_t capacity,hash_capacity,fd_capacity,fd_count,old_net_count,ready_count,pending_base;
  uint64_t next_token;
  KuHttpActiveSet* active;
  KuHttpRegistration* registrations;
  KuHttpRegistrationHash* hash;
  /* Slot+1, not a truncated fd/token. Zero is empty; keys live in registrations. */
  uint32_t* fd_entries;
  int* work;
  int* idle_heap;
  KuHttpReadyEvent ready[KU_HTTP_READY_BATCH];
#if defined(__linux__)
  struct epoll_event events[KU_HTTP_READY_BATCH];
#else
  struct kevent events[KU_HTTP_READY_BATCH];
#endif
} KuHttpReactor;
#if defined(__APPLE__)
typedef char KuHttpReactorTokenWidth[(sizeof(uintptr_t)>=sizeof(uint64_t))?1:-1];
#endif
static size_t ku_http_reactor_hash_capacity(size_t capacity) {
  size_t result=8u;
  while(result<capacity*2u) result*=2u;
  return result;
}
static size_t ku_http_reactor_storage_bytes(size_t slots,size_t net) {
  size_t capacity=slots+3u+net;
  return sizeof(KuHttpReactor)+capacity*sizeof(KuHttpRegistration)
      +ku_http_reactor_hash_capacity(capacity)*(sizeof(KuHttpRegistrationHash)+sizeof(uint32_t))
      +slots*2u*sizeof(int);
}
static size_t ku_http_reactor_pending_storage_bytes(size_t slots,size_t net,size_t pending) {
  size_t before=slots+3u+net,after=before+pending;
  return ku_http_reactor_storage_bytes(slots,net)
      +pending*sizeof(KuHttpRegistration)
      +(ku_http_reactor_hash_capacity(after)-ku_http_reactor_hash_capacity(before))*(sizeof(KuHttpRegistrationHash)+sizeof(uint32_t));
}
static size_t ku_http_reactor_hash_start(const KuHttpReactor* r,uint64_t token) {
  token^=token>>30; token*=UINT64_C(0xbf58476d1ce4e5b9);
  token^=token>>27; token*=UINT64_C(0x94d049bb133111eb); token^=token>>31;
  return (size_t)token&(r->hash_capacity-1u);
}
static size_t ku_http_reactor_lookup(const KuHttpReactor* r,uint64_t token) {
  if(!token || token==UINT64_MAX) return SIZE_MAX;
  size_t start=ku_http_reactor_hash_start(r,token);
  for(size_t i=0;i<r->hash_capacity;i++) {
    const KuHttpRegistrationHash* item=&r->hash[(start+i)&(r->hash_capacity-1u)];
    if(!item->token) break;
    if(item->token==token) return item->entry;
  }
  return SIZE_MAX;
}
static int ku_http_reactor_hash_add(KuHttpReactor* r,uint64_t token,size_t entry) {
  size_t start=ku_http_reactor_hash_start(r,token);
  for(size_t i=0;i<r->hash_capacity;i++) {
    size_t at=(start+i)&(r->hash_capacity-1u);
    if(!r->hash[at].token) {r->hash[at]=(KuHttpRegistrationHash){token,entry};return 1;}
  }
  return 0;
}
static void ku_http_reactor_hash_remove(KuHttpReactor* r,uint64_t token) {
  size_t start=ku_http_reactor_hash_start(r,token);
  for(size_t i=0;i<r->hash_capacity;i++) {
    KuHttpRegistrationHash* item=&r->hash[(start+i)&(r->hash_capacity-1u)];
    if(!item->token) return;
    if(item->token==token) {
      /* Back-shift the finite probe cluster. Repeated request turnover must
       * not fill the table with tombstones and turn stale lookup into O(cap). */
      size_t mask=r->hash_capacity-1u,hole=(start+i)&mask,at=(hole+1u)&mask;
      for(size_t moved=0;moved<r->hash_capacity && r->hash[at].token;moved++,at=(at+1u)&mask) {
        size_t home=ku_http_reactor_hash_start(r,r->hash[at].token);
        if(((at-home)&mask)>=((hole-home)&mask)) {r->hash[hole]=r->hash[at];hole=at;}
      }
      r->hash[hole]=(KuHttpRegistrationHash){0,0};return;
    }
  }
  r->fault=1;
}
/* Owner-local fd index. At most one logical registration owns a numeric fd:
 * a successful NEW replaces a retired external resource before old events are
 * dispatched. Capacity is bounded by the same registration table, not fd value.
 * Open addressing has <=1/2 load; deletion back-shifts instead of accumulating
 * tombstones. Full storage/charge is reserved at startup. The logical prefix
 * grows before kernel ADD without any new allocation or failure timing. */
static size_t ku_http_reactor_fd_start(const KuHttpReactor* r,KuHttpSocket fd) {
  return ku_http_reactor_hash_start(r,(uint64_t)(uintptr_t)fd)&(r->fd_capacity-1u);
}
static size_t ku_http_reactor_fd_bucket(KuHttpReactor* r,KuHttpSocket fd) {
  size_t start=ku_http_reactor_fd_start(r,fd);
  for(size_t i=0;i<r->fd_capacity;i++) {
    size_t at=(start+i)&(r->fd_capacity-1u);
    uint32_t stored=r->fd_entries[at];
    if(!stored)return at;
    size_t index=(size_t)stored-1u;
    if(index>=r->capacity || !r->registrations[index].registered) {r->fault=1;return SIZE_MAX;}
    if(r->registrations[index].fd==fd)return at;
  }
  r->fault=1;return SIZE_MAX;
}
static void ku_http_reactor_fd_remove(KuHttpReactor* r,size_t index) {
  size_t hole=ku_http_reactor_fd_bucket(r,r->registrations[index].fd);
  if(hole==SIZE_MAX || r->fd_entries[hole]!=(uint32_t)(index+1u)) {r->fault=1;return;}
  size_t mask=r->fd_capacity-1u,at=(hole+1u)&mask;
  for(size_t moved=0;moved<r->fd_capacity && r->fd_entries[at];moved++,at=(at+1u)&mask) {
    size_t other=(size_t)r->fd_entries[at]-1u;
    if(other>=r->capacity || !r->registrations[other].registered) {r->fault=1;return;}
    size_t home=ku_http_reactor_fd_start(r,r->registrations[other].fd);
    if(((at-home)&mask)>=((hole-home)&mask)) {r->fd_entries[hole]=r->fd_entries[at];hole=at;}
  }
  r->fd_entries[hole]=0;
  if(!r->fd_count) {r->fault=1;return;}
  r->fd_count--;
}
/* A capacity doubling rehashes stable registrations at most once per level.
 * The same owner exclusively mutates both tables; no kernel identity changes
 * until this bounded preflight succeeds. Capacity never shrinks behind a live
 * identity, and retained high-water storage/charge is reported unchanged. */
static int ku_http_reactor_fd_prepare(KuHttpReactor* r) {
  if(r->fd_count<r->fd_capacity/2u)return 1;
  if(r->fd_capacity>=r->hash_capacity) {r->fault=1;return 0;}
  size_t grown=r->fd_capacity*2u;
  memset(r->fd_entries,0,grown*sizeof(*r->fd_entries));r->fd_capacity=grown;
  size_t count=0;
  for(size_t i=0;i<r->capacity;i++)if(r->registrations[i].registered) {
    size_t at=ku_http_reactor_fd_bucket(r,r->registrations[i].fd);
    if(at==SIZE_MAX || r->fd_entries[at]) {r->fault=1;return 0;}
    r->fd_entries[at]=(uint32_t)(i+1u);count++;
  }
  if(count!=r->fd_count) {r->fault=1;return 0;}
  return 1;
}
#if defined(KU_HTTP_REACTOR_FD_ORACLE)
/* Diagnostic-only oracle: compare exact target identity against the previous
 * full scan on every NEW/delete. Ordinary scoring contains no scan/counters. */
static int ku_http_reactor_fd_oracle(KuHttpReactor* r,KuHttpSocket fd,size_t expected) {
  size_t found=SIZE_MAX;
  for(size_t i=0;i<r->capacity;i++) {
    if(!r->registrations[i].registered || r->registrations[i].fd!=fd)continue;
    if(found!=SIZE_MAX)return 0;
    found=i;
  }
  return found==expected;
}
#endif
static int ku_http_reactor_kernel(KuHttpReactor* r,KuHttpRegistration* entry,
    KuHttpSocket fd,short events,uint64_t token,int remove) {
  /* Task-net owns its sockets and can close one during pump/interests, before
   * the owner reconciles the previous snapshot. That removes its old kernel
   * interest automatically. Never interpret a closed event source as this
   * benign target-fd retirement. Fresh interests still require a live target. */
  if(!remove && !entry->registered && fcntl(fd,F_GETFD)<0) return 0;
#if defined(__linux__)
  struct epoll_event event={0};
  if(events&KU_HTTP_POLL_READ) event.events|=EPOLLIN|EPOLLRDHUP;
  if(events&KU_HTTP_POLL_WRITE) event.events|=EPOLLOUT;
  event.data.u64=token;
  int operation=remove?EPOLL_CTL_DEL:entry->registered?EPOLL_CTL_MOD:EPOLL_CTL_ADD;
  int status;
  do { status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event); }
  while(status<0 && errno==EINTR);
  int error=errno;
  return !status || (remove && (error==ENOENT
      || (error==EBADF && r->descriptor>=0 && fcntl(r->descriptor,F_GETFD)>=0)));
#else
  /* READ is removed while a live handler pauses it. Its real 25ms SO_ERROR
   * deadline remains in the owner maintenance loop; FIN is not cancellation. */
  short previous=entry->registered?entry->events:0;
  const short masks[2]={KU_HTTP_POLL_READ,KU_HTTP_POLL_WRITE};
  const int16_t filters[2]={EVFILT_READ,EVFILT_WRITE};
  for(size_t i=0;i<2u;i++) {
    int had=(previous&masks[i])!=0,want=!remove && (events&masks[i])!=0;
    /* A fresh identity exclusively owns both filters for this numeric fd.
     * Clear any unwanted old filter too; a kqueue ADD replaces one filter,
     * unlike epoll ADD which rejects an existing live-fd registration. */
    if(had==want && (entry->registered || remove || want)) continue;
    struct kevent change;
    EV_SET(&change,(uintptr_t)fd,filters[i],want?(EV_ADD|EV_ENABLE):EV_DELETE,0,0,
        (void*)(uintptr_t)token);
    int status;
    do { status=kevent(r->descriptor,&change,1,NULL,0,NULL); }
    while(status<0 && errno==EINTR);
    if(status<0) {
      int error=errno;
      if(!want && error==ENOENT) continue;
      if(remove && error==EBADF && r->descriptor>=0 && fcntl(r->descriptor,F_GETFD)>=0) continue;
      return 0;
    }
  }
  return 1;
#endif
}
static void ku_http_reactor_delete(KuHttpReactor* r,size_t index) {
  if(index>=r->capacity) {r->fault=1;return;}
  KuHttpRegistration* entry=&r->registrations[index];
  if(!entry->registered) return;
  if(!ku_http_reactor_kernel(r,entry,entry->fd,0,entry->token,1)) r->fault=1;
  ku_http_reactor_hash_remove(r,entry->token);
  ku_http_reactor_fd_remove(r,index);
#if defined(KU_HTTP_REACTOR_FD_ORACLE)
  KuHttpSocket removed_fd=entry->fd;
#endif
  *entry=(KuHttpRegistration){0};
#if defined(KU_HTTP_REACTOR_FD_ORACLE)
  if(!ku_http_reactor_fd_oracle(r,removed_fd,SIZE_MAX))r->fault=1;
#endif
}
static void ku_http_reactor_retire_fd_aliases(KuHttpReactor* r,size_t current,KuHttpSocket fd,size_t bucket) {
  /* Only after the NEW kernel registration succeeded. Never DEL this old
   * numeric fd: it now denotes the new resource and its kernel registration. */
  uint32_t stored=r->fd_entries[bucket];
  if(!stored)return;
  size_t i=(size_t)stored-1u;
  if(i==current || i>=r->capacity || !r->registrations[i].registered
      || r->registrations[i].fd!=fd) {r->fault=1;return;}
  KuHttpRegistration* old=&r->registrations[i];
  ku_http_reactor_hash_remove(r,old->token);
  *old=(KuHttpRegistration){0};

}
static void ku_http_reactor_interest(KuHttpReactor* r,size_t index,
    KuHttpSocket fd,short events,uint64_t generation,int poll_index) {
  if(index>=r->capacity) {r->fault=1;return;}
  KuHttpRegistration* entry=&r->registrations[index];
  if(fd==KU_HTTP_INVALID_SOCKET) {ku_http_reactor_delete(r,index);return;}
  if(entry->registered && entry->fd==fd && entry->generation==generation) {
    if(entry->events!=events && !ku_http_reactor_kernel(r,entry,fd,events,entry->token,0)) r->fault=1;
    entry->events=events;entry->poll_index=poll_index;return;
  }
  ku_http_reactor_delete(r,index);
  if(r->next_token>=UINT64_MAX-1u) {r->fault=1;return;}
  uint64_t token=++r->next_token;
  if(!ku_http_reactor_hash_add(r,token,index)) {r->fault=1;return;}
  /* Preflight before changing the kernel. This owner cannot concurrently
   * mutate the index, so a successful NEW can publish without a fallible insert. */
  if(!ku_http_reactor_fd_prepare(r)) {ku_http_reactor_hash_remove(r,token);return;}
  size_t bucket=ku_http_reactor_fd_bucket(r,fd);
  if(bucket==SIZE_MAX) {ku_http_reactor_hash_remove(r,token);r->fault=1;return;}
#if defined(KU_HTTP_REACTOR_FD_ORACLE)
  size_t expected=r->fd_entries[bucket]?(size_t)r->fd_entries[bucket]-1u:SIZE_MAX;
  if(!ku_http_reactor_fd_oracle(r,fd,expected)) {ku_http_reactor_hash_remove(r,token);r->fault=1;return;}
#endif
  if(!ku_http_reactor_kernel(r,entry,fd,events,token,0)) {
    ku_http_reactor_hash_remove(r,token);r->fault=1;return;
  }
  int new_fd=!r->fd_entries[bucket];
  ku_http_reactor_retire_fd_aliases(r,index,fd,bucket);
  *entry=(KuHttpRegistration){fd,token,generation,events,1,poll_index,0};
  r->fd_entries[bucket]=(uint32_t)(index+1u);
  if(new_fd)r->fd_count++;
#if defined(KU_HTTP_REACTOR_FD_ORACLE)
  if(!ku_http_reactor_fd_oracle(r,fd,index))r->fault=1;
#endif
}
static int ku_http_reactor_idle(const KuHttpConnection* c) {
  return c->phase==KU_HTTP_IO_HEADER && !c->request_lease && !c->request_queued && !c->buffered && !c->carry_length
      && !c->job && !c->async_task
      && c->deadline && c->deadline!=UINT64_MAX && c->socket_value!=KU_HTTP_INVALID_SOCKET;
}
static short ku_http_reactor_connection_events(const KuHttpConnection* c) {
  if(c->phase==KU_HTTP_IO_HEADER || c->phase==KU_HTTP_IO_BODY) return KU_HTTP_POLL_READ;
  if(c->phase==KU_HTTP_IO_WRITE) return KU_HTTP_POLL_WRITE;
  if((c->phase==KU_HTTP_IO_ASYNC || c->phase==KU_HTTP_IO_JOB) && !c->peer_read_paused)
    return KU_HTTP_POLL_READ;
  return 0;
}
static unsigned long long ku_http_reactor_connection_idle_deadline(const KuHttpConnection* c) {
  return c->request_deadline && c->request_deadline<c->deadline?c->request_deadline:c->deadline;
}
static int ku_http_reactor_heap_before(KuHttpReactor* r,int left,int right) {
  const KuHttpConnection* a=&r->active->connections[left];
  const KuHttpConnection* b=&r->active->connections[right];
  unsigned long long ad=ku_http_reactor_connection_idle_deadline(a),bd=ku_http_reactor_connection_idle_deadline(b);
  return ad<bd || (ad==bd && left<right);
}
static void ku_http_reactor_heap_swap(KuHttpReactor* r,int a,int b) {
  int slot=r->idle_heap[a];r->idle_heap[a]=r->idle_heap[b];r->idle_heap[b]=slot;
  r->active->connections[r->idle_heap[a]].idle_position=a;
  r->active->connections[r->idle_heap[b]].idle_position=b;
}
static void ku_http_reactor_heap_fix(KuHttpReactor* r,int position) {
  while(position>0) {
    int parent=(position-1)/2;
    if(!ku_http_reactor_heap_before(r,r->idle_heap[position],r->idle_heap[parent])) break;
    ku_http_reactor_heap_swap(r,position,parent);position=parent;
  }
  for(;;) {
    int left=position*2+1;if(left>=r->idle_count) break;
    int right=left+1,best=right<r->idle_count && ku_http_reactor_heap_before(r,r->idle_heap[right],r->idle_heap[left])?right:left;
    if(!ku_http_reactor_heap_before(r,r->idle_heap[best],r->idle_heap[position])) break;
    ku_http_reactor_heap_swap(r,position,best);position=best;
  }
}
static void ku_http_reactor_idle_remove(KuHttpReactor* r,KuHttpConnection* c) {
  int position=c->idle_position;if(position<0) return;
  if(position>=r->idle_count || &r->active->connections[r->idle_heap[position]]!=c) {r->fault=1;return;}
  if(!c->reused) r->idle_initial_count--;
  int last=r->idle_heap[--r->idle_count];c->idle_position=-1;
  if(position<r->idle_count) {
    r->idle_heap[position]=last;r->active->connections[last].idle_position=position;
    ku_http_reactor_heap_fix(r,position);
  }
}
static void ku_http_reactor_work_remove(KuHttpReactor* r,KuHttpConnection* c) {
  int position=c->work_position;if(position<0) return;
  if(position>=r->work_count || &r->active->connections[r->work[position]]!=c) {r->fault=1;return;}
  int last=r->work[--r->work_count];c->work_position=-1;
  if(position<r->work_count) {r->work[position]=last;r->active->connections[last].work_position=position;}
}
static void ku_http_reactor_work_add(KuHttpReactor* r,KuHttpConnection* c) {
  if(c->work_position>=0) return;
  if(r->work_count>=r->slots) {r->fault=1;return;}
  c->work_position=r->work_count;r->work[r->work_count++]=(int)(c-r->active->connections);
}
static void ku_http_reactor_admit(KuHttpActiveSet* active,KuHttpConnection* c) {
  c->reactor=active->reactor;c->work_position=-1;c->idle_position=-1;
  if(active->reactor) ku_http_reactor_work_add(active->reactor,c);
}
static void ku_http_reactor_retire(KuHttpConnection* c) {
  KuHttpReactor* r=c->reactor;if(!r) return;
  ku_http_reactor_idle_remove(r,c);ku_http_reactor_work_remove(r,c);
}
static void ku_http_reactor_forget(KuHttpConnection* c) {
  KuHttpReactor* r=c->reactor;if(!r) return;
  size_t index=c->active_set?(size_t)(c-r->active->connections):(size_t)r->slots+2u;
  ku_http_reactor_delete(r,index);
  if(c->active_set && c->active_position>=0) {
    ku_http_reactor_idle_remove(r,c);ku_http_reactor_work_add(r,c);
  }
}
static void ku_http_reactor_pending_forget(KuHttpActiveSet* active,int index) {
  if(active && active->reactor) ku_http_reactor_delete(active->reactor,active->reactor->pending_base+(size_t)index);
}
static void ku_http_reactor_refresh(KuHttpConnection* c) {
  KuHttpReactor* r=c->reactor;
  if(!r || !c->active_set || c->active_position<0) return;
  if(!r->draining && ku_http_reactor_idle(c)) {
    ku_http_reactor_work_remove(r,c);
    if(c->idle_position<0) {
      if(r->idle_count>=r->slots) {r->fault=1;return;}
      c->idle_position=r->idle_count;r->idle_heap[r->idle_count++]=(int)(c-r->active->connections);
      if(!c->reused) r->idle_initial_count++;
    }
    ku_http_reactor_heap_fix(r,c->idle_position);
  } else {
    ku_http_reactor_idle_remove(r,c);ku_http_reactor_work_add(r,c);
  }
  int watch=c->phase==KU_HTTP_IO_ASYNC || c->phase==KU_HTTP_IO_JOB
      || (c->phase==KU_HTTP_IO_REQUEST_WAIT && !c->request_wait_probe_deadline);
  ku_http_reactor_interest(r,(size_t)(c-r->active->connections),
      (watch || ku_http_reactor_connection_events(c))?c->socket_value:KU_HTTP_INVALID_SOCKET,
      ku_http_reactor_connection_events(c),c->connection_generation,-1);
}
static unsigned long long ku_http_reactor_idle_deadline(const KuHttpReactor* r) {
  return r->idle_count?ku_http_reactor_connection_idle_deadline(&r->active->connections[r->idle_heap[0]]):UINT64_MAX;
}
static int ku_http_reactor_event_valid(const KuHttpReactor* r,const KuHttpReadyEvent* event) {
  if(event->entry>=r->capacity) return 0;
  const KuHttpRegistration* entry=&r->registrations[event->entry];
  return entry->registered && entry->token==event->token && entry->generation==event->generation && entry->fd==event->fd;
}
static int ku_http_reactor_wait(KuHttpReactor* r,KuHttpPollFd* items,int timeout_ms) {
  r->ready_count=0;if(r->fault) {errno=EIO;return -1;}
#if defined(__linux__)
  int selected=epoll_wait(r->descriptor,r->events,(int)KU_HTTP_READY_BATCH,timeout_ms);
#else
  struct timespec duration={0,0};
  if(timeout_ms>=0) {duration.tv_sec=timeout_ms/1000;duration.tv_nsec=(timeout_ms%1000)*1000000L;}
  int selected=kevent(r->descriptor,NULL,0,r->events,(int)KU_HTTP_READY_BATCH,timeout_ms<0?NULL:&duration);
#endif
  if(selected<=0) return selected;
  int valid=0;
  for(int i=0;i<selected;i++) {
#if defined(__linux__)
    uint64_t token=r->events[i].data.u64;uint32_t flags=r->events[i].events;
    short events=(short)(((flags&EPOLLIN)?KU_HTTP_POLL_READ:0)|((flags&EPOLLOUT)?KU_HTTP_POLL_WRITE:0)
        |((flags&EPOLLERR)?POLLERR:0)|((flags&(EPOLLHUP|EPOLLRDHUP))?POLLHUP:0));
#else
    uint64_t token=(uint64_t)(uintptr_t)r->events[i].udata;
    short events=(short)((r->events[i].filter==EVFILT_READ?KU_HTTP_POLL_READ:KU_HTTP_POLL_WRITE)
        |((r->events[i].flags&EV_ERROR)?POLLERR:0)|((r->events[i].flags&EV_EOF)?POLLHUP:0));
#endif
    size_t index=ku_http_reactor_lookup(r,token);if(index==SIZE_MAX) continue;
    KuHttpRegistration* entry=&r->registrations[index];if(!entry->registered || entry->token!=token) continue;
    if(index<(size_t)r->slots) {
      r->ready[r->ready_count++]=(KuHttpReadyEvent){index,token,entry->generation,entry->fd,events};
    } else if(entry->poll_index>=0) items[entry->poll_index].revents|=events;
    valid++;
  }
  /* Stale events are not a zero-readiness receipt. The caller must re-enter
   * maintenance/I/O rather than grant compute from an invalid old token. */
  return valid?valid:1;
}
static void ku_http_reactor_destroy(KuHttpReactor* r) {
  if(r->descriptor>=0) close(r->descriptor);
  free(r->registrations);free(r->hash);free(r->fd_entries);free(r->work);free(r->idle_heap);
  r->descriptor=-1;r->registrations=NULL;r->hash=NULL;r->fd_entries=NULL;r->work=NULL;r->idle_heap=NULL;
}
static int ku_http_reactor_init_pending(KuHttpReactor* r,KuHttpActiveSet* active,size_t net,size_t pending) {
  memset(r,0,sizeof(*r));r->descriptor=-1;r->slots=active->capacity;r->active=active;
  /* Public limits are much smaller (C<=32768,N<=16384,R<=8192). Keep the
   * slot+1 representation explicit even for direct internal callers. */
  if(r->slots<0 || (size_t)r->slots>UINT32_MAX-3u
      || net>UINT32_MAX-3u-(size_t)r->slots
      || pending>UINT32_MAX-3u-(size_t)r->slots-net)return 0;
  r->pending_base=(size_t)r->slots+3u+net;
  r->capacity=r->pending_base+pending;r->hash_capacity=ku_http_reactor_hash_capacity(r->capacity);
  r->fd_capacity=r->hash_capacity<32u?r->hash_capacity:32u;
  r->registrations=(KuHttpRegistration*)calloc(r->capacity,sizeof(*r->registrations));
  r->hash=(KuHttpRegistrationHash*)calloc(r->hash_capacity,sizeof(*r->hash));
  r->fd_entries=(uint32_t*)calloc(r->hash_capacity,sizeof(*r->fd_entries));
  r->work=(int*)calloc((size_t)r->slots,sizeof(int));r->idle_heap=(int*)calloc((size_t)r->slots,sizeof(int));
#if defined(__linux__)
  r->descriptor=epoll_create1(EPOLL_CLOEXEC);
#else
  r->descriptor=kqueue();
  if(r->descriptor>=0 && fcntl(r->descriptor,F_SETFD,FD_CLOEXEC)<0) {close(r->descriptor);r->descriptor=-1;}
#endif
  if(r->descriptor<0 || !r->registrations || !r->hash || !r->fd_entries || (r->slots && (!r->work || !r->idle_heap))) {
    ku_http_reactor_destroy(r);return 0;
  }
  active->reactor=r;return 1;
}
static int ku_http_reactor_init(KuHttpReactor* r,KuHttpActiveSet* active,size_t net) {
  return ku_http_reactor_init_pending(r,active,net,0);
}
#endif
/* End persistent owner reactor. */
static const char* ku_http_loop_run(KuHttpLoopOwner* owner) {
  KuHttpLoopGroup* group = owner->group;
  KuHttpServer* server = group->server;
  KuHttpSocket srv = group->listener;
  long max_conn = (long)server->max_connections;
  int max_pending = owner->pending;
  int slot_count = owner->slots;
  int request_limit = owner->request_limit;
  int nworkers = owner->workers;
  size_t poll_capacity=(size_t)slot_count+(size_t)max_pending+3u,net_capacity=0,net_bytes=0;
#if defined(KU_HTTP_HAS_TASK_NET)
  net_capacity=ku_http_task_net_capacity(group->tasks);
  if(!net_capacity || net_capacity>KU_TASK_NET_POLL_CAPACITY)return "Task network capacity invalid";
  poll_capacity+=net_capacity;net_bytes=net_capacity*sizeof(KuTaskNetPollV1);
#endif
  /* Configuration limits bound all products below on every supported target.
     One receipt includes every owner array; no Task/connection consumes an
     independent copy of the service's configured byte budget. */
  size_t fixed_bytes=(size_t)slot_count*(sizeof(KuHttpConnection)+sizeof(int)+sizeof(KuHttpPollConnection))
      +(size_t)request_limit*sizeof(KuHttpJob*)
      +poll_capacity*sizeof(KuHttpPollFd)+(size_t)max_pending*sizeof(KuHttpPendingItem)+net_bytes;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  fixed_bytes+=ku_http_reactor_pending_storage_bytes((size_t)slot_count,net_capacity,(size_t)max_pending);
#endif
#if defined(_WIN32)
  fixed_bytes+=(size_t)nworkers*sizeof(HANDLE);
#else
  fixed_bytes+=(size_t)nworkers*sizeof(pthread_t);
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  if(ku_http_task_fixed_reserve(group->tasks,fixed_bytes)!=0u)return "reactor fixed storage exceeds Task budget";
#endif
  KuHttpConnection* connections=(KuHttpConnection*)calloc((size_t)slot_count,sizeof(KuHttpConnection));
  owner->connection_base=connections;
  KuHttpPollFd* poll_items=(KuHttpPollFd*)calloc(poll_capacity,sizeof(KuHttpPollFd));
  int* active_indices=(int*)calloc((size_t)slot_count,sizeof(int));
  KuHttpPollConnection* poll_connections=(KuHttpPollConnection*)calloc((size_t)slot_count,sizeof(KuHttpPollConnection));
  void* net_storage=net_bytes?calloc(1,net_bytes):NULL;
#if defined(KU_HTTP_HAS_TASK_NET)
  KuTaskNetPollV1* net_interests=(KuTaskNetPollV1*)net_storage;
#endif
  KuHttpActiveSet active={.connections=connections,.indices=active_indices,.capacity=slot_count,.free_head=-1,.request_limit=request_limit};
  KuHttpPending pending={0};pending.cap=max_pending;
  pending.items=(KuHttpPendingItem*)calloc((size_t)max_pending,sizeof(KuHttpPendingItem));
  if((slot_count && (!connections || !active_indices || !poll_connections))
      || !poll_items || !pending.items || (net_bytes && !net_storage)) {
    free(connections);free(poll_items);free(pending.items);free(active_indices);free(poll_connections);free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    (void)ku_http_task_fixed_refund(group->tasks,fixed_bytes);
#endif
    return "reactor allocation failed";
  }
  ku_http_pending_init(&pending,&active);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  KuHttpReactor reactor;
  if(!ku_http_reactor_init_pending(&reactor,&active,net_capacity,(size_t)max_pending)) {
    free(connections);free(poll_items);free(pending.items);free(active_indices);free(poll_connections);free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    (void)ku_http_task_fixed_refund(group->tasks,fixed_bytes);
#endif
    return "persistent reactor initialization failed";
  }
#endif
  KuHttpConnection rejection = {0};
  rejection.socket_value = KU_HTTP_INVALID_SOCKET;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  rejection.reactor=&reactor;rejection.work_position=-1;rejection.idle_position=-1;
#endif
  KuHttpWake wake;
  if (!ku_http_wake_init(&wake)) {
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    ku_http_reactor_destroy(&reactor);
#endif
    free(connections); free(poll_items); free(pending.items); free(active_indices); free(poll_connections); free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    (void)ku_http_task_fixed_refund(group->tasks,fixed_bytes);
#endif
    return "reactor wake initialization failed";
  }
  KuHttpQueue queue;
  /* Jobs and Task slots remain bounded by A, independently of C records.
     Raw sockets and existing waiting requests share the single P FIFO. */
  if (!ku_http_queue_init(&queue, request_limit, &wake)) {
    ku_http_wake_close(&wake);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    ku_http_reactor_destroy(&reactor);
#endif
    free(connections); free(poll_items); free(pending.items); free(active_indices); free(poll_connections); free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    (void)ku_http_task_fixed_refund(group->tasks,fixed_bytes);
#endif
    return "handler queue initialization failed";
  }
  KuHttpWorkerCtx ctx; ctx.server = server; ctx.queue = &queue;
#if defined(_WIN32)
  HANDLE* workers = (HANDLE*)malloc((size_t)nworkers * sizeof(HANDLE));
#else
  pthread_t* workers = (pthread_t*)malloc((size_t)nworkers * sizeof(pthread_t));
#endif
  int spawned = 0;
  if (workers) for (int index = 0; index < nworkers; index++) {
#if defined(_WIN32)
    uintptr_t handle = _beginthreadex(NULL, (unsigned)ku_runtime_thread_stack_bytes(), ku_http_worker, &ctx, 0, NULL);
    if (!handle) break;
    workers[spawned++] = (HANDLE)handle;
#else
    pthread_t worker;
    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes) != 0) break;
    int status = pthread_attr_setstacksize(&attributes, ku_runtime_thread_stack_bytes());
    if (!status) status = pthread_create(&worker, &attributes, ku_http_worker, &ctx);
    pthread_attr_destroy(&attributes);
    if (status) break;
    workers[spawned++] = worker;
#endif
  }
  int listening = 1;
  int failure_handled = 0;
  int peer_accept_errors = 0;
  int resource_accept_errors = 0;
  unsigned long long accept_retry_deadline = 0;
  unsigned long long health_deadline=ku_http_deadline_ms(500);
  uint64_t next_cpu_sample_ms=0;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  KuHttpTaskIoBatch task_io_batch={0};
  KuHttpTaskSliceTurn task_io_slice_turn={0,UINT64_MAX};
#endif
  const char* listen_failure = spawned == nworkers ? NULL : "handler thread startup failed";
  ku_http_mutex_lock(&group->lock);
  owner->wake = &wake; owner->ready = !listen_failure;
  if (listen_failure) group->failure = listen_failure;
  ku_http_mutex_unlock(&group->lock);
  ku_http_wake_notify(&group->wake);
  for (;;) {
    queue.owner_inline_remaining=32u;
    ku_http_mutex_lock(&queue.lock);
    ku_http_queue_observation_begin_locked(&queue);
    size_t queued = (size_t)queue.count, running = queue.running, resident = queue.resident;
    size_t queued_bytes=queue.queued_bytes, peak_queued_bytes=queue.peak_queued_bytes;
    ku_http_mutex_unlock(&queue.lock);
    size_t occupied_connections=active.sockets+(size_t)pending.raw_count;
    int occupied_requests=active.request_active;
    uint64_t sampled_cpu_ns=0, sampled_cpu_ms=ku_http_now_ms();
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    int task_io_pending_seen=pending.count!=0;
    ku_http_task_io_batch_begin(&task_io_batch,sampled_cpu_ms);
#endif
    int cpu_sample_due=sampled_cpu_ms>=next_cpu_sample_ms;
    int cpu_sample_valid=cpu_sample_due && ku_http_thread_cpu_ns(&sampled_cpu_ns);
    if (cpu_sample_due) next_cpu_sample_ms=sampled_cpu_ms>UINT64_MAX-10u ? UINT64_MAX : sampled_cpu_ms+10u;
    ku_http_mutex_lock(&group->lock);
    if (cpu_sample_due) ku_http_owner_cpu_sample(owner,sampled_cpu_ns,sampled_cpu_ms,cpu_sample_valid);
    owner->queued = queued; owner->running = running; owner->resident = resident;
    owner->queued_bytes=queued_bytes; owner->peak_queued_bytes=peak_queued_bytes;
    owner->connections = occupied_connections; owner->pending_count = (size_t)pending.count;
    owner->active_requests=(size_t)active.request_active;owner->peak_active_requests=(size_t)active.request_peak;
    owner->heartbeat = ku_http_now_ms();
    if (owner->heartbeat >= health_deadline) {
      unsigned long long lag=owner->heartbeat-health_deadline;
      if (lag>owner->peak_loop_delay_ms) owner->peak_loop_delay_ms=lag;
      health_deadline=ku_http_deadline_ms(500);
    }
    int started = group->started;
    int accept_turn = group->accept_turn == owner->index;
    if (group->draining || group->failure) listening = 0;
    if (group->failure && !listen_failure) listen_failure = group->failure;
    ku_http_mutex_unlock(&group->lock);
    if (listen_failure && !failure_handled) {
      failure_handled = 1; listening = 0;
      ku_http_mutex_lock(&group->lock);
      if (!group->failure) group->failure = listen_failure;
      if (!group->draining) { group->draining = 1; group->drain_deadline = ku_http_deadline_ms(group->drain_ms); }
      ku_http_mutex_unlock(&group->lock);
      ku_http_group_notify(group);

      ku_http_queue_close(&queue, 0);
      ku_http_pending_close(&pending);
      ku_http_connection_close(&rejection);
      for (int position=active.count;position-->0;) {
        int index=active.indices[position];
        KuHttpConnection* connection = &connections[index];
        if (connection->phase == KU_HTTP_IO_JOB) {
          ku_http_mutex_lock(&connection->job->lock);
          int claimed = connection->job->claimed;
          ku_http_mutex_unlock(&connection->job->lock);
          if (claimed) continue; /* Already executing handlers finish normally. */
        } else if (connection->phase == KU_HTTP_IO_WRITE) continue;
        ku_http_connection_close(connection);
      }
    }
    uint64_t task_deadline = UINT64_MAX;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    /* A finished private slice owes I/O a full turn. In particular, do not
     * execute another early pump before rebuilding and probing its interests.
     * Carry its real ready/deadline receipt and force one nonblocking probe;
     * an own ready continuation deliberately did not write a wake byte. */
    uint32_t pumped=0;
    if (ku_http_task_slice_needs_pump(&task_io_slice_turn,&task_deadline))
      pumped=ku_http_task_pump(group->tasks,(size_t)owner->index,32u,&task_deadline);
    if (pumped > 1u && !listen_failure) listen_failure = "HTTP Task event loop failed";
#endif
    unsigned long long now = ku_http_now_ms();
    unsigned long long pending_deadline=ku_http_pending_maintain(&pending,server,now);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    /* Idle sockets are touched only for a due deadline, bounded admission
     * pressure, drain or actual kernel readiness. Move candidates to the work
     * list before its reverse pass; list/heap storage never duplicates slots. */
    reactor.draining=!listening || listen_failure;
    for(unsigned int turn=0;turn<64u && reactor.idle_count;turn++) {
      int connection_pressure=pending.raw_count && ku_http_free_slot(&active)<0;
      KuHttpConnection* idle=&connections[reactor.idle_heap[0]];
      if(!reactor.draining && !connection_pressure && ku_http_reactor_idle_deadline(&reactor)>now) break;
      ku_http_reactor_idle_remove(&reactor,idle);ku_http_reactor_work_add(&reactor,idle);
    }
    int maintenance_count=reactor.work_count;
#else
    int maintenance_count=active.count;
#endif
    unsigned int first_write_turns=0;
    /* Reverse traversal is intentional: swap-remove moves only a tail that
       this pass already observed. Admission runs strictly between passes. */
    for (int position=maintenance_count;position-->0;) {
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
      int index=reactor.work[position];
#else
      int index=active.indices[position];
#endif
      KuHttpConnection* connection = &connections[index];
      /* A response releases A only after real work retirement. Queued peers
         take their FIFO turn; they no longer evict an idle C record for A. */
      if (!listening || listen_failure) connection->keep_alive = 0;
      if (!listening && connection->phase == KU_HTTP_IO_HEADER && connection->reused && !connection->buffered && !connection->carry_length) {
        ku_http_count_transport_close(connection,KU_HTTP_CLOSE_DRAIN_IDLE);
        ku_http_connection_close(connection); continue;
      }
      int before_observe_phase=connection->phase;
      ku_http_connection_observe_job(connection, server);
      int response_became_ready=before_observe_phase!=KU_HTTP_IO_WRITE
          && connection->phase==KU_HTTP_IO_WRITE;
#if defined(__APPLE__)
      if ((connection->phase==KU_HTTP_IO_ASYNC || connection->phase==KU_HTTP_IO_JOB)
          && connection->peer_read_paused && connection->peer_error_probe_deadline
          && now>=connection->peer_error_probe_deadline)
        ku_http_connection_observe_peer(connection,0,1);
#endif
      if (connection->request_deadline && (connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY || connection->phase == KU_HTTP_IO_WRITE || connection->phase == KU_HTTP_IO_REQUEST_WAIT)
          && connection->deadline > connection->request_deadline) connection->deadline = connection->request_deadline;
      if (connection->async_task && connection->async_cancelled && now >= connection->async_cleanup_deadline)
        listen_failure = "asynchronous native work exceeded its cleanup deadline";
      if (connection->job) {
        ku_http_mutex_lock(&connection->job->lock);
        int stuck = connection->job->started && !connection->job->completed
          && now > connection->job->handler_deadline && now-connection->job->handler_deadline >= KU_HANDLER_CLEANUP_GRACE_MS;
        ku_http_mutex_unlock(&connection->job->lock);
        if (stuck) listen_failure = "native handler exceeded its cleanup deadline";
      }
      if (connection->phase == KU_HTTP_IO_HEADER && connection->carry_length)
        ku_http_connection_read_header(connection, server, &queue, spawned);
      /* Only real C-record exhaustion can retire a reused idle connection.
         Waiting for A alone must preserve idle connections and their bytes. */
      if (pending.raw_count && ku_http_free_slot(&active)<0 && connection->phase == KU_HTTP_IO_HEADER &&
          !connection->request_lease && !connection->buffered && !connection->carry_length) {
        /* An empty user buffer is not idle when the next request is already
           waiting in the socket. A single peek never consumes bytes or renews D. */
        char next_byte;
        int ready = recv(connection->socket_value, &next_byte, 1, MSG_PEEK);
        if (ready == 0 || (ready < 0 &&
            !ku_http_socket_error_interrupted(ku_http_socket_last_error()))) {
          ku_http_count_transport_close(connection,KU_HTTP_CLOSE_IDLE_PRESSURE);
          ku_http_connection_close(connection);
        }
      }
      if ((connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY || connection->phase == KU_HTTP_IO_REQUEST_WAIT) && now >= connection->deadline)
        ku_http_connection_status(connection, server, 408);
      else if (connection->phase == KU_HTTP_IO_WRITE && now >= connection->deadline) {
        ku_http_count_transport_close(connection,KU_HTTP_CLOSE_WRITE_DEADLINE);
        ku_http_connection_close(connection);
      }
      /* Newly published output may already fit a writable socket. Try only
       * one nonblocking send, after the original request/write deadline clip,
       * before changing its persistent interest. A partial send or EAGAIN
       * keeps WRITE and its cursor for the ordinary readiness path; an old
       * WRITE is never retried here. Bound the whole maintenance pass too.
       * Completed output does not recursively consume a pipelined request. */
      if(response_became_ready && connection->phase==KU_HTTP_IO_WRITE && first_write_turns<32u) {
        first_write_turns++;
        ku_http_connection_write_limited(connection,server,1,(size_t)16384);
      }
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
      ku_http_reactor_refresh(connection);
#endif
    }
    if (rejection.phase == KU_HTTP_IO_REJECT && now >= rejection.deadline)
      ku_http_connection_status(&rejection, server, 503);
    else if (rejection.phase == KU_HTTP_IO_WRITE && now >= rejection.deadline) {
      ku_http_count_transport_close(&rejection,KU_HTTP_CLOSE_WRITE_DEADLINE);
      ku_http_connection_close(&rejection);
    }
    int revisit_pending=!listen_failure && ku_http_pending_activate(&pending,&active,server);
    /* Maintenance may close a socket while a worker still owns its job.
       Refresh the already-published membership snapshot before a blocking wait;
       one extra turn suffices unless physical membership changes again. */
    if (revisit_pending || active.sockets+(size_t)pending.raw_count != occupied_connections
        || active.request_active!=occupied_requests) continue;
    int occupied = rejection.phase != KU_HTTP_IO_FREE || active.count;
    if (!listening && !occupied && !pending.count && !ku_http_queue_resident(&queue)) {
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
      ku_http_mutex_lock(&group->lock); owner->drained = 1; ku_http_mutex_unlock(&group->lock);
      ku_http_wake_notify(&group->wake);
      if (ku_http_task_owner_exited(group->tasks,(size_t)owner->index)) break;
#else
      break;
#endif
    }

    int poll_count = 0;
    int wake_index = poll_count;
    ku_http_poll_add(poll_items, &poll_count, wake.read_socket, KU_HTTP_POLL_READ);
    int listener_index = -1;
    unsigned long long nearest = health_deadline;
    if(pending_deadline<nearest) nearest=pending_deadline;
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    unsigned long long idle_deadline=ku_http_reactor_idle_deadline(&reactor);
    if(idle_deadline<nearest) nearest=idle_deadline;
#endif
    if (task_deadline < nearest) nearest = task_deadline; /* real in-loop health timer */
    now = ku_http_now_ms();
    if (started && accept_turn && listening && rejection.phase == KU_HTTP_IO_FREE) {
      if (accept_retry_deadline > now) { if (accept_retry_deadline < nearest) nearest = accept_retry_deadline; }
      else {
        listener_index = poll_count;
        ku_http_poll_add(poll_items, &poll_count, srv, KU_HTTP_POLL_READ);
      }
    }
#if !defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    int connection_poll_base=poll_count,connection_poll_count=0;
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    KuHttpTaskIoEligibility task_io_eligibility={~0ULL,
        task_deadline==0u && active.count!=0,task_deadline==0u && active.count!=0,0};
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    task_io_eligibility.deadline=idle_deadline;
    task_io_eligibility.has_idle=reactor.idle_count!=0;
    if(reactor.idle_count) task_io_eligibility.all_async=0;
    if(reactor.idle_initial_count) task_io_eligibility.all_batchable=0;
#endif
    unsigned long long task_io_slice_deadline=health_deadline;
    if (accept_retry_deadline>now && accept_retry_deadline<task_io_slice_deadline)
      task_io_slice_deadline=accept_retry_deadline;
#endif
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    int interest_count=reactor.work_count;
#else
    int interest_count=active.count;
#endif
    for (int position=0;position<interest_count;position++) {
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
      int index=reactor.work[position];
#else
      int index=active.indices[position];
#endif
      KuHttpConnection* connection = &connections[index];
      connection->poll_index = -1;
      short events = 0;
      int watch_peer=connection->phase==KU_HTTP_IO_ASYNC || connection->phase==KU_HTTP_IO_JOB
          || (connection->phase==KU_HTTP_IO_REQUEST_WAIT && !connection->request_wait_probe_deadline);
      if (connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY) events = KU_HTTP_POLL_READ;
      else if (connection->phase == KU_HTTP_IO_WRITE) events = KU_HTTP_POLL_WRITE;
      else if (watch_peer && connection->phase!=KU_HTTP_IO_REQUEST_WAIT && !connection->peer_read_paused) events=KU_HTTP_POLL_READ;
#if defined(__APPLE__)
      if (watch_peer && connection->peer_read_paused && connection->peer_error_probe_deadline
          && connection->peer_error_probe_deadline<nearest)
        nearest=connection->peer_error_probe_deadline;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
      if (watch_peer && connection->peer_read_paused && connection->peer_error_probe_deadline
          && connection->peer_error_probe_deadline<task_io_slice_deadline)
        task_io_slice_deadline=connection->peer_error_probe_deadline;
#endif
#endif
      if (connection->phase == KU_HTTP_IO_JOB && connection->job && connection->job->request_deadline < nearest)
        nearest = connection->job->request_deadline;
      if(connection->job) {
        ku_http_mutex_lock(&connection->job->lock);
        if(connection->job->started && !connection->job->completed) {
          unsigned long long deadline=connection->job->handler_deadline;
          deadline=KU_HANDLER_CLEANUP_GRACE_MS>~0ULL-deadline ? ~0ULL : deadline+KU_HANDLER_CLEANUP_GRACE_MS;
          if(deadline<nearest)nearest=deadline;
        }
        ku_http_mutex_unlock(&connection->job->lock);
      }
      if (connection->async_task && !connection->async_cancelled && connection->async_deadline < nearest)
        nearest = connection->async_deadline;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
      if (task_io_eligibility.all_batchable) {
        int async_eligible=connection->phase==KU_HTTP_IO_ASYNC && connection->async_task && !connection->async_cancelled;
        int idle_eligible=ku_http_task_io_idle_eligible(connection);
        ku_http_task_io_eligibility_observe_idle(&task_io_eligibility,
            async_eligible,idle_eligible,idle_eligible ? connection->deadline : connection->async_deadline);
      }
#endif
      if(connection->phase==KU_HTTP_IO_REQUEST_WAIT) {
        if(connection->deadline<nearest) nearest=connection->deadline;
        if(connection->request_wait_probe_deadline && connection->request_wait_probe_deadline<nearest)
          nearest=connection->request_wait_probe_deadline;
      }
      /* poll/WSAPoll return ERR/HUP/NVAL even with events=0. Pausing READ
         must not remove a live handler socket from transport-error observation. */
      if (!events && !watch_peer) continue;
      connection->poll_index = poll_count;
#if !defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
      poll_connections[connection_poll_count++]=(KuHttpPollConnection){index,connection->connection_generation};
#endif
      ku_http_poll_add(poll_items, &poll_count, connection->socket_value, events);
      if ((connection->phase==KU_HTTP_IO_HEADER || connection->phase==KU_HTTP_IO_BODY || connection->phase==KU_HTTP_IO_WRITE)
          && connection->deadline < nearest) nearest = connection->deadline;
    }
    for(int node=pending.head;node>=0;node=pending.items[node].next) {
      KuHttpPendingItem* item=&pending.items[node];item->poll_index=-1;
      if(item->kind==KU_HTTP_PENDING_RAW && !item->probe_only) {
        item->poll_index=poll_count;
        ku_http_poll_add(poll_items,&poll_count,item->socket_value,0);
      }
    }
    rejection.poll_index = -1;
    if (rejection.phase == KU_HTTP_IO_REJECT || rejection.phase == KU_HTTP_IO_WRITE) {
      rejection.poll_index = poll_count;
      ku_http_poll_add(poll_items, &poll_count, rejection.socket_value,
        rejection.phase == KU_HTTP_IO_REJECT ? KU_HTTP_POLL_READ : KU_HTTP_POLL_WRITE);
      if (rejection.deadline < nearest) nearest = rejection.deadline;
    }

#if defined(KU_HTTP_HAS_TASK_NET)
    uint64_t net_deadline=UINT64_MAX;
    size_t net_count=ku_http_task_net_interests(group->tasks,(size_t)owner->index,
        net_interests,net_capacity,&net_deadline);
    if (net_count>net_capacity) {
      listen_failure="Task network interest bound exceeded";
      ku_http_task_io_batch_reset(&task_io_batch); continue;
    }
    if (net_deadline<nearest) nearest=net_deadline;
    int net_base=poll_count;
    for (size_t i=0;i<net_count;i++) {
      short events=0;
      if (net_interests[i].events&KU_TASK_NET_READ) events|=KU_HTTP_POLL_READ;
      if (net_interests[i].events&KU_TASK_NET_WRITE) events|=KU_HTTP_POLL_WRITE;
      ku_http_poll_add(poll_items,&poll_count,(KuHttpSocket)net_interests[i].socket_value,events);
    }
#endif
    /* Self-publication is owner-local. Consume it only at an actual I/O turn,
     * after maintenance and FIFO admission, so no pending work is put to sleep.
     * Cross-thread publications still use the original wake protocol. */
    int owner_work=ku_http_queue_take_owner_work(&queue);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    owner_work|=owner->task_work_pending;
#endif
    int wait_ms = -1;
    if (nearest != ~0ULL) {
      now = ku_http_now_ms();
      unsigned long long remaining = nearest > now ? nearest - now : 0;
      wait_ms = remaining > (unsigned long long)INT_MAX ? INT_MAX : (int)remaining;
    }
    if(owner_work)wait_ms=0;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    /* Only already-ready computation can defer an empty nonblocking probe.
     * Every subturn above still observes group close, Task deadlines, response
     * completion and finite cleanup. Initial/partial reads, writes, pending
     * admission and Task-network interests retain their original I/O turn.
     * Strict idle reused reads may share only a same-interest zero-poll proof;
     * this cooperative window is not a hard wall-clock response guarantee.
     * Fresh listener/peer readiness can wait only for this bounded batch;
     * its bytes and every wake publication remain registered and unconsumed. */
    int task_io_ready=task_deadline==0u && wait_ms==0 && active.count;
    int task_io_structural_urgent=owner_work || !task_io_ready || !started || !listening || listen_failure
        || pending.count
        || rejection.phase!=KU_HTTP_IO_FREE || now>=health_deadline;
#if defined(KU_HTTP_HAS_TASK_NET)
    if (net_count) task_io_structural_urgent=1;
#endif
    /* The owner has not admitted, dispatched or retired a connection since
     * the interest pass. Reuse its structural facts, but test the minimum
     * deadline against this original late sample, never the earlier scan. */
    if (!task_io_eligibility.all_batchable || now>=task_io_eligibility.deadline
        || (task_io_eligibility.has_idle && owner->connection_generation==UINT64_MAX))
      task_io_structural_urgent=1;
    if (task_io_eligibility.has_idle && task_io_pending_seen) task_io_structural_urgent=1;
#if !defined(_WIN32)
    /* An already-published wake is an immediate reason to service the pipe.
     * This is only an acquire observation: the original drain/clear and the
     * producer's byte publication remain the no-lost-wakeup protocol. */
    if (atomic_load_explicit(&wake.pending,memory_order_acquire)) task_io_structural_urgent=1;
#else
    /* Windows' UDP wake has no coalescing marker. Keep its original I/O turn
     * rather than adding a new notification protocol to this optimization. */
    task_io_structural_urgent=1;
#endif
    int task_io_proof_missing=task_io_eligibility.has_idle
        && !ku_http_task_io_batch_probe_matches(&task_io_batch,
            owner->connection_generation,active.count,accept_turn);
    int task_io_urgent=task_io_structural_urgent || task_io_proof_missing;
    /* Eligibility scans/locks can consume time too. Never use the earlier
     * deadline sample to extend the OS-service batch past its time bound. */
    unsigned long long batch_now=task_io_urgent ? now : ku_http_now_ms();
    if (task_io_eligibility.has_idle && batch_now>=task_io_eligibility.deadline) {
      task_io_structural_urgent=1; task_io_urgent=1;
    }
    int task_io_probe_can_bootstrap=task_io_eligibility.has_idle
        && !task_io_structural_urgent;
    if (!task_io_slice_turn.force_io && ku_http_task_io_batch_continue(&task_io_batch,batch_now,
        task_io_ready,task_io_urgent)) continue;
    if (task_io_slice_turn.force_io) {
      ku_http_task_io_batch_reset(&task_io_batch);
    }
    wait_ms=ku_http_task_slice_wait_ms(&task_io_slice_turn,wait_ms);
#endif
    /* No global wake tick: progress blocks until readiness, worker publication
       or the nearest deadline (including only Darwin's paused-handler error probe). */
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    ku_http_reactor_interest(&reactor,(size_t)slot_count,wake.read_socket,KU_HTTP_POLL_READ,0,wake_index);
    ku_http_reactor_interest(&reactor,(size_t)slot_count+1u,
        listener_index>=0?srv:KU_HTTP_INVALID_SOCKET,KU_HTTP_POLL_READ,0,listener_index);
    ku_http_reactor_interest(&reactor,(size_t)slot_count+2u,
        rejection.poll_index>=0?rejection.socket_value:KU_HTTP_INVALID_SOCKET,
        rejection.phase==KU_HTTP_IO_REJECT?KU_HTTP_POLL_READ:KU_HTTP_POLL_WRITE,0,rejection.poll_index);
#if defined(KU_HTTP_HAS_TASK_NET)
    /* The driver's bounded snapshot is compact and may reorder live FDs.
     * Detach every changed identity before adding any replacement, otherwise
     * swapping two entries could delete a just-added generation or hit EEXIST. */
    for(size_t i=0;i<reactor.old_net_count;i++) {
      KuHttpRegistration* entry=&reactor.registrations[(size_t)slot_count+3u+i];
      if(i>=net_count || entry->fd!=poll_items[net_base+(int)i].fd
          || entry->generation!=net_interests[i].generation || entry->resource_slot!=net_interests[i].slot)
        ku_http_reactor_delete(&reactor,(size_t)slot_count+3u+i);
    }
    for(size_t i=0;i<net_count;i++)
    {
      ku_http_reactor_interest(&reactor,(size_t)slot_count+3u+i,poll_items[net_base+(int)i].fd,
          poll_items[net_base+(int)i].events,net_interests[i].generation,net_base+(int)i);
      reactor.registrations[(size_t)slot_count+3u+i].resource_slot=net_interests[i].slot;
    }
    reactor.old_net_count=net_count;
#endif
    for(int node=pending.head;node>=0;node=pending.items[node].next) {
      KuHttpPendingItem* item=&pending.items[node];
      if(item->kind==KU_HTTP_PENDING_RAW)
        ku_http_reactor_interest(&reactor,reactor.pending_base+(size_t)node,
            item->poll_index>=0?item->socket_value:KU_HTTP_INVALID_SOCKET,0,
            item->queue_generation,item->poll_index);
    }
    if(reactor.idle_count && (reactor.draining
        || (pending.raw_count && ku_http_free_slot(&active)<0))) wait_ms=0;
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    /* Registrations may publish Task work after the earlier eligibility pass.
     * Only this thread can set this flag; once taken, no self-publication can
     * interleave before the OS call. Other threads retain the real wake FD.
     * This turn cannot certify an idle socket or start a computation slice. */
    if(ku_http_task_take_owner_work(owner)) {
      wait_ms=0;task_io_structural_urgent=1;task_io_probe_can_bootstrap=0;
    }
#endif
    if(ku_http_queue_arm_wait(&queue,wait_ms==0?0:nearest)) {
      wait_ms=0;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
      task_io_structural_urgent=1;task_io_probe_can_bootstrap=0;
#endif
    }
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    int selected=ku_http_reactor_wait(&reactor,poll_items,wait_ms);
#else
    int selected = ku_http_poll(poll_items, (size_t)poll_count, wait_ms);
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    ku_http_task_io_batch_probe_result(&task_io_batch,wait_ms,selected,
        task_io_probe_can_bootstrap,batch_now,owner->connection_generation,active.count,accept_turn);
#endif
    if (selected < 0) {
      if (ku_http_socket_error_interrupted(ku_http_socket_last_error())) continue;
      if (!listen_failure) listen_failure = "reactor wait failed";
      else { fputs("http reactor wait repeatedly failed\n", stderr); exit(1); }
      continue;
    }
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    /* Only a real, successful OS return pays the owed I/O turn. EINTR or
     * earlier maintenance continues preserve the obligation above. A zero
     * result can now grant a NEW bounded computation slice, never renew the
     * old no-readiness observation inside that slice. */
    ku_http_task_slice_observed_io(&task_io_slice_turn,selected);
    int task_slice_allowed=!selected && !task_io_structural_urgent;
#if defined(KU_HTTP_HAS_TASK_NET)
    if (net_count || net_deadline!=UINT64_MAX) task_slice_allowed=0;
#endif
    if (task_io_eligibility.deadline<task_io_slice_deadline)
      task_io_slice_deadline=task_io_eligibility.deadline;
    if (task_slice_allowed && task_io_slice_deadline>batch_now
        && !ku_http_task_slice_interrupted(&wake)) {
      ku_http_task_io_batch_reset(&task_io_batch);
      uint32_t sliced=ku_http_task_compute_slice(group->tasks,(size_t)owner->index,
          batch_now,task_io_slice_deadline,
          ku_http_task_slice_interrupted,&wake,&task_io_slice_turn.next_deadline,&task_io_slice_turn.force_io);
      if (sliced>1u && !listen_failure) listen_failure="HTTP Task computation slice failed";
      continue; /* Fresh maintenance/interests before any subsequent OS wait. */
    }
#endif
    if (!selected) continue;
#if defined(KU_HTTP_HAS_TASK_NET)
    for (size_t i=0;i<net_count;i++) {
      short ready=poll_items[net_base+(int)i].revents;
      if (!ready) continue;
      uint32_t events=0;
      if (ready&KU_HTTP_POLL_READ) events|=KU_TASK_NET_READ;
      if (ready&KU_HTTP_POLL_WRITE) events|=KU_TASK_NET_WRITE;
      if (ready&(POLLERR|POLLNVAL)) events|=KU_TASK_NET_ERROR;
      if (ready&POLLHUP) events|=KU_TASK_NET_HUP;
      if (ku_http_task_net_dispatch(group->tasks,(size_t)owner->index,
          net_interests[i].slot,net_interests[i].generation,events)>1u)
        listen_failure="Task network readiness dispatch failed";
    }
#endif
    for(int node=pending.head;node>=0;) {
      KuHttpPendingItem* item=&pending.items[node];int next=item->next;
      if(item->kind==KU_HTTP_PENDING_RAW && item->poll_index>=0)
        ku_http_pending_ready(&pending,node,poll_items[item->poll_index].revents);
      node=next;
    }
    short wake_events = poll_items[wake_index].revents;
    if (wake_events & POLLNVAL) { fputs("http reactor wake socket invalid\n", stderr); exit(1); }
    if (wake_events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)) ku_http_wake_drain(&wake);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
    for(size_t selected_connection=0;selected_connection<reactor.ready_count;selected_connection++) {
      const KuHttpReadyEvent* event=&reactor.ready[selected_connection];
      if(!ku_http_reactor_event_valid(&reactor,event)) continue;
      KuHttpConnection* connection=&connections[event->entry];
      if(connection->active_set!=&active || connection->active_position<0
          || connection->connection_generation!=event->generation || connection->socket_value!=event->fd) continue;
      short events=event->events;
#else
    for (int selected_connection=0;selected_connection<connection_poll_count;selected_connection++) {
      int poll_index=connection_poll_base+selected_connection;
      /* Poll entries borrow this exact owner/slot/socket generation. Close
         may swap the dense active tail, but never changes this snapshot map. */
      KuHttpConnection* connection=ku_http_polled_connection(&active,
          &poll_connections[selected_connection],poll_index,poll_items[poll_index].fd);
      if(!connection) continue;
      short events=poll_items[poll_index].revents;
#endif
      if (!events) continue;
      if (events & POLLNVAL) { ku_http_connection_close(connection); continue; }
      if (connection->phase == KU_HTTP_IO_HEADER && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)))
        ku_http_connection_read_header(connection, server, &queue, spawned);
      else if (connection->phase == KU_HTTP_IO_BODY && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)))
        ku_http_connection_read_body(connection, server, &queue, spawned);
      else if (connection->phase == KU_HTTP_IO_WRITE && (events & (KU_HTTP_POLL_WRITE | POLLERR | POLLHUP)))
        ku_http_connection_write(connection, server);
      else if ((connection->phase==KU_HTTP_IO_ASYNC || connection->phase==KU_HTTP_IO_JOB)
          && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP))) ku_http_connection_observe_peer(connection,events,0);
      else if(connection->phase==KU_HTTP_IO_REQUEST_WAIT && (events&(POLLERR|POLLHUP))) {
        ku_http_connection_observe_peer(connection,events,0);
        if(connection->phase==KU_HTTP_IO_REQUEST_WAIT)
          connection->request_wait_probe_deadline=ku_http_deadline_ms(KU_HTTP_PAUSED_PEER_PROBE_MS);
      }
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
      ku_http_reactor_refresh(connection);
#endif
    }
    if (rejection.poll_index >= 0) {
      short events = poll_items[rejection.poll_index].revents;
      if (events & POLLNVAL) ku_http_connection_close(&rejection);
      else if (rejection.phase == KU_HTTP_IO_REJECT && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)))
        ku_http_reject_read(&rejection, server);
      else if (rejection.phase == KU_HTTP_IO_WRITE && (events & (KU_HTTP_POLL_WRITE | POLLERR | POLLHUP)))
        ku_http_connection_write(&rejection, server);
    }
    /* Existing pending connections always acquire newly free slots first. */
    if (!listen_failure) ku_http_pending_activate(&pending, &active, server);
    if (listener_index < 0 || !poll_items[listener_index].revents) continue;
    if (poll_items[listener_index].revents & POLLNVAL) { listen_failure = "listener descriptor invalid"; continue; }
    for (int turn = 0; turn < 32 && listening && rejection.phase == KU_HTTP_IO_FREE; turn++) {
      ku_http_mutex_lock(&group->lock);
      if (group->draining || group->failure || group->accept_turn != owner->index) {
        ku_http_mutex_unlock(&group->lock); break;
      }
      /* Only this owner can advance its turn, and it cannot reenter accept.
       * Even a nonblocking listener may sleep in kernel FD allocation: do not
       * hold the lifecycle/admission mutex across that syscall. The listener
       * itself survives until all owner threads have joined. */
      ku_http_mutex_unlock(&group->lock);
      KuHttpSocket cli = accept(srv, NULL, NULL);
      int accept_error = cli == KU_HTTP_INVALID_SOCKET ? ku_http_socket_last_error() : 0;
      ku_http_mutex_lock(&group->lock);
      /* Drain/failure may have linearized while accept was outside the lock.
       * Such a socket never receives C/A/P ownership or a handler submission.
       * Close outside the group lock, just as the ordinary socket paths do. */
      if (group->draining || group->failure || group->accept_turn != owner->index) {
        ku_http_mutex_unlock(&group->lock);
        if (cli != KU_HTTP_INVALID_SOCKET) ku_http_socket_close(cli);
        break;
      }
      if (cli != KU_HTTP_INVALID_SOCKET) {
        group->accepted++; owner->accepted++;
        /* Explicit loops may exceed C. Zero-C owners retain Task/net/wake
         * duties but never accept a transport they cannot record. */
        for(int step=1;step<=group->count;step++) {
          int next=(owner->index+step)%group->count;
          if(group->loops[next].slots) {group->accept_turn=next;break;}
        }
        KuHttpWake* next_wake = group->loops[group->accept_turn].wake;
        if (next_wake) ku_http_wake_notify(next_wake);
        if (group->max_requests && group->accepted >= group->max_requests) {
          group->draining = 1; group->drain_deadline = ku_http_deadline_ms(group->drain_ms);
        }
      }
      ku_http_mutex_unlock(&group->lock);
      if (cli == KU_HTTP_INVALID_SOCKET) {
        if (ku_http_socket_would_block(accept_error)) break;
        if (ku_http_socket_error_accept_peer_transient(accept_error)) {
          if (peer_accept_errors < KU_HTTP_ACCEPT_PEER_BACKOFF_CAP_MS) peer_accept_errors++;
          resource_accept_errors = 0;
          accept_retry_deadline = ku_http_deadline_ms((uint32_t)peer_accept_errors);
          break;
        }
        peer_accept_errors = 0;
        if (ku_http_socket_error_accept_resource_pressure(accept_error) && resource_accept_errors < KU_HTTP_ACCEPT_RESOURCE_RETRY_CAP) {
          resource_accept_errors++;
          accept_retry_deadline = ku_http_deadline_ms((uint32_t)resource_accept_errors * KU_HTTP_ACCEPT_RESOURCE_BACKOFF_STEP_MS);
          break;
        }
        listen_failure = "accept failed";
        break;
      }
      peer_accept_errors = resource_accept_errors = 0;
      accept_retry_deadline = 0;
      if (ku_http_socket_suppress_sigpipe(cli) != 0 || !ku_http_socket_nonblocking(cli)) {
        ku_http_socket_close(cli);
        listen_failure = "accepted socket configuration failed";
        break;
      }

      long live = ku_http_atomic_increment(&ku_http_active_conns);
      int slot = ku_http_free_slot(&active);
      /* C permits are partitioned with their stable owner records. A raw P
       * fd is allowed only while a closed job/Task still occupies a record;
       * accepting past local live C could otherwise strand a raw FIFO head
       * behind existing queued records with no record capable of retiring. */
      if (live > max_conn || active.sockets+(size_t)pending.raw_count>=(size_t)slot_count)
        ku_http_reject_start(&rejection, cli);
      else if (slot >= 0) ku_http_connection_start(&connections[slot], &active, cli, server);
      else if(!ku_http_pending_push_raw(&pending,cli,ku_http_deadline_ms(server->request_timeout_ms)))
        ku_http_reject_start(&rejection,cli);

    }
  }
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  ku_http_task_io_batch_reset(&task_io_batch);
#endif
  /* Finite N drains accepted reads, handler jobs, partial responses and their
     actual owner retirement before stopping workers. Fatal accept failure has
     already cancelled queued work, without touching a running handler's TLS. */
  ku_http_queue_close(&queue, listen_failure == NULL);
  if (spawned > 0) {
#if defined(_WIN32)
    if (WaitForMultipleObjects((DWORD)spawned, workers, TRUE, INFINITE) == WAIT_FAILED) {
      fputs("http worker join failed\n", stderr); exit(1);
    }
    for (int index = 0; index < spawned; index++) CloseHandle(workers[index]);
#else
    for (int w = 0; w < spawned; w++) {
      if (pthread_join(workers[w], NULL) != 0) { fputs("http worker join failed\n", stderr); exit(1); }
    }
#endif
  }
  free(workers);
  ku_http_queue_free(&queue);
  ku_http_mutex_lock(&group->lock); owner->wake = NULL; ku_http_mutex_unlock(&group->lock);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  ku_http_reactor_delete(&reactor,(size_t)slot_count);
#endif
  ku_http_wake_close(&wake);
#if defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  ku_http_reactor_destroy(&reactor);
#endif
  free(pending.items); free(poll_items); free(connections); free(active_indices); free(poll_connections);free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  if(ku_http_task_fixed_refund(group->tasks,fixed_bytes)!=0u)return "reactor fixed storage refund failed";
#endif
  return listen_failure;
}

#if defined(_WIN32)
static unsigned __stdcall ku_http_loop_thread(void* argument) {
#else
static void* ku_http_loop_thread(void* argument) {
#endif
  KuHttpLoopOwner* owner = (KuHttpLoopOwner*)argument;
  ku_http_current_loop = owner;
  const char* failure = ku_http_loop_run(owner);
  /* Allocation/wake/queue failure can return before the first owner pump.
     That real thread still owns its external driver slot: publish an empty
     drain and retire it here, rather than making the coordinator impersonate
     an already-created thread or wait forever for an impossible drained bit. */
  ku_http_mutex_lock(&owner->group->lock);
  if (failure && !owner->group->failure) owner->group->failure = failure;
  int early_failure = failure && !owner->drained;
  uint64_t cleanup_deadline = 0;
  if (early_failure) {
    owner->drained = 1;
    if (!owner->group->draining) {
      owner->group->draining = 1;
      owner->group->drain_deadline = ku_http_deadline_ms(owner->group->drain_ms);
    }
    cleanup_deadline = owner->group->drain_deadline;
    owner->group->tasks_closing = 1;
  }
  ku_http_mutex_unlock(&owner->group->lock);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  if (early_failure) {
    /* This owner was never ready, so the service could not admit a request.
       Preserve the old deadline and the driver's normal binding/exit path. */
    uint64_t states[8] = {0};
    ku_http_task_metrics(owner->group->tasks,(size_t)owner->index,states);
    if (states[3]) _Exit(80);
    /* request_close is idempotent. Another caller may have published
       tasks_closing but not reached the driver yet; do not race its call. */
    if (ku_http_task_runtime_close(owner->group->tasks,cleanup_deadline)>1u) _Exit(80);
    uint64_t next_deadline = UINT64_MAX;
    if (ku_http_task_pump(owner->group->tasks,(size_t)owner->index,1u,&next_deadline)>1u
        || !ku_http_task_owner_exited(owner->group->tasks,(size_t)owner->index)) _Exit(80);
  }
#else
  (void)cleanup_deadline;
#endif
  uint64_t final_cpu_ns=0, final_cpu_ms=ku_http_now_ms();
  int final_cpu_valid=ku_http_thread_cpu_ns(&final_cpu_ns);
  ku_http_mutex_lock(&owner->group->lock);
  ku_http_owner_cpu_sample(owner,final_cpu_ns,final_cpu_ms,final_cpu_valid);
  owner->ready = 0; owner->exited = 1;
  ku_http_mutex_unlock(&owner->group->lock);
  ku_http_group_notify(owner->group);
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

/* Reserved loopback management ingress. It is owned by the existing main
 * coordinator, has one physical connection and a separate 64 KiB ledger. Its
 * liveness answer still depends on every real event-loop heartbeat. */
static KuHttpSocket ku_http_management_open(const char* text) {
  char address[64]; size_t size = strlen(text);
  if (!size || size >= sizeof(address)) return KU_HTTP_INVALID_SOCKET;
  memcpy(address,text,size+1);
  char* colon = strrchr(address,':');
  if (!colon) return KU_HTTP_INVALID_SOCKET;
  *colon++=0;
  unsigned long host = htonl(INADDR_LOOPBACK);
  if (strcmp(address,"localhost") && !ku_http_parse_ipv4(address,&host)) return KU_HTTP_INVALID_SOCKET;
  if ((ntohl(host)>>24) != 127) return KU_HTTP_INVALID_SOCKET;
  unsigned long port=0;
  if (!*colon) return KU_HTTP_INVALID_SOCKET;
  for (const char* at=colon; *at; at++) {
    if (*at<'0' || *at>'9') return KU_HTTP_INVALID_SOCKET;
    port=port*10u+(unsigned long)(*at-'0');
    if (port>65535u) return KU_HTTP_INVALID_SOCKET;
  }
  if (!port) return KU_HTTP_INVALID_SOCKET;
  KuHttpSocket listener=socket(AF_INET,SOCK_STREAM,IPPROTO_TCP);
  if (listener==KU_HTTP_INVALID_SOCKET) return listener;
  int yes=1;
#if defined(_WIN32)
  if (setsockopt(listener,SOL_SOCKET,SO_EXCLUSIVEADDRUSE,(const char*)&yes,sizeof(yes))) goto failed;
#else
  if (setsockopt(listener,SOL_SOCKET,SO_REUSEADDR,(const char*)&yes,sizeof(yes))) goto failed;
#endif
  struct sockaddr_in endpoint; memset(&endpoint,0,sizeof(endpoint));
  endpoint.sin_family=AF_INET; endpoint.sin_addr.s_addr=host; endpoint.sin_port=htons((unsigned short)port);
  if (bind(listener,(struct sockaddr*)&endpoint,sizeof(endpoint)) || listen(listener,8) || !ku_http_socket_nonblocking(listener)) goto failed;
  return listener;
failed:
  ku_http_socket_close(listener); return KU_HTTP_INVALID_SOCKET;
}
static KuResult_null ku_http_listen(KuHttpServer* server, KuString address) {
  ku_http_validate_server(server);
  if (address.len == 0 || memchr(address.ptr, 0, address.len) != NULL) {
    ku_string_drop(&address);
    return ku_http_listen_err(server, "invalid listen address");
  }
  char* addr = ku_string_to_cstr(address);
  ku_string_drop(&address);
  unsigned long host = htonl(INADDR_LOOPBACK);
  unsigned long port = 0;
  char* colon = strchr(addr, ':');
  int valid_address = colon != NULL && strchr(colon + 1, ':') == NULL;
  if (valid_address) {
    *colon = '\0';
    const char* port_text = colon + 1;
    if (*port_text == '\0') valid_address = 0;
    while (valid_address && *port_text) {
      if (*port_text < '0' || *port_text > '9') { valid_address = 0; break; }
      port = port * 10UL + (unsigned long)(*port_text - '0');
      if (port > 65535UL) { valid_address = 0; break; }
      port_text++;
    }
    if (valid_address && addr[0] != '\0' && strcmp(addr, "localhost") != 0 &&
        !ku_http_parse_ipv4(addr, &host)) valid_address = 0;
  }
  if (!valid_address) {
    free(addr);
    return ku_http_listen_err(server, "invalid listen address");
  }
  free(addr);
  if (ku_http_net_init() != 0) return ku_http_listen_err(server, "network initialization failed");
  KuHttpSocket srv = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (srv == KU_HTTP_INVALID_SOCKET) { ku_http_net_cleanup(); return ku_http_listen_err(server, "socket failed"); }
  int yes = 1;
#if defined(_WIN32)
  if (setsockopt(srv, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, (const char*)&yes, sizeof(yes)) != 0) {
    ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "exclusive listener setup failed");
  }
#else
  setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, (const char*)&yes, sizeof(yes));
#endif
  struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET; sa.sin_addr.s_addr = host; sa.sin_port = htons((unsigned short)port);
  if (bind(srv, (struct sockaddr*)&sa, sizeof(sa)) != 0) { ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "bind failed"); }
  if (listen(srv, SOMAXCONN) != 0) { ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "listen failed"); }
  if (!ku_http_socket_nonblocking(srv)) {
    ku_http_socket_close(srv); ku_http_net_cleanup();
    return ku_http_listen_err(server, "listener nonblocking setup failed");
  }

  KuHttpLoopGroup group = {0};
  group.server = server; group.listener = srv;
  const char* control_token = getenv("KU_SERVICE_CONTROL_TOKEN");
  if (control_token) {
    size_t length = strlen(control_token);
    if (!length || length > 128) {
      ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "service control token length must be 1..128");
    }
    memcpy(group.control_token, control_token, length+1);
  }
  group.drain_ms = (uint32_t)ku_http_env_limit("KU_HTTP_DRAIN_TIMEOUT_MS", 5000, 300000);
  group.max_requests = (unsigned long long)ku_http_env_limit("KU_HTTP_MAX_REQUESTS", LONG_MAX, LONG_MAX);
  if (group.max_requests == (unsigned long long)LONG_MAX) group.max_requests = 0;
  /* Conservative default until the quota-aware target probe is applied. */
  group.count = (int)ku_runtime_loop_count();
  size_t blocking_reserved=0u;
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  blocking_reserved=ku_http_task_blocking_threads(1u);
#endif
  int blocking = (int)(ku_runtime_blocking_count_reserved(blocking_reserved)-blocking_reserved);
  if (!getenv("KU_EVENT_LOOPS")) {
    if (group.count > server->max_connections) group.count = (int)server->max_connections;
    if (group.count > server->max_active_requests) group.count = (int)server->max_active_requests;
    if (group.count > server->max_pending_requests) group.count = (int)server->max_pending_requests;
    if (group.count > blocking) group.count = blocking;
  }
  if (group.count<1 || blocking<1 || group.count > server->max_active_requests || group.count > server->max_pending_requests || blocking < group.count) {
    ku_http_socket_close(srv); ku_http_net_cleanup();
    return ku_http_listen_err(server, "event loops must fit active/pending limits and blocking_threads must cover every loop");
  }
  ku_http_prepare_fd_table(srv,server->max_connections,group.count);
  if (!ku_http_mutex_init(&group.lock)) {
    ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "loop group mutex initialization failed");
  }
  if (!ku_http_wake_init(&group.wake)) {
    ku_http_mutex_destroy(&group.lock); ku_http_socket_close(srv); ku_http_net_cleanup();
    return ku_http_listen_err(server, "loop group wake initialization failed");
  }
  KuHttpBufferBudget budget = {0}; budget.limit = (size_t)server->max_buffer_bytes;
  if (!ku_http_mutex_init(&budget.lock)) {
    ku_http_wake_close(&group.wake); ku_http_mutex_destroy(&group.lock);
    ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server, "buffer budget initialization failed");
  }
  server->buffer_budget = &budget;
  ku_http_atomic_store(&ku_http_active_conns, 0);
  ku_http_live_group = &group;
  unsigned long long startup_deadline = ku_http_deadline_ms(5000);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  size_t task_bytes = (size_t)ku_http_env_limit("KU_TASK_MAX_BYTES", 16777216, 1073741824);
  group.tasks = ku_http_task_runtime_new((size_t)group.count,(size_t)server->max_active_requests,
      task_bytes,&group,ku_http_task_event_wake,startup_deadline);
  if (!group.tasks) {
    ku_http_live_group=NULL; ku_http_mutex_destroy(&budget.lock); server->buffer_budget=NULL;
    ku_http_wake_close(&group.wake); ku_http_mutex_destroy(&group.lock);
    ku_http_socket_close(srv); ku_http_net_cleanup(); return ku_http_listen_err(server,"HTTP Task runtime startup failed; verify KU_TASK_MAX_BYTES against configured request capacity");
  }
#endif
  const int native_only_routes = server->has_native_task_route && !server->has_sync_route;
  for (int i=0; i<group.count; i++) {
    KuHttpLoopOwner* owner = &group.loops[i]; owner->group = &group; owner->index = i;
    owner->slots = (int)(server->max_connections/group.count) + (i < server->max_connections%group.count);
    owner->request_limit = (int)(server->max_active_requests/group.count) + (i < server->max_active_requests%group.count);
    owner->pending = (int)(server->max_pending_requests/group.count) + (i < server->max_pending_requests%group.count);
    owner->workers = native_only_routes ? 1 : blocking/group.count + (i < blocking%group.count);
    if (owner->workers > owner->request_limit) owner->workers = owner->request_limit;
#if defined(_WIN32)
    uintptr_t thread = _beginthreadex(NULL, (unsigned)ku_runtime_thread_stack_bytes(), ku_http_loop_thread, owner, 0, NULL);
    if (!thread) break;
    owner->thread = (HANDLE)thread;
#else
    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes) != 0) break;
    int status = pthread_attr_setstacksize(&attributes, ku_runtime_thread_stack_bytes());
    if (!status) status = pthread_create(&owner->thread, &attributes, ku_http_loop_thread, owner);
    pthread_attr_destroy(&attributes);
    if (status) break;
#endif
    ku_http_mutex_lock(&group.lock); group.created++; ku_http_mutex_unlock(&group.lock);
  }
  KuHttpSocket management_listener=KU_HTTP_INVALID_SOCKET;
  KuHttpConnection management={0}; management.socket_value=KU_HTTP_INVALID_SOCKET;
  KuHttpServer management_server=*server;
  KuHttpBufferBudget management_budget={0}; management_budget.limit=65536;
  int management_initialized=ku_http_mutex_init(&management_budget.lock);
  management_server.buffer_budget=&management_budget;
  management_server.max_header_bytes=4096; management_server.max_body_bytes=1;
  management_server.idle_timeout_ms=200; management_server.read_header_timeout_ms=200;
  management_server.read_body_timeout_ms=200; management_server.write_timeout_ms=200;
  management_server.request_timeout_ms=200;
  const char* management_address=getenv("KU_SERVICE_PROBE_ADDR");
  if (management_address && group.control_token[0] && management_initialized)
    management_listener=ku_http_management_open(management_address);
  ku_http_mutex_lock(&group.lock);
  if (!management_initialized || (management_address && management_listener==KU_HTTP_INVALID_SOCKET))
    group.failure="reserved loopback management listener initialization failed";
  if (group.created != group.count) group.failure = "event loop thread startup failed";
  ku_http_mutex_unlock(&group.lock);
  for (;;) {
    if (management.phase == KU_HTTP_IO_WRITE) ku_http_connection_write(&management,&management_server);
    if (management.phase != KU_HTTP_IO_FREE && ku_http_now_ms() >= management.deadline)
      ku_http_connection_close(&management);
    ku_http_mutex_lock(&group.lock);
    int ready = 0, exited = 0, drained = 0;
    for (int i=0; i<group.created; i++) { ready += group.loops[i].ready; exited += group.loops[i].exited; drained += group.loops[i].drained; }
    unsigned long long now = ku_http_now_ms();
    int notify_loops=ready==group.count && !group.started;
    if (notify_loops) group.started=1;
    if (!group.failure && !group.draining && ready != group.count && now >= startup_deadline) group.failure = "event loop startup deadline exceeded";
    if (group.failure && !group.draining) { group.draining = 1; group.drain_deadline = ku_http_deadline_ms(group.drain_ms); }
    if (group.draining && !group.drain_notified) { group.drain_notified=1; notify_loops=1; }
    int close_tasks = !group.tasks_closing && group.draining && drained == group.created;
    if (close_tasks) group.tasks_closing = 1;
    int done = exited == group.created;
    int expired = group.draining && now >= group.drain_deadline;
    ku_http_mutex_unlock(&group.lock);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    if (close_tasks) {
      uint64_t deadline = group.drain_deadline;
      if (ku_http_task_runtime_close(group.tasks,deadline) > 1u) _Exit(80);
      ku_http_group_notify(&group);
    }
#else
    (void)close_tasks;
#endif
    if (done && management.phase == KU_HTTP_IO_FREE) break;
    if (expired) {
      /* A native call cannot be killed as a thread. Process termination is the
         explicit final boundary, and the service owner reports uncertainty. */
      _Exit(75);
    }
    if (notify_loops) ku_http_group_notify(&group);
    KuHttpPollFd items[3]; int count = 0;
    ku_http_poll_add(items,&count,group.wake.read_socket,KU_HTTP_POLL_READ);
    int accept_index=-1,connection_index=-1;
    if (!done && management_listener!=KU_HTTP_INVALID_SOCKET && management.phase==KU_HTTP_IO_FREE) {
      accept_index=count;
      ku_http_poll_add(items,&count,management_listener,KU_HTTP_POLL_READ);
    }
    if (management.phase!=KU_HTTP_IO_FREE) {
      connection_index=count;
      ku_http_poll_add(items,&count,management.socket_value,
          management.phase==KU_HTTP_IO_WRITE ? KU_HTTP_POLL_WRITE : KU_HTTP_POLL_READ);
    }
    int wait_ms=500;
    if (management.phase!=KU_HTTP_IO_FREE) {
      uint64_t current=ku_http_now_ms();
      wait_ms=management.deadline>current ? (int)(management.deadline-current) : 0;
      if (wait_ms>200) wait_ms=200;
    }
    int selected=ku_http_poll(items,(size_t)count,wait_ms);
    if (selected>0) {
      if (items[0].revents) ku_http_wake_drain(&group.wake);
      if (connection_index>=0 && items[connection_index].revents) {
        if (management.phase==KU_HTTP_IO_WRITE) ku_http_connection_write(&management,&management_server);
        else if (management.phase==KU_HTTP_IO_HEADER)
          ku_http_connection_read_header(&management,&management_server,NULL,0);
        else ku_http_connection_close(&management);
      }
      if (accept_index>=0 && items[accept_index].revents) {
        KuHttpSocket client=accept(management_listener,NULL,NULL);
        if (client!=KU_HTTP_INVALID_SOCKET) {
          if (!ku_http_socket_nonblocking(client) || ku_http_socket_suppress_sigpipe(client)) ku_http_socket_close(client);
          else {
            ku_http_connection_start(&management,NULL,client,&management_server);
            management.management=1;
          }
        }
      }
    } else if (selected<0 && !ku_http_socket_error_interrupted(ku_http_socket_last_error())) {
      ku_http_mutex_lock(&group.lock); group.failure="loop group wait failed"; ku_http_mutex_unlock(&group.lock);
    }
  }
  for (int i=0; i<group.created; i++) {
#if defined(_WIN32)
    if (WaitForSingleObject(group.loops[i].thread, INFINITE) != WAIT_OBJECT_0) _Exit(76);
    CloseHandle(group.loops[i].thread);
#else
    if (pthread_join(group.loops[i].thread, NULL) != 0) _Exit(76);
#endif
  }
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  /* No request can be admitted before every loop is ready. Owners whose OS
     thread could not be created therefore have no tasks and have never bound
     to a thread. Bind only these unused owners here and let the normal closing
     state machine retire them; never move a previously active owner. */
  for (int i=group.created; i<group.count; i++) {
    uint64_t ignored_deadline=UINT64_MAX;
    if (ku_http_task_pump(group.tasks,(size_t)i,1,&ignored_deadline)>1u) _Exit(80);
  }
  uint64_t cleanup_deadline = ku_http_deadline_ms(KU_HANDLER_CLEANUP_GRACE_MS);
  if (group.drain_deadline && group.drain_deadline < cleanup_deadline) cleanup_deadline = group.drain_deadline;
  for (;;) {
    uint32_t freed = ku_http_task_runtime_free(group.tasks,cleanup_deadline);
    if (!freed) break;
    if (freed != 1u || ku_http_now_ms() >= cleanup_deadline) _Exit(80);
    KuHttpPollFd item; int count = 0;
    ku_http_poll_add(&item,&count,group.wake.read_socket,KU_HTTP_POLL_READ);
    uint64_t remaining = cleanup_deadline-ku_http_now_ms();
    int selected = ku_http_poll(&item,1,(int)(remaining > 1000 ? 1000 : remaining));
    if (selected > 0) ku_http_wake_drain(&group.wake);
  }
#endif
  ku_http_connection_close(&management);
  ku_http_socket_close(management_listener);
  if (management_initialized) ku_http_mutex_destroy(&management_budget.lock);
  ku_http_live_group = NULL;
  ku_http_socket_close(srv);
  if (budget.current) _Exit(77);
  ku_http_mutex_destroy(&budget.lock); server->buffer_budget = NULL;
  ku_http_wake_close(&group.wake); ku_http_mutex_destroy(&group.lock);
  ku_http_net_cleanup();
  if (group.failure) return ku_http_listen_err(server, group.failure);
  ku_http_server_free(server);
  return (KuResult_null){ true, 0, (KuError){0} };
}
