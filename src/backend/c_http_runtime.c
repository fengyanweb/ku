#define KU_NATIVE_RUNTIME_HTTP_SOCKET 1
#if !defined(_WIN32)
#include <sys/uio.h>
#endif
#if defined(__APPLE__)
#include <sys/select.h>
#endif
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
} KuHttpInput;
static void ku_http_input_drop(KuHttpInput* input) {
  if (!input) return;
  if (input->headers) ku_object_drop(input->headers);
  free(input->target);
  free(input->body);
  ku_http_buffer_refund(input->budget, input->body_charge);
  ku_http_buffer_refund(input->budget, input->metadata_charge);
  *input = (KuHttpInput){0};
}
static KuStruct___ku_http_response ku_http_status_response(int64_t status, const char* message) {
  KuStruct___ku_http_response response = {0};
  response.status = status;
  response.content_type = ku_string_static((const uint8_t*)"text/plain; charset=utf-8", 25);
  response.body = ku_string_static((const uint8_t*)message, strlen(message));
  return response;
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
  size_t header_fields=0;
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
          for (size_t k = vs; k < ve; k++) if (hdr[k] < '0' || hdr[k] > '9') { return 400; }
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
  size_t header_capacity=ku_http_map_capacity(header_fields);
  size_t metadata_bound = target_len+1+hlen+sizeof(KuObject)+header_capacity*sizeof(KuEntry);
  if (!ku_http_buffer_reserve(input->budget, metadata_bound)) return 503;
  input->metadata_charge = metadata_bound;
  char* target = (char*)malloc(target_len + 1);
  if (!target) return 500;
  input->target = target;
  memcpy(target, hdr + target_start, target_len); target[target_len] = '\0';
  /* Header map (lowercased names, trimmed values) matching the interpreter, plus a
     strict content-length parse: present-but-non-numeric -> 400. */
  KuObject* headers = ku_http_parse_headers(hdr,(int)hlen,header_capacity);
  input->headers = headers;
  size_t metadata_bytes = target_len+1+sizeof(KuObject)+headers->cap*sizeof(KuEntry);
  for (size_t slot=0; slot<headers->cap; slot++) {
    KuEntry* entry=&headers->entries[slot];
    if (!entry->used) continue;
    if (entry->key.storage==KU_STRING_OWNED) metadata_bytes+=entry->key.capacity;
    if (entry->value.tag==KU_STR && entry->value.as.s.storage==KU_STRING_OWNED)
      metadata_bytes+=entry->value.as.s.capacity;
  }
  if (metadata_bytes > input->metadata_charge) { return 500; }
  ku_http_buffer_refund(input->budget, input->metadata_charge-metadata_bytes);
  input->metadata_charge=metadata_bytes;
  /* Content-Length is strict RFC decimal: one or more digits, no sign. Duplicate
     fields and Transfer-Encoding were rejected before the map was built.

     The accumulator is unsigned WITH an explicit overflow guard. A signed
     accumulator wraps on a huge value (undefined behaviour), and the wrapped
     negative result would then slip past the 413 check and reach the handler with
     an empty body and a 200 -- where the interpreter answers 400. */
  unsigned long long content_length = 0;
  { KuValue* cv = ku_object_get(headers, ku_string_static((const uint8_t*)"content-length", 14));
    if (cv && cv->tag == KU_STR) {
      KuString s = cv->as.s;
      unsigned long long v = 0; int ok = 1; size_t k = 0;
      if (s.len == 0) ok = 0;
      for (; ok && k < s.len; k++) {
        uint8_t d = ((uint8_t*)s.ptr)[k];
        if (d < '0' || d > '9') { ok = 0; break; }
        if (v > (~0ULL - (unsigned long long)(d - '0')) / 10ULL) { ok = 0; break; }
        v = v * 10ULL + (unsigned long long)(d - '0');
      }
      if (!ok) { return 400; }
      content_length = v;
    }
  }
  if (content_length > (unsigned long long)max_body) { return 413; }
  char* qmark = strchr(target, '?');
  size_t path_len = qmark ? (size_t)(qmark - target) : strlen(target);
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
static size_t ku_http_projection_bytes(KuHttpInput* input,size_t parameters,size_t parameter_names) {
  size_t target=strlen(input->target),method=strlen(input->method);
  size_t query_capacity=ku_http_map_capacity(ku_http_query_fields(input->target,input->path_len));
  size_t parameter_capacity=ku_http_map_capacity(parameters);
  /* Bounds include duplicate query values until overwritten and all route
     parameter names/values. Target/header limits bound each arithmetic term. */
  return 2*sizeof(KuObject)+(query_capacity+parameter_capacity)*sizeof(KuEntry)
      +target*3+input->body_len+method+parameter_names+sizeof(KuStruct___ku_http_request);
}
static int ku_http_job_projection(KuHttpJob*,size_t);
static void ku_http_job_begin(KuHttpJob* job, unsigned long long timeout_ms);
/* Only a handler worker calls this function. All synchronous exit/deadline TLS
   remains on that same execution thread; socket ownership never enters it. */
static KuStruct___ku_http_response ku_http_execute_request(KuHttpServer* server, KuHttpInput* input, KuHttpJob* job) {
  const char* method = input->method;
  const char* target = input->target;
  size_t path_len = input->path_len;
  const char* body_buf = input->body;
  size_t body_len = input->body_len;
  /* Sized off the target limit, not a smaller fixed buffer: normalization only
     ever adds a leading '/', so this cannot truncate a target that passed the 414
     check -- and a truncated normalized path would reintroduce exactly the route
     collision the 414 check exists to prevent. */
  char norm[KU_HTTP_MAX_TARGET + 2];
  ku_http_normalize_path(target, path_len, norm, sizeof(norm));
  const char* segs[KU_HTTP_MAX_SEGS]; size_t seglens[KU_HTTP_MAX_SEGS]; size_t nseg = 0;
  { size_t nl = strlen(norm); size_t j = 0;
    while (j < nl) {
      while (j < nl && norm[j] == '/') j++;
      size_t st = j;
      while (j < nl && norm[j] != '/') j++;
      if (j > st) {
        if (nseg >= KU_HTTP_MAX_SEGS) { return ku_http_status_response(414, "URI Too Long"); }
        segs[nseg] = norm + st; seglens[nseg] = j - st; nseg++;
      }
    }
  }
  const char* param_values[KU_HTTP_MAX_SEGS];
  size_t param_value_lens[KU_HTTP_MAX_SEGS];
  size_t param_count = 0;
  KuHttpHandler* route = ku_http_trie_match(server->root, segs, seglens, nseg, 0, method, param_values, param_value_lens, &param_count);
  if (route && param_count != route->nparams) {
    ku_http_failure_current.reason=KU_HTTP_ERROR_PARSER_INVARIANT;
    return ku_http_status_response(500, "Internal Server Error");
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
      KuObject* params = ku_object_new(ku_http_map_capacity(route->nparams));
      for (size_t p = 0; p < route->nparams; p++) {
        KuString key = ku_http_string_copy(route->param_names[p], strlen(route->param_names[p]));
        KuString val = ku_http_string_copy(param_values[p], param_value_lens[p]);
        ku_object_set(params, key, ku_v_str(val));
      }
      KuStruct___ku_http_request req;
      req.method = ku_http_string_copy(method, strlen(method));
      req.path = ku_http_string_copy(target, path_len);
      req.body = ku_http_string_copy(body_buf ? body_buf : "", body_len);
      req.params = params;
      req.query = ku_http_parse_query(target, path_len);
      req.headers = input->headers; input->headers = NULL;
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
    if (ku_http_trie_path_exists(server->root, segs, seglens, nseg, 0)) {
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
/* One shared socket owner, bounded input slots, and handler-only workers.
   A slot remains charged while its independent job has any worker/queue owner,
   even after its socket closes. Thus at most max_active slot/job obligations
   coexist, plus max_pending accepted sockets and one bounded 503 owner. */
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
} KuHttpQueue;
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
  ku_http_wake_notify(queue->wake);
}
static int ku_http_job_only_owner(KuHttpJob* job) {
  size_t current = ku_atomic_refcount_load(&job->owners);
  for (;;) {
    size_t expected = current;
    if (ku_atomic_refcount_compare_exchange_acq_rel(&job->owners, &expected, current)) return current == 1;
    current = expected;
  }
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
  ku_http_wake_notify(job->queue->wake);
}
static void ku_http_job_run(KuHttpServer* server, KuHttpJob* job) {
  ku_http_mutex_lock(&job->lock);
  int cancelled = job->cancelled || ku_http_now_ms() >= job->request_deadline;
  if (cancelled) { job->cancelled = 1; job->completed = 1; }
  ku_http_mutex_unlock(&job->lock);
  if (cancelled) { ku_http_input_drop(&job->input); ku_http_wake_notify(job->queue->wake); return; }
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
  ku_http_wake_notify(job->queue->wake);
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
  ku_http_mutex_unlock(&queue->lock);
  if (job) ku_http_wake_notify(queue->wake);
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
  char first[256]; char last[128];
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
  KU_HTTP_IO_ASYNC = 8, KU_HTTP_IO_CLOSED_ASYNC = 9
};
typedef struct KuHttpActiveSet {
  struct KuHttpConnection* connections;
  int* indices;
  int count,capacity,free_head,high_water;
  size_t sockets;
} KuHttpActiveSet;
typedef struct KuHttpConnection {
  KuHttpSocket socket_value;
  KuHttpBufferBudget* budget;
  KuHttpActiveSet* active_set;
  int active_position,free_next;
  int phase; int poll_index; int management;
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
  KuHttpOutput output;
  size_t rejected_bytes; size_t rejected_seen; unsigned char rejected_window[4];
} KuHttpConnection;
typedef struct { int slot; uint64_t generation; } KuHttpPollConnection;
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
}
static void ku_http_active_retire(KuHttpConnection* connection) {
  KuHttpActiveSet* active=connection->active_set;
  if(!active || connection->active_position<0) return;
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
  ku_http_active_retire(connection);
  KuHttpActiveSet* active=connection->active_set;
  int free_next=connection->free_next;
  memset(connection, 0, sizeof(*connection));
  connection->socket_value = KU_HTTP_INVALID_SOCKET;
  connection->active_set=active; connection->active_position=-1; connection->free_next=free_next;
}
static void ku_http_connection_response(KuHttpConnection* connection, KuHttpServer* server, KuStruct___ku_http_response response, size_t response_charge, int admitted) {
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
  memset(connection, 0, sizeof(*connection));
  connection->active_set=active; connection->active_position=position;
  connection->socket_value = socket_value;
  connection->budget = server->buffer_budget;
  connection->phase = KU_HTTP_IO_HEADER;
  ku_http_connection_identity(connection);
  connection->request_sequence=1;
  connection->request_deadline = ku_http_deadline_ms(server->request_timeout_ms);
  if (!ku_http_buffer_reserve(connection->budget, 1024)) { connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection, server, 503); return; }
  connection->capacity = 1024;
  connection->header = (char*)malloc(connection->capacity);
  connection->deadline = ku_http_deadline_ms((uint32_t)(server->idle_timeout_ms > 0 ? server->idle_timeout_ms : 5000));
  if (!connection->header) { connection->failure.reason=KU_HTTP_ERROR_ALLOCATION; ku_http_connection_status(connection, server, 500); }
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
  ku_drop_struct___ku_http_response(&connection->output.response);
  ku_http_buffer_refund(connection->budget, connection->output.charge); connection->output.charge = 0;
  memset(&connection->output, 0, sizeof(connection->output));
  connection->keep_alive = 0; connection->reused = 1;
  if(connection->request_sequence!=UINT64_MAX) connection->request_sequence++;
  connection->request_id_length=0; connection->request_id_truncated=0;
  connection->peer_read_paused=0;
  connection->failure=(KuHttpFailure){0};
  connection->request_deadline = 0;
  connection->buffered = 0; connection->body_received = 0;
  if (!ku_http_buffer_reserve(connection->budget, 1024)) { connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection, server, 503); return; }
  connection->capacity = 1024;
  connection->header = (char*)malloc(connection->capacity);
  connection->phase = KU_HTTP_IO_HEADER;
  connection->deadline = ku_http_deadline_ms((uint32_t)(server->idle_timeout_ms > 0 ? server->idle_timeout_ms : 5000));
  if (!connection->header) { connection->failure.reason=KU_HTTP_ERROR_ALLOCATION; ku_http_connection_status(connection, server, 500); }
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
  KuValue* rid=connection->input.headers ? ku_object_get(connection->input.headers,
      ku_string_static((const uint8_t*)"x-request-id",12)) : NULL;
  if(rid && rid->tag==KU_STR) {
    size_t n=rid->as.s.len;
    if(n>sizeof(connection->request_id)) { n=sizeof(connection->request_id); connection->request_id_truncated=1; }
    memcpy(connection->request_id,rid->as.s.ptr,n); connection->request_id_length=(uint32_t)n;
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
  if (!workers || !ku_http_queue_push(queue, job)) {
    ku_http_count_transport_close(connection,KU_HTTP_CLOSE_QUEUE_NOT_ADMITTED);
    ku_http_connection_close(connection);
  } else ku_http_count_admitted();
}
static void ku_http_connection_read_header(KuHttpConnection* connection, KuHttpServer* server, KuHttpQueue* queue, int workers) {
  if (ku_http_now_ms() >= connection->deadline) { ku_http_connection_status(connection, server, 408); return; }
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
  if (!scan_from) connection->deadline = ku_http_deadline_ms((uint32_t)(server->read_header_timeout_ms > 0 ? server->read_header_timeout_ms : 5000));
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
static void ku_http_connection_write(KuHttpConnection* connection, KuHttpServer* server) {
  KuHttpOutput* output = &connection->output;
  size_t bytes = 0;
  /* Each readiness turn is finite even for a peer that remains writable and a
     very large Ku response. Neither partial sends nor EINTR refresh write D. */
  for (int turn = 0; turn < 32 && bytes < (size_t)262144; turn++) {
    if (ku_http_now_ms() >= connection->deadline) {
      ku_http_count_transport_close(connection,KU_HTTP_CLOSE_WRITE_DEADLINE);
      ku_http_connection_close(connection); return;
    }
    if (output->part >= output->count) { ku_http_connection_next_request(connection, server); return; }
    size_t remaining=(size_t)262144-bytes;
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
  memset(connection, 0, sizeof(*connection));
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
/* Pending sockets carry permits, but no input allocation, job, or worker. */
typedef struct { KuHttpSocket socket_value; unsigned long long deadline; } KuHttpPendingItem;
typedef struct { KuHttpPendingItem* items; int cap; int head; int tail; int count; } KuHttpPending;
static void ku_http_pending_activate(KuHttpPending* pending, KuHttpActiveSet* active, KuHttpServer* server) {
  while (pending->count) {
    int slot = ku_http_free_slot(active);
    if (slot < 0) break;
    KuHttpPendingItem item = pending->items[pending->head];
    KuHttpSocket socket_value = item.socket_value;
    pending->head = (pending->head + 1) % pending->cap;
    pending->count--;
    ku_http_connection_start(&active->connections[slot], active, socket_value, server);
    active->connections[slot].request_deadline = item.deadline;
  }
}
static void ku_http_pending_close(KuHttpPending* pending) {
  while (pending->count) {
    KuHttpPendingItem item = pending->items[pending->head];
    KuHttpSocket socket_value = item.socket_value;
    pending->head = (pending->head + 1) % pending->cap;
    pending->count--;
    ku_http_socket_close(socket_value);
    ku_http_atomic_decrement(&ku_http_active_conns);
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
 * Only the caller owns/closes the shared nonblocking listener. A short accept
 * mutex transfers each new socket directly to exactly one loop; reads, writes,
 * parser state and completion observation never move between loops. */
typedef struct KuHttpLoopGroup KuHttpLoopGroup;
typedef struct KuHttpLoopOwner {
  KuHttpLoopGroup* group;
  int index, slots, pending, workers, ready, exited, drained;
  unsigned long long heartbeat, accepted, requests, admitted, success, rejected, timed_out, other_responses, peak_loop_delay_ms;
  KuHttpConnection* connection_base; uint64_t connection_generation;
  uint64_t cpu_ns, cpu_sample_ms, cpu_clock_errors; /* Published under group->lock. */
  unsigned long long transport_closes[KU_HTTP_CLOSE_REASON_COUNT];
  size_t resident, queued, running, connections, pending_count, queued_bytes, peak_queued_bytes;
  KuHttpWake* wake;
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
static void ku_http_task_event_wake(void* context, size_t owner) {
  KuHttpLoopGroup* group = (KuHttpLoopGroup*)context;
  ku_http_mutex_lock(&group->lock);
  if (owner < (size_t)group->created && group->loops[owner].wake) ku_http_wake_notify(group->loops[owner].wake);
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
  /* Sized off the target limit, not a smaller fixed buffer: normalization only
     ever adds a leading '/', so this cannot truncate a target that passed the 414
     check -- and a truncated normalized path would reintroduce exactly the route
     collision the 414 check exists to prevent. */
  char norm[KU_HTTP_MAX_TARGET + 2];
  ku_http_normalize_path(target, path_len, norm, sizeof(norm));
  const char* segs[KU_HTTP_MAX_SEGS]; size_t seglens[KU_HTTP_MAX_SEGS]; size_t nseg = 0;
  { size_t nl = strlen(norm); size_t j = 0;
    while (j < nl) {
      while (j < nl && norm[j] == '/') j++;
      size_t st = j;
      while (j < nl && norm[j] != '/') j++;
      if (j > st) {
        if (nseg >= KU_HTTP_MAX_SEGS) { ku_http_connection_status(connection,server,414); return 1; }
        segs[nseg] = norm + st; seglens[nseg] = j - st; nseg++;
      }
    }
  }
  const char* param_values[KU_HTTP_MAX_SEGS];
  size_t param_value_lens[KU_HTTP_MAX_SEGS];
  size_t param_count = 0;
  KuHttpHandler* route = ku_http_trie_match(server->root, segs, seglens, nseg, 0, method, param_values, param_value_lens, &param_count);

  if (!route || route->returns_result != 2) return 0;
  if (param_count != route->nparams) { connection->failure.reason=KU_HTTP_ERROR_PARSER_INVARIANT; ku_http_connection_status(connection,server,500); return 1; }
  KuStruct___ku_http_request req = {0};
  size_t projection_charge=0;
  if (route->arity == 1) {
    size_t parameter_names=0;
    for (size_t i=0;i<route->nparams;i++) parameter_names+=strlen(route->param_names[i]);
    projection_charge=ku_http_projection_bytes(input,route->nparams,parameter_names);
    if (!ku_http_buffer_reserve(connection->budget,projection_charge)) { connection->failure.reason=KU_HTTP_ERROR_PROJECTION; ku_http_connection_status(connection,server,503); return 1; }
    req.params = ku_object_new(ku_http_map_capacity(route->nparams));
    for (size_t i=0; i<route->nparams; i++)
      ku_object_set(req.params,ku_http_string_copy(route->param_names[i],strlen(route->param_names[i])),
        ku_v_str(ku_http_string_copy(param_values[i],param_value_lens[i])));
    req.method = ku_http_string_copy(method,strlen(method));
    req.path = ku_http_string_copy(target,path_len);
    req.body = ku_http_string_copy(body_buf ? body_buf : "",body_len);
    req.query = ku_http_parse_query(target,path_len);
    req.headers = input->headers; input->headers = NULL;
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
/* Observe only a real readiness event while work owns the response. A FIN
 * is a legal request write-half-close and pipeline bytes belong to the next
 * request: neither means that this response is undeliverable. Pause that read
 * interest after observing either, avoiding a level-triggered busy loop. A
 * failed peek (for example ECONNRESET) is a concrete transport failure. The
 * ordinary close path requests cancellation but retains the physical Task/job
 * lease, buffers and active slot until actual retirement. */
static void ku_http_connection_observe_peer(KuHttpConnection* connection) {
  char byte;
  int received=recv(connection->socket_value,&byte,1,MSG_PEEK);
  if(received>=0) { connection->peer_read_paused=1; return; }
  int error=ku_http_socket_last_error();
  if(ku_http_socket_would_block(error) || ku_http_socket_error_interrupted(error)) return;
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
  KuValue* header = input->headers ? ku_object_get(input->headers,
    ku_string_static((const uint8_t*)"x-ku-control-token", 18)) : NULL;
  size_t token_length = strlen(group->control_token);
  int match = header && header->tag == KU_STR && header->as.s.len == token_length;
  unsigned char difference = 0;
  if (match) for (size_t i=0; i<token_length; i++) difference |= header->as.s.ptr[i] ^ (unsigned char)group->control_token[i];
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
    ku_http_mutex_lock(&group->lock);
    int n = snprintf(text,sizeof(text),"{\"limits\":{\"max_active_requests\":%lld,\"max_connections\":%lld,\"max_pending_requests\":%lld},\"loops\":[",
      (long long)server->max_active_requests,(long long)server->max_connections,(long long)server->max_pending_requests);
    if (n < 0) _Exit(79);
    used = (size_t)n;
    for (int i=0; i<group->created; i++) {
      KuHttpLoopOwner* owner = &group->loops[i];
      uint64_t sampled_now=ku_http_now_ms();
      uint64_t sample_age=sampled_now>=owner->cpu_sample_ms ? sampled_now-owner->cpu_sample_ms : 0;
      n = snprintf(text+used,sizeof(text)-used,"%s{\"id\":%d,\"accepted\":%llu,\"requests\":%llu,\"queued\":%llu,\"running\":%llu,\"resident\":%llu,\"connections\":%llu,\"pending\":%llu,\"cpu_ns\":%llu,\"cpu_sample_ms\":%llu,\"cpu_sample_age_ms\":%llu,\"cpu_clock_errors\":%llu}",
        i?",":"",i,owner->accepted,owner->requests,(unsigned long long)owner->queued,(unsigned long long)owner->running,(unsigned long long)owner->resident,(unsigned long long)owner->connections,(unsigned long long)owner->pending_count,
        (unsigned long long)owner->cpu_ns,(unsigned long long)owner->cpu_sample_ms,(unsigned long long)sample_age,(unsigned long long)owner->cpu_clock_errors);
      if (n<0 || (size_t)n>=sizeof(text)-used) _Exit(79);
      used+=(size_t)n;
    }
    n=snprintf(text+used,sizeof(text)-used,"],\"accepted\":%llu,\"draining\":%s",group->accepted,group->draining?"true":"false");
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    ku_http_mutex_unlock(&group->lock);
    n=snprintf(text+used,sizeof(text)-used,",\"outcomes\":[");
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    ku_http_mutex_lock(&group->lock);
    for (int i=0;i<group->created;i++) {
      KuHttpLoopOwner* owner=&group->loops[i];
      n=snprintf(text+used,sizeof(text)-used,"%s{\"id\":%d,\"admitted\":%llu,\"success_responses\":%llu,\"rejected_responses\":%llu,\"timeout_responses\":%llu,\"other_responses\":%llu,\"peak_loop_delay_ms\":%llu,\"queued_bytes\":%llu,\"peak_queued_bytes\":%llu}",i?",":"",i,
        owner->admitted,owner->success,owner->rejected,owner->timed_out,owner->other_responses,owner->peak_loop_delay_ms,(unsigned long long)owner->queued_bytes,(unsigned long long)owner->peak_queued_bytes);
      if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    }
    ku_http_mutex_unlock(&group->lock);
    text[used++]=']';
    n=snprintf(text+used,sizeof(text)-used,",\"transport_closes\":[");
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    ku_http_mutex_lock(&group->lock);
    for (int i=0;i<group->created;i++) {
      unsigned long long* counts=group->loops[i].transport_closes;
      n=snprintf(text+used,sizeof(text)-used,"%s{\"id\":%d,\"idle_pressure\":%llu,\"response_policy\":%llu,\"write_deadline\":%llu,\"peer_write_error\":%llu,\"queue_not_admitted\":%llu,\"drain_idle\":%llu}",
          i?",":"",i,counts[0],counts[1],counts[2],counts[3],counts[4],counts[5]);
      if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    }
    ku_http_mutex_unlock(&group->lock);
    text[used++]=']';
    {
      static const char* reasons[KU_HTTP_ERROR_COUNT]={"unknown","handler_result","handler_500","task_limit","task_closed","task_oom","driver_invariant","invalid_response","allocation","projection_budget","queue_admission","deadline","handler_exit","parser_invariant","peer_reset"};
      n=snprintf(text+used,sizeof(text)-used,",\"errors\":{\"sample_limit\":8,\"request_id_encoding\":\"hex\",\"counts\":{");
      if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
      ku_http_mutex_lock(&group->lock);
      for(size_t i=0;i<KU_HTTP_ERROR_COUNT;i++) {
        n=snprintf(text+used,sizeof(text)-used,"%s\"%s\":%llu",i?",":"",reasons[i],(unsigned long long)group->error_counts[i]);
        if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
      }
      n=snprintf(text+used,sizeof(text)-used,"},\"sample_policy\":\"500_and_peer_reset_500_priority\",\"dropped_samples\":%llu,\"http_500_total\":%llu,\"unknown_500\":%llu,\"samples\":[",(unsigned long long)group->error_samples_dropped,(unsigned long long)group->http_500_total,(unsigned long long)group->unknown_500);
      if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
      for(size_t i=0;i<group->error_sample_count;i++) {
        char rid[97]; static const char hex[]="0123456789abcdef";
        for(size_t j=0;j<group->error_samples[i].request_id_length;j++) {
          unsigned char c=group->error_samples[i].request_id[j]; rid[j*2]=hex[c>>4]; rid[j*2+1]=hex[c&15];
        }
        rid[group->error_samples[i].request_id_length*2]=0;
        KuHttpFailure* f=&group->error_samples[i].failure;
        n=snprintf(text+used,sizeof(text)-used,"%s{\"reason\":\"%s\",\"owner\":%llu,\"request_id\":\"%s\",\"request_id_truncated\":%s,\"request_sequence\":%llu,\"connection_slot\":%llu,\"connection_generation\":%llu,\"task_slot\":%llu,\"task_generation\":%llu,\"function_id\":%llu,\"phase\":%d,\"stage\":%u,\"driver_status\":%u,\"http_status\":%d,\"at_ms\":%llu}",i?",":"",reasons[f->reason],(unsigned long long)group->error_samples[i].owner,rid,group->error_samples[i].request_id_truncated?"true":"false",(unsigned long long)group->error_samples[i].request_sequence,(unsigned long long)group->error_samples[i].connection_slot,(unsigned long long)group->error_samples[i].connection_generation,(unsigned long long)f->task_slot,(unsigned long long)f->task_generation,(unsigned long long)f->function_id,group->error_samples[i].phase,f->stage,f->driver_status,group->error_samples[i].status,(unsigned long long)group->error_samples[i].at_ms);
        if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
      }
      ku_http_mutex_unlock(&group->lock);
      n=snprintf(text+used,sizeof(text)-used,"]}");
      if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    }
    ku_http_mutex_lock(&group->server->buffer_budget->lock);
    n=snprintf(text+used,sizeof(text)-used,",\"buffer_bytes\":%llu,\"peak_buffer_bytes\":%llu,\"buffer_rejections\":%llu",
      (unsigned long long)group->server->buffer_budget->current,(unsigned long long)group->server->buffer_budget->peak,(unsigned long long)group->server->buffer_budget->rejected);
    ku_http_mutex_unlock(&group->server->buffer_budget->lock);
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;

#if defined(KU_FEATURE_DATABASE_BUDGET)
    KuDatabaseBudgetSnapshot db = ku_database_budget_snapshot();
    n=snprintf(text+used,sizeof(text)-used,",\"database\":{\"reserved_connections\":%llu,\"reserved_waiters\":%llu,\"pending_operations\":%llu,\"pending_bytes\":%llu,\"peak_pending_bytes\":%llu,\"rejections\":%llu,\"unknown_results\":%llu,\"confirmed_cancellations\":%llu}",
      (unsigned long long)db.reserved_connections,(unsigned long long)db.reserved_waiters,
      (unsigned long long)db.pending_operations,(unsigned long long)db.pending_bytes,(unsigned long long)db.peak_pending_bytes,
      (unsigned long long)db.rejections,(unsigned long long)db.unknown_results,(unsigned long long)db.confirmed_cancellations);
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
#endif
#if defined(KU_FEATURE_SERVICE_OUTPUT)
    KuServiceOutputSnapshot output=ku_service_output_snapshot();
    n=snprintf(text+used,sizeof(text)-used,",\"output\":{\"queued_records\":%llu,\"queued_bytes\":%llu,\"peak_bytes\":%llu,\"dropped_records\":%llu,\"dropped_bytes\":%llu,\"failures\":%llu,\"written_bytes\":%llu}",
      (unsigned long long)output.queued_records,(unsigned long long)output.queued_bytes,(unsigned long long)output.peak_bytes,
      (unsigned long long)output.dropped_records,(unsigned long long)output.dropped_bytes,(unsigned long long)output.failures,(unsigned long long)output.written_bytes);
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
#endif
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    n=snprintf(text+used,sizeof(text)-used,",\"tasks\":[");
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    for (int i=0; i<group->count; i++) {
      uint64_t states[8]={0};
      /* Never hold group->lock across a driver API: driver publications call
         event_wake while holding their own mutex. */
      ku_http_task_metrics(group->tasks,(size_t)i,states);
      n=snprintf(text+used,sizeof(text)-used,"%s{\"id\":%d,\"ready\":%llu,\"waiting\":%llu,\"running\":%llu,\"resident\":%llu,\"timers\":%llu,\"polls\":%llu,\"peak_ready_delay_ms\":%llu,\"exited\":%llu}",i?",":"",i,
        (unsigned long long)states[0],(unsigned long long)states[1],(unsigned long long)states[2],(unsigned long long)states[3],
        (unsigned long long)states[4],(unsigned long long)states[5],(unsigned long long)states[6],(unsigned long long)states[7]);
      if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    }
    text[used++]=']';
    uint64_t driver_budget[9]={0};
    ku_http_task_driver_budget(group->tasks,driver_budget);
    n=snprintf(text+used,sizeof(text)-used,",\"task_driver\":{\"capacity\":%llu,\"resident\":%llu,\"peak_resident\":%llu,\"fixed_bytes\":%llu,\"reserved_bytes\":%llu,\"peak_reserved_bytes\":%llu,\"byte_limit\":%llu,\"admission_slot_rejections\":%llu,\"admission_byte_rejections\":%llu}",
      (unsigned long long)driver_budget[0],(unsigned long long)driver_budget[1],(unsigned long long)driver_budget[2],
      (unsigned long long)driver_budget[3],(unsigned long long)driver_budget[4],(unsigned long long)driver_budget[5],
      (unsigned long long)driver_budget[6],(unsigned long long)driver_budget[7],(unsigned long long)driver_budget[8]);
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    uint64_t driver_timing[10]={0};
    ku_http_task_driver_timing(group->tasks,driver_timing);
    n=snprintf(text+used,sizeof(text)-used,",\"task_driver_timing\":{\"enabled\":%s,\"lock_count\":%llu,\"lock_wait_ns\":%llu,\"max_lock_wait_ns\":%llu,\"lock_hold_ns\":%llu,\"max_lock_hold_ns\":%llu,\"condition_count\":%llu,\"condition_sleep_reacquire_ns\":%llu,\"max_condition_sleep_reacquire_ns\":%llu,\"clock_errors\":%llu}",
      driver_timing[0]?"true":"false",(unsigned long long)driver_timing[1],
      (unsigned long long)driver_timing[2],(unsigned long long)driver_timing[3],
      (unsigned long long)driver_timing[4],(unsigned long long)driver_timing[5],
      (unsigned long long)driver_timing[6],(unsigned long long)driver_timing[7],
      (unsigned long long)driver_timing[8],(unsigned long long)driver_timing[9]);
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
    uint64_t blocking_states[16]={0};
    ku_http_task_blocking_metrics(group->tasks,blocking_states);
    n=snprintf(text+used,sizeof(text)-used,",\"task_blocking\":{\"resident\":%llu,\"queued\":%llu,\"running\":%llu,\"peak_resident\":%llu,\"peak_queued\":%llu,\"peak_running\":%llu,\"accepted\":%llu,\"rejected\":%llu,\"finished\":%llu,\"output_charge_bytes\":%llu,\"workers_created\":%llu,\"workers_exited\":%llu,\"cancelled_still_running\":%llu,\"net_wait\":%llu,\"net_step\":%llu}",
      (unsigned long long)blocking_states[0],(unsigned long long)blocking_states[1],(unsigned long long)blocking_states[2],
      (unsigned long long)blocking_states[3],(unsigned long long)blocking_states[4],(unsigned long long)blocking_states[5],
      (unsigned long long)blocking_states[6],(unsigned long long)blocking_states[7],(unsigned long long)blocking_states[8],
      (unsigned long long)blocking_states[9],(unsigned long long)blocking_states[10],(unsigned long long)blocking_states[11],
      (unsigned long long)blocking_states[12],(unsigned long long)blocking_states[13],(unsigned long long)blocking_states[14]);
    if(n<0 || (size_t)n>=sizeof(text)-used) _Exit(79); used+=(size_t)n;
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
static const char* ku_http_loop_run(KuHttpLoopOwner* owner) {
  KuHttpLoopGroup* group = owner->group;
  KuHttpServer* server = group->server;
  KuHttpSocket srv = group->listener;
  long max_conn = (long)server->max_connections;
  int max_pending = owner->pending;
  int slot_count = owner->slots;
  int nworkers = owner->workers;
  size_t poll_capacity=(size_t)slot_count+3u,net_capacity=0,net_bytes=0;
#if defined(KU_HTTP_HAS_TASK_NET)
  net_capacity=ku_http_task_net_capacity(group->tasks);
  if(!net_capacity || net_capacity>KU_TASK_NET_POLL_CAPACITY)return "Task network capacity invalid";
  poll_capacity+=net_capacity;net_bytes=net_capacity*sizeof(KuTaskNetPollV1);
#endif
  /* Configuration limits bound all products below on every supported target.
     One receipt includes every owner array; no Task/connection consumes an
     independent copy of the service's configured byte budget. */
  size_t fixed_bytes=(size_t)slot_count*(sizeof(KuHttpConnection)+sizeof(int)+sizeof(KuHttpPollConnection)+sizeof(KuHttpJob*))
      +poll_capacity*sizeof(KuHttpPollFd)+(size_t)max_pending*sizeof(KuHttpPendingItem)+net_bytes;
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
  KuHttpActiveSet active={connections,active_indices,0,slot_count,-1,0,0};
  KuHttpPending pending={0};pending.cap=max_pending;
  pending.items=(KuHttpPendingItem*)calloc((size_t)max_pending,sizeof(KuHttpPendingItem));
  if(!connections || !poll_items || !pending.items || !active_indices || !poll_connections || (net_bytes && !net_storage)) {
    free(connections);free(poll_items);free(pending.items);free(active_indices);free(poll_connections);free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    (void)ku_http_task_fixed_refund(group->tasks,fixed_bytes);
#endif
    return "reactor allocation failed";
  }
  KuHttpConnection rejection = {0};
  rejection.socket_value = KU_HTTP_INVALID_SOCKET;
  KuHttpWake wake;
  if (!ku_http_wake_init(&wake)) {
    free(connections); free(poll_items); free(pending.items); free(active_indices); free(poll_connections); free(net_storage);
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
    (void)ku_http_task_fixed_refund(group->tasks,fixed_bytes);
#endif
    return "reactor wake initialization failed";
  }
  KuHttpQueue queue;
  /* At most one job per active slot; socket pending is a separate FIFO whose
     original max_pending bound is not duplicated by another pending job pool. */
  if (!ku_http_queue_init(&queue, slot_count, &wake)) {
    ku_http_wake_close(&wake);
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
  const char* listen_failure = spawned == nworkers ? NULL : "handler thread startup failed";
  ku_http_mutex_lock(&group->lock);
  owner->wake = &wake; owner->ready = !listen_failure;
  if (listen_failure) group->failure = listen_failure;
  ku_http_mutex_unlock(&group->lock);
  ku_http_wake_notify(&group->wake);
  for (;;) {
    ku_http_mutex_lock(&queue.lock);
    size_t queued = (size_t)queue.count, running = queue.running, resident = queue.resident;
    size_t queued_bytes=queue.queued_bytes, peak_queued_bytes=queue.peak_queued_bytes;
    ku_http_mutex_unlock(&queue.lock);
    size_t occupied_connections=active.sockets;
    uint64_t sampled_cpu_ns=0, sampled_cpu_ms=ku_http_now_ms();
    int cpu_sample_due=sampled_cpu_ms>=next_cpu_sample_ms;
    int cpu_sample_valid=cpu_sample_due && ku_http_thread_cpu_ns(&sampled_cpu_ns);
    if (cpu_sample_due) next_cpu_sample_ms=sampled_cpu_ms>UINT64_MAX-10u ? UINT64_MAX : sampled_cpu_ms+10u;
    ku_http_mutex_lock(&group->lock);
    if (cpu_sample_due) ku_http_owner_cpu_sample(owner,sampled_cpu_ns,sampled_cpu_ms,cpu_sample_valid);
    owner->queued = queued; owner->running = running; owner->resident = resident;
    owner->queued_bytes=queued_bytes; owner->peak_queued_bytes=peak_queued_bytes;
    owner->connections = occupied_connections; owner->pending_count = (size_t)pending.count;
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
    uint32_t pumped = ku_http_task_pump(group->tasks,(size_t)owner->index,32u,&task_deadline);
    if (pumped > 1u && !listen_failure) listen_failure = "HTTP Task event loop failed";
#endif
    unsigned long long now = ku_http_now_ms();
    /* Reverse traversal is intentional: swap-remove moves only a tail that
       this pass already observed. Admission runs strictly between passes. */
    for (int position=active.count;position-->0;) {
      int index=active.indices[position];
      KuHttpConnection* connection = &connections[index];
      /* This is reuse admission at the response boundary. A keep-alive header
         already sent is a snapshot, not a promise to retain a pressured idle
         connection. Preserve pending FIFO priority, including pipeline bytes. */
      if (!listening || listen_failure || pending.count) connection->keep_alive = 0;
      if (!listening && connection->phase == KU_HTTP_IO_HEADER && connection->reused && !connection->buffered && !connection->carry_length) {
        ku_http_count_transport_close(connection,KU_HTTP_CLOSE_DRAIN_IDLE);
        ku_http_connection_close(connection); continue;
      }
      ku_http_connection_observe_job(connection, server);
      if (connection->request_deadline && (connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY || connection->phase == KU_HTTP_IO_WRITE)
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
      /* Under pressure a genuinely idle persistent socket yields its slot.
         Partial requests keep their original byte ownership and deadline. */
      if (pending.count && connection->phase == KU_HTTP_IO_HEADER &&
          connection->reused && !connection->buffered && !connection->carry_length) {
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
      if ((connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY) && now >= connection->deadline)
        ku_http_connection_status(connection, server, 408);
      else if (connection->phase == KU_HTTP_IO_WRITE && now >= connection->deadline) {
        ku_http_count_transport_close(connection,KU_HTTP_CLOSE_WRITE_DEADLINE);
        ku_http_connection_close(connection);
      }
    }
    if (rejection.phase == KU_HTTP_IO_REJECT && now >= rejection.deadline)
      ku_http_connection_status(&rejection, server, 503);
    else if (rejection.phase == KU_HTTP_IO_WRITE && now >= rejection.deadline) {
      ku_http_count_transport_close(&rejection,KU_HTTP_CLOSE_WRITE_DEADLINE);
      ku_http_connection_close(&rejection);
    }
    if (!listen_failure) ku_http_pending_activate(&pending, &active, server);
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
    if (task_deadline < nearest) nearest = task_deadline; /* real in-loop health timer */
    now = ku_http_now_ms();
    if (started && accept_turn && listening && rejection.phase == KU_HTTP_IO_FREE) {
      if (accept_retry_deadline > now) { if (accept_retry_deadline < nearest) nearest = accept_retry_deadline; }
      else {
        listener_index = poll_count;
        ku_http_poll_add(poll_items, &poll_count, srv, KU_HTTP_POLL_READ);
      }
    }
    int connection_poll_base=poll_count,connection_poll_count=0;
    for (int position=0;position<active.count;position++) {
      int index=active.indices[position];
      KuHttpConnection* connection = &connections[index];
      connection->poll_index = -1;
      short events = 0;
      if (connection->phase == KU_HTTP_IO_HEADER || connection->phase == KU_HTTP_IO_BODY) events = KU_HTTP_POLL_READ;
      else if (connection->phase == KU_HTTP_IO_WRITE) events = KU_HTTP_POLL_WRITE;
      else if ((connection->phase == KU_HTTP_IO_ASYNC || connection->phase == KU_HTTP_IO_JOB)
          && !connection->peer_read_paused) events=KU_HTTP_POLL_READ;
      if (connection->phase == KU_HTTP_IO_JOB && connection->job && connection->job->request_deadline < nearest)
        nearest = connection->job->request_deadline;
      if (connection->async_task && !connection->async_cancelled && connection->async_deadline < nearest)
        nearest = connection->async_deadline;
      if (!events) continue;
      connection->poll_index = poll_count;
      poll_connections[connection_poll_count++]=(KuHttpPollConnection){index,connection->connection_generation};
      ku_http_poll_add(poll_items, &poll_count, connection->socket_value, events);
      if ((connection->phase==KU_HTTP_IO_HEADER || connection->phase==KU_HTTP_IO_BODY || connection->phase==KU_HTTP_IO_WRITE)
          && connection->deadline < nearest) nearest = connection->deadline;
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
    if (net_count>net_capacity) { listen_failure="Task network interest bound exceeded"; continue; }
    if (net_deadline<nearest) nearest=net_deadline;
    int net_base=poll_count;
    for (size_t i=0;i<net_count;i++) {
      short events=0;
      if (net_interests[i].events&KU_TASK_NET_READ) events|=KU_HTTP_POLL_READ;
      if (net_interests[i].events&KU_TASK_NET_WRITE) events|=KU_HTTP_POLL_WRITE;
      ku_http_poll_add(poll_items,&poll_count,(KuHttpSocket)net_interests[i].socket_value,events);
    }
#endif
    int wait_ms = -1;
    if (nearest != ~0ULL) {
      now = ku_http_now_ms();
      unsigned long long remaining = nearest > now ? nearest - now : 0;
      wait_ms = remaining > (unsigned long long)INT_MAX ? INT_MAX : (int)remaining;
    }
    /* No periodic wake tick: idle/read/write/job progress blocks in one OS
       poll until actual readiness, worker publication, or the nearest old D. */
    int selected = ku_http_poll(poll_items, (size_t)poll_count, wait_ms);
    if (selected < 0) {
      if (ku_http_socket_error_interrupted(ku_http_socket_last_error())) continue;
      if (!listen_failure) listen_failure = "reactor wait failed";
      else { fputs("http reactor wait repeatedly failed\n", stderr); exit(1); }
      continue;
    }
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
    short wake_events = poll_items[wake_index].revents;
    if (wake_events & POLLNVAL) { fputs("http reactor wake socket invalid\n", stderr); exit(1); }
    if (wake_events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)) ku_http_wake_drain(&wake);
    for (int selected_connection=0;selected_connection<connection_poll_count;selected_connection++) {
      int poll_index=connection_poll_base+selected_connection;
      /* Poll entries borrow this exact owner/slot/socket generation. Close
         may swap the dense active tail, but never changes this snapshot map. */
      KuHttpConnection* connection=ku_http_polled_connection(&active,
          &poll_connections[selected_connection],poll_index,poll_items[poll_index].fd);
      if(!connection) continue;
      short events=poll_items[poll_index].revents;
      if (!events) continue;
      if (events & POLLNVAL) { ku_http_connection_close(connection); continue; }
      if (connection->phase == KU_HTTP_IO_HEADER && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)))
        ku_http_connection_read_header(connection, server, &queue, spawned);
      else if (connection->phase == KU_HTTP_IO_BODY && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP)))
        ku_http_connection_read_body(connection, server, &queue, spawned);
      else if (connection->phase == KU_HTTP_IO_WRITE && (events & (KU_HTTP_POLL_WRITE | POLLERR | POLLHUP)))
        ku_http_connection_write(connection, server);
      else if ((connection->phase==KU_HTTP_IO_ASYNC || connection->phase==KU_HTTP_IO_JOB)
          && (events & (KU_HTTP_POLL_READ | POLLERR | POLLHUP))) ku_http_connection_observe_peer(connection);
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
      KuHttpSocket cli = accept(srv, NULL, NULL);
      int accept_error = cli == KU_HTTP_INVALID_SOCKET ? ku_http_socket_last_error() : 0;
      if (cli != KU_HTTP_INVALID_SOCKET) {
        group->accepted++; owner->accepted++;
        group->accept_turn = (owner->index+1) % group->count;
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
      if (live > max_conn) ku_http_reject_start(&rejection, cli);
      else if (slot >= 0 && !pending.count) ku_http_connection_start(&connections[slot], &active, cli, server);
      else if (pending.count < pending.cap) {
        pending.items[pending.tail] = (KuHttpPendingItem){cli,ku_http_deadline_ms(server->request_timeout_ms)};
        pending.tail = (pending.tail + 1) % pending.cap;
        pending.count++;
      } else ku_http_reject_start(&rejection, cli);

    }
  }
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
  ku_http_wake_close(&wake);
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
  int blocking = (int)ku_runtime_blocking_count();
#if defined(KU_HTTP_HAS_TASK_RUNTIME)
  blocking -= (int)ku_http_task_blocking_threads((size_t)blocking);
#endif
  if (!getenv("KU_EVENT_LOOPS")) {
    if (group.count > server->max_active_requests) group.count = (int)server->max_active_requests;
    if (group.count > server->max_pending_requests) group.count = (int)server->max_pending_requests;
    if (group.count > blocking) group.count = blocking;
  }
  if (group.count<1 || blocking<1 || group.count > server->max_active_requests || group.count > server->max_pending_requests || blocking < group.count) {
    ku_http_socket_close(srv); ku_http_net_cleanup();
    return ku_http_listen_err(server, "event loops must fit active/pending limits and blocking_threads must cover every loop");
  }
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
  for (int i=0; i<group.count; i++) {
    KuHttpLoopOwner* owner = &group.loops[i]; owner->group = &group; owner->index = i;
    owner->slots = (int)(server->max_active_requests/group.count) + (i < server->max_active_requests%group.count);
    owner->pending = (int)(server->max_pending_requests/group.count) + (i < server->max_pending_requests%group.count);
    owner->workers = blocking/group.count + (i < blocking%group.count);
    if (owner->workers > owner->slots) owner->workers = owner->slots;
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
