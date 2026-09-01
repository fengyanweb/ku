//! Deterministic native Redis allocation/deadline failure tests. The generated
//! parser and ownership code run unchanged; only allocation, clock and OS socket
//! calls are scripted. Real Winsock/POSIX framing is covered by native_redis_test.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::fs;
use std::process::Command;

use native_pg_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

#[test]
fn native_redis_failures_are_catchable_bounded_and_release_resources() {
    let directory = TempDir::new("redis-failures");
    let generated = emit_c(
        directory.path(),
        r#"import redis from "std.redis"
fn main(): null! {
    client = redis.client({ host: "127.0.0.1", port: 6379, max_connections: 1 })?
    client.ping()?
    println(client.get("key")?)
    client.close()
    return ok(null)
}
"#,
    );
    assert!(generated.contains("#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1"));
    assert!(generated.contains("typedef struct KuString {"));
    assert!(generated.contains("#define KU_REDIS_MAX_CONFIG_BYTES 65536ULL"));
    assert!(generated.contains("redis server returned an error"));
    assert!(generated.contains("else ku_redis_client_handoff_available_locked(client)"));
    let mut harness = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOKS}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1",
            &format!("{SOCKET_HOOKS}\n#define KU_NATIVE_RUNTIME_REDIS_SOCKET 1"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        )
        .replacen(
            "static void ku_redis_pool_wake_one(KuRedisPoolSync* sync) {",
            "static int ku_test_redis_wake_one_calls = 0;\nstatic void ku_redis_pool_wake_one(KuRedisPoolSync* sync) { ku_test_redis_wake_one_calls++;",
            1,
        );
    harness.push_str(REDIS_FAILURE_STUB);
    let source = directory.path().join("redis-failure-harness.c");
    fs::write(&source, harness).expect("write Redis failure harness");
    let Some(executable) = compile_harness(directory.path(), &source, "redis-failure-harness")
    else {
        return;
    };
    // Each case has a fresh process, so the pre-fix exit(1) cannot hide later
    // scenarios, and allocation failure never puts the test host under pressure.
    for case in [
        "normal",
        "get-body-oom",
        "connect-host-oom",
        "connect-owner-oom",
        "server-error-static",
        "auth-rejection-static",
        "auth-oom-classification",
        "auth-expired-classification",
        "connect-os-timeout",
        "connect-pending-os-timeout",
        "send-os-timeout",
        "recv-os-timeout",
        "recv-transport-error",
        "send-deadline-overrun",
        "recv-deadline-overrun",
        "pool-expired-classification",
        "waiter-handoff-helper",
        "max-waiters-zero-deferred-close",
        "credential-wipe-close",
        "dynamic-config-validation",
        "static-error-no-allocation",
        "expired-handler",
        "resolver-overrun",
        "connect-wait-budget",
        "connect-late-success",
        "connect-setup-overrun",
        "connect-owner-overrun",
    ] {
        let mut command = Command::new(&executable);
        command.current_dir(directory.path()).arg(case);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .unwrap_or_else(|error| panic!("Redis failure case {case} exceeded bounds: {error}"));
        assert!(
            output.status.success(),
            "Redis failure case {case} failed ({:?}):\n{}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace('\r', ""),
            format!("redis failure closed loop: {case}\n")
        );
        assert!(output.stderr.is_empty(), "unexpected Redis failure output");
    }
}

const ALLOCATION_HOOKS: &str = r#"
static void* ku_test_malloc(size_t);
static void* ku_test_calloc(size_t, size_t);
static void* ku_test_realloc(void*, size_t);
static void ku_test_free(void*);
#define malloc ku_test_malloc
#define calloc ku_test_calloc
#define realloc ku_test_realloc
#define free ku_test_free
"#;

const SOCKET_HOOKS: &str = r#"
static unsigned long long ku_test_now(void);
static int ku_test_getaddrinfo(const char*, const char*, const struct addrinfo*, struct addrinfo**);
static void ku_test_freeaddrinfo(struct addrinfo*);
#if defined(_WIN32)
static SOCKET ku_test_socket(int, int, int);
static int ku_test_connect(SOCKET, const struct sockaddr*, int);
static int ku_test_select(int, fd_set*, fd_set*, fd_set*, const struct timeval*);
static int ku_test_getsockopt(SOCKET, int, int, char*, int*);
static int ku_test_setsockopt(SOCKET, int, int, const char*, int);
static int ku_test_ioctlsocket(SOCKET, long, u_long*);
static int ku_test_send(SOCKET, const char*, int, int);
static int ku_test_recv(SOCKET, char*, int, int);
static int ku_test_closesocket(SOCKET);
#define select ku_test_select
#define ioctlsocket ku_test_ioctlsocket
#define closesocket ku_test_closesocket
#else
static int ku_test_socket(int, int, int);
static int ku_test_connect(int, const struct sockaddr*, socklen_t);
static int ku_test_poll(struct pollfd*, nfds_t, int);
static int ku_test_getsockopt(int, int, int, void*, socklen_t*);
static int ku_test_setsockopt(int, int, int, const void*, socklen_t);
static int ku_test_fcntl(int, int, ...);
static ssize_t ku_test_send(int, const void*, size_t, int);
static ssize_t ku_test_recv(int, void*, size_t, int);
static int ku_test_close(int);
#define poll ku_test_poll
#define fcntl ku_test_fcntl
#define close ku_test_close
#endif
#define __ku_handler_now_ms ku_test_now
#define getaddrinfo ku_test_getaddrinfo
#define freeaddrinfo ku_test_freeaddrinfo
#define socket ku_test_socket
#define connect ku_test_connect
#define getsockopt ku_test_getsockopt
#define setsockopt ku_test_setsockopt
#define send ku_test_send
#define recv ku_test_recv
"#;

const REDIS_FAILURE_STUB: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
#undef __ku_handler_now_ms
#undef getaddrinfo
#undef freeaddrinfo
#undef socket
#undef connect
#undef select
#undef poll
#undef getsockopt
#undef setsockopt
#undef ioctlsocket
#undef fcntl
#undef send
#undef recv
#undef closesocket
#undef close

#define CHECK(value) do { if (!(value)) { fprintf(stderr, "check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
static unsigned long long clock_ms = 1000, expire_at = 0;
static unsigned long long resolve_elapsed = 0, connect_elapsed = 0, wait_budget = 0;
static int bad_calls = 0, resolve_calls = 0, free_address_calls = 0;
static int socket_calls = 0, close_calls = 0, socket_live = 0, connect_calls = 0;
static int send_calls = 0, recv_calls = 0, wait_calls = 0, connect_pending = 0;
static int fail_connect_timeout = 0;
static int connect_wait_ready = 0, pending_connect_timeout = 0;
static int expire_setup = 0, expire_owner = 0;
static int fail_send_timeout = 0, fail_recv_timeout = 0, fail_recv_transport = 0;
static unsigned long long send_elapsed = 0, recv_elapsed = 0;
static int fail_all_allocations = 0, injected_failures = 0, allocation_calls = 0;
static size_t fail_size = 0, live_allocations = 0, live_bytes = 0;
static void* wipe_username = 0;
static void* wipe_password = 0;
static int credential_wipes = 0;
static struct { void* pointer; size_t size; } allocations[64];
static struct addrinfo address;
static struct sockaddr_in socket_address;
static const char* reply = "";
static size_t reply_length = 0, reply_position = 0;

static int allocation_slot(void* pointer) {
  for (int i = 0; i < 64; i++) if (allocations[i].pointer == pointer) return i;
  return -1;
}
static void* ku_test_malloc(size_t size) {
  allocation_calls++;
  if (fail_all_allocations || (fail_size && size == fail_size)) {
    fail_size = 0; injected_failures++; return 0;
  }
  void* pointer = malloc(size);
  if (!pointer) { fputs("test host allocation failed\n", stderr); exit(2); }
  int slot = allocation_slot(0);
  if (slot < 0) { free(pointer); fputs("allocation ledger exhausted\n", stderr); exit(2); }
  allocations[slot].pointer = pointer; allocations[slot].size = size;
  live_allocations++; live_bytes += size;
  if (expire_owner && size == sizeof(KuRedis)) { expire_owner = 0; clock_ms = expire_at; }
  return pointer;
}
static void* ku_test_calloc(size_t count, size_t size) {
  if (count && size > SIZE_MAX / count) return 0;
  size_t total = count * size;
  void* pointer = ku_test_malloc(total);
  if (pointer) memset(pointer, 0, total);
  return pointer;
}
static void* ku_test_realloc(void* pointer, size_t size) {
  if (!pointer) return ku_test_malloc(size);
  allocation_calls++;
  if (fail_all_allocations || (fail_size && size == fail_size)) {
    fail_size = 0; injected_failures++; return 0;
  }
  int slot = allocation_slot(pointer);
  if (slot < 0 || size == 0) { bad_calls++; return 0; }
  void* replacement = realloc(pointer, size);
  if (!replacement) { fputs("test host reallocation failed\n", stderr); exit(2); }
  live_bytes -= allocations[slot].size;
  allocations[slot].pointer = replacement; allocations[slot].size = size; live_bytes += size;
  return replacement;
}
static void ku_test_free(void* pointer) {
  if (!pointer) return;
  int slot = allocation_slot(pointer);
  if (slot < 0) { bad_calls++; return; }
  if (pointer == wipe_username || pointer == wipe_password) {
    const unsigned char* bytes = (const unsigned char*)pointer;
    for (size_t index = 0; index < allocations[slot].size; index++) {
      if (bytes[index] != 0) bad_calls++;
    }
    if (pointer == wipe_username) wipe_username = 0;
    if (pointer == wipe_password) wipe_password = 0;
    credential_wipes++;
  }
  live_bytes -= allocations[slot].size; live_allocations--;
  allocations[slot].pointer = 0; allocations[slot].size = 0;
  free(pointer);
}
static unsigned long long ku_test_now(void) { return clock_ms; }
static int ku_test_getaddrinfo(const char* host, const char* service, const struct addrinfo* hints, struct addrinfo** out) {
  resolve_calls++;
  if (!host || strcmp(host, "127.0.0.1") || !service || strcmp(service, "6379") || !hints || !out) {
    bad_calls++; return -1;
  }
  memset(&address, 0, sizeof(address)); memset(&socket_address, 0, sizeof(socket_address));
  address.ai_family = AF_INET; address.ai_socktype = SOCK_STREAM; address.ai_protocol = IPPROTO_TCP;
  address.ai_addr = (struct sockaddr*)&socket_address; address.ai_addrlen = sizeof(socket_address);
  *out = &address; clock_ms += resolve_elapsed;
  return 0;
}
static void ku_test_freeaddrinfo(struct addrinfo* value) {
  if (value != &address || ++free_address_calls > resolve_calls) bad_calls++;
}
static int check_socket(uintptr_t value) {
  if (value != 41 || !socket_live) { bad_calls++; return 0; }
  return 1;
}
static int socket_created(int family, int type, int protocol) {
  if (family != AF_INET || type != SOCK_STREAM || protocol != IPPROTO_TCP || socket_live) bad_calls++;
  socket_calls++; socket_live = 1; return 41;
}
static int socket_closed(uintptr_t value) {
  if (!check_socket(value)) return -1;
  socket_live = 0; close_calls++; return 0;
}
static void set_socket_failure(int timed_out);
static int socket_connecting(uintptr_t value, const struct sockaddr* target, size_t len) {
  if (!check_socket(value) || target != address.ai_addr || len != address.ai_addrlen) return -1;
  connect_calls++; clock_ms += connect_elapsed;
  if (fail_connect_timeout) {
    fail_connect_timeout = 0; set_socket_failure(1); return -1;
  }
  if (connect_pending) {
#if defined(_WIN32)
    WSASetLastError(WSAEWOULDBLOCK);
#else
    errno = EINPROGRESS;
#endif
    return -1;
  }
  return 0;
}
static int socket_option(uintptr_t value, int level, int option) {
  if (!check_socket(value) || level != SOL_SOCKET) return -1;
  if (expire_setup && option == SO_SNDTIMEO) { expire_setup = 0; clock_ms = expire_at; }
  return 0;
}
static void set_socket_failure(int timed_out) {
#if defined(_WIN32)
  WSASetLastError(timed_out ? WSAETIMEDOUT : WSAECONNRESET);
#else
  errno = timed_out ? ETIMEDOUT : ECONNRESET;
#endif
}
static int socket_send(uintptr_t value, const char* data, size_t len) {
  if (!check_socket(value) || (len && !data) || len > 8192) return -1;
  send_calls++;
  if (fail_send_timeout) { fail_send_timeout = 0; set_socket_failure(1); return -1; }
  clock_ms += send_elapsed; send_elapsed = 0;
  return (int)len;
}
static int socket_recv(uintptr_t value, char* data, size_t capacity) {
  if (!check_socket(value) || !data || !capacity) return -1;
  recv_calls++;
  if (fail_recv_timeout) { fail_recv_timeout = 0; set_socket_failure(1); return -1; }
  if (fail_recv_transport) { fail_recv_transport = 0; set_socket_failure(0); return -1; }
  clock_ms += recv_elapsed; recv_elapsed = 0;
  size_t available = reply_length - reply_position;
  size_t take = available < capacity ? available : capacity;
  if (take) memcpy(data, reply + reply_position, take);
  reply_position += take; return (int)take;
}
#if defined(_WIN32)
static SOCKET ku_test_socket(int family, int type, int protocol) { return (SOCKET)socket_created(family, type, protocol); }
static int ku_test_connect(SOCKET value, const struct sockaddr* target, int len) { return socket_connecting((uintptr_t)value, target, (size_t)len); }
static int ku_test_closesocket(SOCKET value) { return socket_closed((uintptr_t)value); }
static int ku_test_ioctlsocket(SOCKET value, long command, u_long* mode) {
  if (!check_socket((uintptr_t)value) || command != FIONBIO || !mode) return -1;
  return 0;
}
static int ku_test_select(int ignored, fd_set* read_set, fd_set* write_set, fd_set* error_set, const struct timeval* timeout) {
  (void)ignored; (void)read_set;
  if (!write_set || !error_set || !timeout || !FD_ISSET((SOCKET)41, write_set)) { bad_calls++; return -1; }
  wait_calls++; wait_budget = (unsigned long long)timeout->tv_sec * 1000ULL + (unsigned long long)timeout->tv_usec / 1000ULL;
  if (connect_wait_ready) {
    connect_wait_ready = 0; FD_ZERO(write_set); FD_SET((SOCKET)41, error_set); return 1;
  }
  clock_ms += wait_budget; FD_ZERO(error_set); return 0;
}
static int ku_test_getsockopt(SOCKET value, int level, int option, char* out, int* len) {
  if (!check_socket((uintptr_t)value) || level != SOL_SOCKET || option != SO_ERROR || !out || !len || *len < (int)sizeof(int)) return -1;
  *(int*)out = pending_connect_timeout ? WSAETIMEDOUT : 0;
  pending_connect_timeout = 0; *len = sizeof(int); return 0;
}
static int ku_test_setsockopt(SOCKET value, int level, int option, const char* data, int len) {
  if (!data || len <= 0) { bad_calls++; return -1; }
  return socket_option((uintptr_t)value, level, option);
}
static int ku_test_send(SOCKET value, const char* data, int len, int flags) { (void)flags; return socket_send((uintptr_t)value, data, (size_t)len); }
static int ku_test_recv(SOCKET value, char* data, int len, int flags) { (void)flags; return socket_recv((uintptr_t)value, data, (size_t)len); }
#else
static int ku_test_socket(int family, int type, int protocol) { return socket_created(family, type, protocol); }
static int ku_test_connect(int value, const struct sockaddr* target, socklen_t len) { return socket_connecting((uintptr_t)value, target, (size_t)len); }
static int ku_test_close(int value) { return socket_closed((uintptr_t)value); }
static int ku_test_fcntl(int value, int command, ...) {
  if (!check_socket((uintptr_t)value) || (command != F_GETFL && command != F_SETFL)) return -1;
  return 0;
}
static int ku_test_poll(struct pollfd* items, nfds_t count, int timeout) {
  if (!items || count != 1 || !check_socket((uintptr_t)items[0].fd) || timeout < 0) { bad_calls++; return -1; }
  wait_calls++; wait_budget = (unsigned long long)timeout;
  if (connect_wait_ready) {
    connect_wait_ready = 0; items[0].revents = POLLERR; return 1;
  }
  clock_ms += wait_budget; items[0].revents = 0; return 0;
}
static int ku_test_getsockopt(int value, int level, int option, void* out, socklen_t* len) {
  if (!check_socket((uintptr_t)value) || level != SOL_SOCKET || option != SO_ERROR || !out || !len || *len < sizeof(int)) return -1;
  *(int*)out = pending_connect_timeout ? ETIMEDOUT : 0;
  pending_connect_timeout = 0; *len = sizeof(int); return 0;
}
static int ku_test_setsockopt(int value, int level, int option, const void* data, socklen_t len) {
  if (!data || !len) { bad_calls++; return -1; }
  return socket_option((uintptr_t)value, level, option);
}
static ssize_t ku_test_send(int value, const void* data, size_t len, int flags) { (void)flags; return socket_send((uintptr_t)value, (const char*)data, len); }
static ssize_t ku_test_recv(int value, void* data, size_t len, int flags) { (void)flags; return socket_recv((uintptr_t)value, (char*)data, len); }
#endif

static KuString text(const char* value) { return ku_string_static((const uint8_t*)value, strlen(value)); }
static int equals(KuString value, const char* expected) { size_t len = strlen(expected); return value.len == len && (!len || memcmp(value.ptr, expected, len) == 0); }
static int static_error(KuError error, const char* code) {
  return equals(error.domain, "redis") && equals(error.code, code) && error.message.len > 0
      && error.domain.storage == KU_STRING_STATIC && error.code.storage == KU_STRING_STATIC
      && error.message.storage == KU_STRING_STATIC;
}
static void set_reply(KuRedis* connection, const char* value) {
  if (connection->read_position != connection->read_length) { fputs("test reply replaced before consumption\n", stderr); exit(2); }
  reply = value; reply_length = strlen(value); reply_position = 0;
}
static KuRedis* open_connection(void) {
  KuRedisOpenResult opened = ku_redis_open_connection(
      text("127.0.0.1"), 6379, 5000, ku_redis_deadline_after_ms(5000));
  if (!opened.ok || !opened.value) { fputs("test baseline connection failed\n", stderr); exit(2); }
  return opened.value;
}
static KuObject* redis_config_base(void) {
  KuObject* config = ku_object_new(4);
  ku_object_set(config, text("host"), ku_v_str(text("127.0.0.1")));
  return config;
}
static KuObject* redis_auth_config(void) {
  KuObject* config = redis_config_base();
  ku_object_set(config, text("username"), ku_v_str(text("alice")));
  ku_object_set(config, text("password"), ku_v_str(text("secret")));
  return config;
}
static int rejects_dynamic_config(KuObject* config) {
  KuResult_redis_client result = ku_redis_client(config);
  int rejected = !result.ok && !result.value && static_error(result.error, "invalid_config");
  if (result.ok) ku_redis_close(result.value);
  else ku_error_drop(&result.error);
  ku_object_drop(config);
  return rejected;
}
static int run_case(const char* mode) {
  if (!strcmp(mode, "normal")) {
    KuRedis* conn = open_connection(); set_reply(conn, "$5\r\nhello\r\n");
    KuResult_str got = ku_redis_connection_get(conn, text("key"), ku_redis_deadline_after_ms(5000)); CHECK(got.ok && equals(got.value, "hello"));
    ku_string_drop(&got.value); set_reply(conn, "+PONG\r\n");
    KuResult_null pong = ku_redis_connection_ping(conn, ku_redis_deadline_after_ms(5000)); CHECK(pong.ok && !close_calls && recv_calls == 2);
    ku_redis_connection_destroy(conn); conn = 0; CHECK(!conn && close_calls == 1 && resolve_calls == 1 && free_address_calls == 1);
  } else if (!strcmp(mode, "get-body-oom")) {
    KuRedis* conn = open_connection(); set_reply(conn, "$5\r\n"); fail_all_allocations = 1;
    KuResult_str got = ku_redis_connection_get(conn, text("key"), ku_redis_deadline_after_ms(5000));
    CHECK(!got.ok && !got.value.ptr && static_error(got.error, "out_of_memory") && injected_failures == 1);
    CHECK(close_calls == 1 && !ku_redis_is_open(conn) && recv_calls == 1);
    ku_error_drop(&got.error); int sent = send_calls;
    KuResult_null pong = ku_redis_connection_ping(conn, ku_redis_deadline_after_ms(5000)); CHECK(!pong.ok && send_calls == sent && injected_failures == 1);
    ku_error_drop(&pong.error); ku_redis_connection_destroy(conn); conn = 0; CHECK(!conn && close_calls == 1);
  } else if (!strcmp(mode, "connect-host-oom") || !strcmp(mode, "connect-owner-oom")) {
    int host_failure = !strcmp(mode, "connect-host-oom");
    fail_size = host_failure ? sizeof("127.0.0.1") : sizeof(KuRedis);
    KuRedisOpenResult opened = ku_redis_open_connection(
        text("127.0.0.1"), 6379, 5000, ku_redis_deadline_after_ms(5000));
    CHECK(!opened.ok && !opened.value && static_error(opened.error, "out_of_memory") && injected_failures == 1 && !fail_size);
    CHECK(host_failure ? (!resolve_calls && !socket_calls && !close_calls) : (resolve_calls == 1 && socket_calls == 1 && close_calls == 1));
    ku_error_drop(&opened.error);
  } else if (!strcmp(mode, "server-error-static")) {
    KuRedis* conn = open_connection(); set_reply(conn, "-ERR reflected secret fixture\r\n"); fail_all_allocations = 1;
    KuResult_str got = ku_redis_connection_get(conn, text("key"), ku_redis_deadline_after_ms(5000));
    CHECK(!got.ok && !got.value.ptr && static_error(got.error, "redis_error")
        && equals(got.error.message, "redis server returned an error") && injected_failures == 0);
    CHECK(ku_redis_is_open(conn) && !close_calls && reply_position == reply_length);
    ku_error_drop(&got.error); set_reply(conn, "+PONG\r\n");
    KuResult_null pong = ku_redis_connection_ping(conn, ku_redis_deadline_after_ms(5000)); CHECK(pong.ok && injected_failures == 0);
    ku_redis_connection_destroy(conn); conn = 0; CHECK(!conn && close_calls == 1);
  } else if (!strcmp(mode, "auth-rejection-static")) {
    KuRedis* conn = open_connection(); set_reply(conn, "-ERR user alice supplied password secret\r\n");
    fail_all_allocations = 1;
    KuResult_null auth = ku_redis_connection_auth_user(
        conn, text("alice"), text("secret"), ku_redis_deadline_after_ms(5000));
    CHECK(!auth.ok && static_error(auth.error, "auth_failed")
        && equals(auth.error.message, "redis authentication failed") && injected_failures == 0
        && ku_redis_is_open(conn) && !close_calls);
    ku_error_drop(&auth.error); ku_redis_connection_destroy(conn); conn = 0;
    CHECK(!conn && close_calls == 1);
  } else if (!strcmp(mode, "auth-oom-classification")) {
    KuObject* config = redis_auth_config();
    fail_size = 5; /* username clone, after the host copy */
    KuResult_redis_client client = ku_redis_client(config);
    CHECK(!client.ok && !client.value && static_error(client.error, "out_of_memory")
        && !equals(client.error.code, "auth_failed") && injected_failures == 1
        && !fail_size && !resolve_calls && !socket_calls && !close_calls);
    ku_error_drop(&client.error); ku_object_drop(config);
  } else if (!strcmp(mode, "auth-expired-classification")) {
    KuRedis* conn = open_connection();
    KuResult_null auth = ku_redis_connection_auth_user(
        conn, text("alice"), text("secret"), clock_ms);
    CHECK(!auth.ok && static_error(auth.error, "timeout")
        && !equals(auth.error.code, "auth_failed") && ku_redis_is_open(conn)
        && !close_calls && !send_calls);
    ku_error_drop(&auth.error); ku_redis_connection_destroy(conn); conn = 0;
    CHECK(!conn && close_calls == 1);
  } else if (!strcmp(mode, "connect-os-timeout")
      || !strcmp(mode, "connect-pending-os-timeout")) {
    if (!strcmp(mode, "connect-os-timeout")) fail_connect_timeout = 1;
    else { connect_pending = 1; connect_wait_ready = 1; pending_connect_timeout = 1; }
    KuRedisOpenResult opened = ku_redis_open_connection(
        text("127.0.0.1"), 6379, 5000, ku_redis_deadline_after_ms(5000));
    CHECK(!opened.ok && !opened.value && static_error(opened.error, "timeout")
        && !fail_connect_timeout && !connect_wait_ready && !pending_connect_timeout
        && connect_calls == 1 && close_calls == 1);
    ku_error_drop(&opened.error);
  } else if (!strcmp(mode, "send-os-timeout")
      || !strcmp(mode, "recv-os-timeout")
      || !strcmp(mode, "recv-transport-error")
      || !strcmp(mode, "send-deadline-overrun")
      || !strcmp(mode, "recv-deadline-overrun")) {
    KuRedis* conn = open_connection(); set_reply(conn, "+PONG\r\n");
    unsigned long long deadline = clock_ms + 7;
    if (!strcmp(mode, "send-os-timeout")) fail_send_timeout = 1;
    else if (!strcmp(mode, "recv-os-timeout")) fail_recv_timeout = 1;
    else if (!strcmp(mode, "recv-transport-error")) fail_recv_transport = 1;
    else if (!strcmp(mode, "send-deadline-overrun")) send_elapsed = 7;
    else recv_elapsed = 7;
    KuResult_null pong = ku_redis_connection_ping(conn, deadline);
    const char* expected = !strcmp(mode, "recv-transport-error") ? "redis_error" : "timeout";
    CHECK(!pong.ok && static_error(pong.error, expected) && !ku_redis_is_open(conn)
        && close_calls == 1);
    ku_error_drop(&pong.error); ku_redis_connection_destroy(conn); conn = 0;
    CHECK(!conn && close_calls == 1);
  } else if (!strcmp(mode, "pool-expired-classification")) {
    KuRedis* conn = open_connection();
    KuRedisClient* client = (KuRedisClient*)ku_test_malloc(sizeof(KuRedisClient));
    CHECK(client); memset(client, 0, sizeof(*client));
    client->idle = (KuRedis**)ku_test_malloc(sizeof(KuRedis*)); CHECK(client->idle); client->idle[0] = 0;
    CHECK(ku_redis_pool_sync_init(&client->sync) == 0);
    client->max_connections = 1; client->max_waiters = 1;
    client->connect_timeout_ms = 5000; client->acquire_timeout_ms = 5000;
    client->command_timeout_ms = 5000; client->total_connections = 1; client->borrowed = 1;
    KuRedisLeaseResult lease = ku_redis_client_acquire(client, clock_ms);
    CHECK(!lease.ok && !lease.value && static_error(lease.error, "timeout")
        && !client->waiters && !close_calls);
    ku_error_drop(&lease.error);
    ku_redis_close(client);
    ku_redis_client_release(client, conn); client = 0; conn = 0;
    CHECK(!client && !conn && close_calls == 1);
  } else if (!strcmp(mode, "waiter-handoff-helper")) {
    KuRedis* conn = open_connection();
    KuRedisClient* client = (KuRedisClient*)ku_test_malloc(sizeof(KuRedisClient));
    CHECK(client); memset(client, 0, sizeof(*client));
    client->idle = (KuRedis**)ku_test_malloc(sizeof(KuRedis*)); CHECK(client->idle);
    client->idle[0] = conn; client->idle_count = 1; client->total_connections = 1;
    client->max_connections = 1; client->waiters = 1;
    CHECK(ku_redis_pool_sync_init(&client->sync) == 0);
    int before_wake = ku_test_redis_wake_one_calls;
    ku_redis_client_handoff_available_locked(client);
    CHECK(ku_test_redis_wake_one_calls == before_wake + 1);
    client->idle_count = 0;
    ku_redis_client_handoff_available_locked(client);
    CHECK(ku_test_redis_wake_one_calls == before_wake + 1);
    client->idle_count = 1; client->waiters = 0;
    ku_redis_close(client); client = 0; conn = 0;
    CHECK(!client && !conn && close_calls == 1);
  } else if (!strcmp(mode, "max-waiters-zero-deferred-close")) {
    KuRedis* conn = open_connection();
    /* Pool storage is released by runtime functions compiled through the
       allocation hooks, so construct this white-box fixture through the same
       hooks instead of mixing it with the host allocator. */
    KuRedisClient* client = (KuRedisClient*)ku_test_malloc(sizeof(KuRedisClient));
    CHECK(client); memset(client, 0, sizeof(*client));
    client->idle = (KuRedis**)ku_test_malloc(sizeof(KuRedis*)); CHECK(client->idle); client->idle[0] = 0;
    CHECK(ku_redis_pool_sync_init(&client->sync) == 0);
    client->max_connections = 1; client->max_waiters = 0;
    client->connect_timeout_ms = 5000; client->acquire_timeout_ms = 5000;
    client->command_timeout_ms = 5000; client->total_connections = 1; client->borrowed = 1;
    KuRedisLeaseResult rejected = ku_redis_client_acquire(client, ku_redis_deadline_after_ms(5000));
    CHECK(!rejected.ok && !rejected.value && static_error(rejected.error, "pool_exhausted"));
    ku_error_drop(&rejected.error);
    ku_redis_close(client); /* borrowed connection keeps the client alive without blocking */
    CHECK(close_calls == 0);
    ku_redis_client_release(client, conn); /* last borrower disposes exactly once */
    conn = 0; CHECK(close_calls == 1);
  } else if (!strcmp(mode, "credential-wipe-close")) {
    KuRedisClient* client = (KuRedisClient*)ku_test_malloc(sizeof(KuRedisClient));
    CHECK(client); memset(client, 0, sizeof(*client));
    client->max_connections = 1;
    client->idle = (KuRedis**)ku_test_malloc(sizeof(KuRedis*));
    CHECK(client->idle); client->idle[0] = 0;
    CHECK(ku_redis_pool_sync_init(&client->sync) == 0);
    client->username.ptr = (uint8_t*)ku_test_malloc(5);
    client->password.ptr = (uint8_t*)ku_test_malloc(6);
    CHECK(client->username.ptr && client->password.ptr);
    memcpy(client->username.ptr, "alice", 5);
    memcpy(client->password.ptr, "secret", 6);
    client->username.len = client->username.capacity = 5;
    client->password.len = client->password.capacity = 6;
    client->username.storage = client->password.storage = KU_STRING_OWNED;
    wipe_username = client->username.ptr;
    wipe_password = client->password.ptr;
    ku_redis_close(client);
    client = 0;
    CHECK(!client && !wipe_username && !wipe_password && credential_wipes == 2);
  } else if (!strcmp(mode, "dynamic-config-validation")) {
    KuObject* config = redis_config_base();
    ku_object_set(config, text("extra"), ku_v_int(1));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("max_connections"), ku_v_str(text("8")));
    CHECK(rejects_dynamic_config(config));

    config = ku_object_new(2);
    ku_object_set(config, text("host"), ku_v_str(ku_string_static(
        (const uint8_t*)"bad\0host", 8)));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("username"), ku_v_str(text("alice")));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("username"), ku_v_str(ku_string_static(
        (const uint8_t*)"bad\0user", 8)));
    ku_object_set(config, text("password"), ku_v_str(text("secret")));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("password"), ku_v_str(ku_string_static(
        (const uint8_t*)"bad\0secret", 10)));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("max_waiters"), ku_v_int(4097));
    CHECK(rejects_dynamic_config(config));

    config = ku_object_new(2);
    ku_object_set(config, text("host"), ku_v_str((KuString){
        (uint8_t*)"x", (size_t)KU_REDIS_MAX_CONFIG_BYTES + 1, 0, KU_STRING_STATIC }));
    CHECK(rejects_dynamic_config(config));

    config = ku_object_new(2);
    ku_object_set(config, text("host"), ku_v_str((KuString){
        0, 1, 0, KU_STRING_STATIC }));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("username"), ku_v_str((KuString){
        (uint8_t*)"x", (size_t)KU_REDIS_MAX_CONFIG_BYTES + 1, 0, KU_STRING_STATIC }));
    ku_object_set(config, text("password"), ku_v_str(text("secret")));
    CHECK(rejects_dynamic_config(config));

    config = redis_config_base();
    ku_object_set(config, text("password"), ku_v_str((KuString){
        (uint8_t*)"x", (size_t)KU_REDIS_MAX_CONFIG_BYTES + 1, 0, KU_STRING_STATIC }));
    CHECK(rejects_dynamic_config(config));
    CHECK(!resolve_calls && !socket_calls && !close_calls);
  } else if (!strcmp(mode, "static-error-no-allocation")) {
    fail_all_allocations = 1;
    KuRedisOpenResult invalid = ku_redis_open_connection(
        text(""), 6379, 5000, ku_redis_deadline_after_ms(5000));
    CHECK(!invalid.ok && static_error(invalid.error, "redis_error")); ku_error_drop(&invalid.error);
    KuResult_str closed = ku_redis_connection_get(0, text("key"), ku_redis_deadline_after_ms(5000));
    CHECK(!closed.ok && static_error(closed.error, "redis_error")); ku_error_drop(&closed.error);
    CHECK(!allocation_calls && !injected_failures && !resolve_calls && !socket_calls);
  } else {
    expire_at = clock_ms + 7; __ku_handler_deadline = expire_at;
    if (!strcmp(mode, "expired-handler")) __ku_handler_deadline = clock_ms;
    else if (!strcmp(mode, "resolver-overrun")) resolve_elapsed = 7;
    else if (!strcmp(mode, "connect-wait-budget")) connect_pending = 1;
    else if (!strcmp(mode, "connect-late-success")) connect_elapsed = 7;
    else if (!strcmp(mode, "connect-setup-overrun")) expire_setup = 1;
    else if (!strcmp(mode, "connect-owner-overrun")) expire_owner = 1;
    else { fputs("unknown test case\n", stderr); return 2; }
    KuRedisOpenResult opened = ku_redis_open_connection(
        text("127.0.0.1"), 6379, 5000, ku_redis_deadline_after_ms(5000));
    CHECK(!opened.ok && !opened.value && static_error(opened.error, "timeout")); ku_error_drop(&opened.error);
    if (!strcmp(mode, "expired-handler")) CHECK(!allocation_calls && !resolve_calls && !socket_calls);
    else {
      CHECK(resolve_calls == 1 && free_address_calls == 1 && clock_ms == expire_at);
      if (!strcmp(mode, "resolver-overrun")) CHECK(!socket_calls && !close_calls);
      else CHECK(socket_calls == 1 && close_calls == 1);
      if (!strcmp(mode, "connect-wait-budget")) CHECK(wait_calls == 1 && wait_budget == 7);
      if (!strcmp(mode, "connect-setup-overrun")) CHECK(!expire_setup);
      if (!strcmp(mode, "connect-owner-overrun")) CHECK(!expire_owner);
    }
  }
  if (bad_calls || socket_live || live_allocations || live_bytes) {
    fprintf(stderr, "fixture state: bad=%d socket=%d allocations=%zu bytes=%zu\n",
        bad_calls, socket_live, live_allocations, live_bytes);
  }
  CHECK(!bad_calls && !socket_live && !live_allocations && !live_bytes);
  return 0;
}
int main(int argc, char** argv) {
  if (argc != 2) return 2;
  int result = run_case(argv[1]);
  if (!result) printf("redis failure closed loop: %s\n", argv[1]);
  return result;
}
"#;
