//! Native binary-data and network foundation tests. Local loopback peers keep
//! protocol, timeout and EOF behavior deterministic and offline.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::process::Command;
use std::thread;
use std::time::Duration;

use ku::cli::check_source;
use native_harness::{compile_harness, emit_c, run_bounded, BoundedOutput, OutputLimits, TempDir};

const RUN_TIMEOUT: Duration = Duration::from_secs(20);
const RUN_LIMITS: OutputLimits = OutputLimits::new(1024 * 1024, 2 * 1024 * 1024);

fn compile_generated(
    directory: &TempDir,
    generated: &str,
    stem: &str,
) -> Option<std::path::PathBuf> {
    let source = directory.path().join(format!("{stem}.c"));
    fs::write(&source, generated).expect("write native net C artifact");
    compile_harness(directory.path(), &source, stem)
}

fn run_native(executable: &std::path::Path) -> BoundedOutput {
    let mut command = Command::new(executable);
    command.current_dir(
        executable
            .parent()
            .expect("native net executable directory"),
    );
    run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("native net executable was not bounded: {error}"))
}

fn assert_success(output: &BoundedOutput, expected: &str) {
    assert!(
        output.status.success(),
        "native program failed with status {:?}:\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        expected
    );
    assert!(output.stderr.is_empty(), "unexpected native stderr");
}

#[test]
fn native_bytes_owned_clone_bounds_and_utf8_are_closed_loop() {
    let directory = TempDir::new("bytes-owned");
    let generated = emit_c(
        directory.path(),
        r#"import bytes from "std.bytes"
fn main(): null! {
    values = [65, 0, 255]
    data = bytes.from_array(values)?
    copy = data.clone()
    group = [data.clone(), copy.clone()]
    group_copy = group.clone()
    first = group_copy[0].clone()
    captured_source = data.clone()
    load = () => captured_source.clone()
    captured = load()
    println(values.len())
    println(data.len())
    println(data.get(1)?)
    println(first.get(0)?)
    println(captured.get(2)?)
    try {
        bytes.from_array([256])?
    } catch(err) {
        println(err.code)
    }
    try {
        data.get(9)?
    } catch(err) {
        println(err.code)
    }
    try {
        println(copy.to_str()?)
    } catch(err) {
        println(err.domain)
        println(err.code)
    }
    text = bytes.from_str("hé")?
    println(text.to_str()?)
    return ok(null)
}
"#,
    );
    assert!(generated.contains("typedef struct KuBytes {"));
    assert!(generated.contains("static KuBytes ku_clone_bytes(KuBytes value)"));
    assert!(generated.contains("#define KU_BYTES_MAX_LENGTH (64ULL * 1024ULL * 1024ULL)"));
    let Some(executable) = compile_generated(&directory, &generated, "bytes-owned") else {
        return;
    };
    assert_success(
        &run_native(&executable),
        "3\n3\n0\n65\n255\ninvalid_byte\nindex_out_of_bounds\nbytes\ninvalid_utf8\nhé\n",
    );
}

#[test]
fn native_net_write_borrows_bytes_and_read_preserves_binary() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind native net peer");
    let port = listener
        .local_addr()
        .expect("native net peer address")
        .port();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept native net client");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("bound native net peer read");
        let mut received = [0u8; 3];
        stream
            .read_exact(&mut received)
            .expect("read native net bytes");
        assert_eq!(received, [65, 0, 255]);
        stream
            .write_all(&[255])
            .expect("write native net binary response");
        stream.flush().expect("flush native net response");
        let mut eof = [0u8; 1];
        assert_eq!(stream.read(&mut eof).expect("wait for native net close"), 0);
    });

    let directory = TempDir::new("net-binary");
    let generated = emit_c(
        directory.path(),
        &format!(
            r#"import bytes from "std.bytes"
import net from "std.net"
fn main(): null! {{
    payload = bytes.from_array([65, 0, 255])?
    client = net.client({{ host: "127.0.0.1", port: {port}, tls: false, connect_timeout_ms: 1000,
        read_timeout_ms: 1000, write_timeout_ms: 1000, max_read_bytes: 32 }})?
    client.write(payload)?
    println(payload.get(2)?)
    reply = client.read(1)?
    println(reply.len())
    println(reply.get(0)?)
    try {{
        println(reply.to_str()?)
    }} catch(err) {{
        println(err.domain)
        println(err.code)
    }}
    client.close()
    return ok(null)
}}
"#
        ),
    );
    assert!(generated.contains("#define KU_NATIVE_RUNTIME_NET_SOCKET 1"));
    assert!(generated.contains("typedef SOCKET KuNetSocket;"));
    assert!(generated.contains("typedef int KuNetSocket;"));
    assert!(generated.contains("static int ku_net_gate_acquire("));
    assert!(
        !generated.contains("#define KU_FEATURE_NATIVE_TLS 1"),
        "plain TCP must not require the native TLS target pack"
    );
    let Some(executable) = compile_generated(&directory, &generated, "net-binary") else {
        drop(peer);
        return;
    };
    assert_success(
        &run_native(&executable),
        "255\n1\n255\nbytes\ninvalid_utf8\n",
    );
    peer.join().expect("native net peer");
}

#[test]
fn tls_shaped_objects_without_net_client_do_not_link_the_tls_pack() {
    let directory = TempDir::new("net-unrelated-tls-config");
    let generated = emit_c(
        directory.path(),
        r#"fn main() {
    config = { tls: true, tls_server_name: "example.com", tls_ca_pem: "test-ca" }
    println("ok")
}
"#,
    );
    assert!(
        !generated.contains("#define KU_FEATURE_NATIVE_TLS 1"),
        "an unrelated TLS-shaped object must not add a target-pack dependency"
    );
    assert!(!generated.contains("ku_tls_v1_client_new"));
}

#[test]
fn native_net_timeout_and_eof_are_structured_and_poison_the_stream() {
    for (label, stall, expected_code) in [
        ("timeout", true, "read_timeout"),
        ("eof", false, "end_of_stream"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind native net failure peer");
        let port = listener
            .local_addr()
            .expect("native net failure address")
            .port();
        let peer = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept native net failure client");
            if stall {
                thread::sleep(Duration::from_millis(300));
            }
            stream.shutdown(Shutdown::Both).ok();
        });
        let directory = TempDir::new(label);
        let generated = emit_c(
            directory.path(),
            &format!(
                r#"import net from "std.net"
fn main(): null! {{
    client = net.client({{ host: "127.0.0.1", port: {port}, connect_timeout_ms: 1000,
        read_timeout_ms: 50, write_timeout_ms: 1000, max_read_bytes: 8 }})?
    try {{
        client.read(1)?
    }} catch(err) {{
        println(err.domain)
        println(err.code)
    }}
    try {{
        client.read(1)?
    }} catch(err) {{
        println(err.code)
    }}
    client.close()
    return ok(null)
}}
"#
            ),
        );
        let Some(executable) = compile_generated(&directory, &generated, label) else {
            drop(peer);
            return;
        };
        assert_success(
            &run_native(&executable),
            &format!("net\n{expected_code}\nclient_closed\n"),
        );
        peer.join().expect("native net failure peer");
    }
}

#[test]
fn native_net_read_oom_is_structured_and_releases_the_gate() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind native net OOM peer");
    let port = listener
        .local_addr()
        .expect("native net OOM address")
        .port();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept native net OOM client");
        let mut byte = [0u8; 1];
        stream
            .read_exact(&mut byte)
            .expect("gate must remain usable after read OOM");
        assert_eq!(byte, [65]);
    });
    let directory = TempDir::new("net-read-oom");
    let generated = emit_c(
        directory.path(),
        &format!(
            r#"import bytes from "std.bytes"
import net from "std.net"
fn main(): null! {{
    client = net.client({{ host: "127.0.0.1", port: {port}, connect_timeout_ms: 1000,
        read_timeout_ms: 1000, write_timeout_ms: 1000, max_read_bytes: 8 }})?
    try {{
        client.read(1)?
    }} catch(err) {{
        println(err.domain)
        println(err.code)
    }}
    payload = bytes.from_array([65])?
    client.write(payload)?
    client.close()
    return ok(null)
}}
"#
        ),
    );
    let allocation = "result.ptr = (uint8_t*)malloc(result.capacity);";
    assert_eq!(
        generated.matches(allocation).count(),
        1,
        "read allocation injection point must remain unique"
    );
    let generated = generated.replace(allocation, "result.ptr = NULL;");
    let Some(executable) = compile_generated(&directory, &generated, "net-read-oom") else {
        drop(peer);
        return;
    };
    assert_success(&run_native(&executable), "net\nout_of_memory\n");
    peer.join().expect("native net OOM peer");
}

#[test]
fn native_net_gate_sync_failures_poison_instead_of_reusing_the_stream() {
    for mode in ["acquire", "release"] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gate-fault peer");
        let port = listener.local_addr().expect("gate-fault address").port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gate-fault client");
            let mut sink = Vec::new();
            stream.read_to_end(&mut sink).expect("gate-fault EOF");
        });
        let directory = TempDir::new(&format!("net-gate-{mode}-fault"));
        let mut generated = emit_c(
            directory.path(),
            &format!(
                r#"import bytes from "std.bytes"
import net from "std.net"
fn main(): null! {{
    client = net.client({{ host: "127.0.0.1", port: {port}, connect_timeout_ms: 1000,
        read_timeout_ms: 1000, write_timeout_ms: 1000, max_read_bytes: 8 }})?
    payload = bytes.from_array([65])?
    try {{
        client.write(payload)?
    }} catch(err) {{
        println(err.code)
    }}
    try {{
        client.read(1)?
    }} catch(err) {{
        println(err.code)
    }}
    try {{
        client.read(1)?
    }} catch(err) {{
        println(err.code)
    }}
    client.close()
    return ok(null)
}}
"#
            ),
        );
        if mode == "acquire" {
            let marker = "int acquired = ku_net_gate_acquire(&client->gate, deadline);";
            assert_eq!(generated.matches(marker).count(), 2);
            generated = generated.replacen(marker, "int acquired = -1;", 1);
        } else {
            let release = "static int ku_net_gate_release(KuNetGate* gate) {";
            assert_eq!(generated.matches(release).count(), 1);
            generated = generated.replace(
                release,
                "static int ku_net_gate_release(KuNetGate* gate) {\n  (void)gate; return -1;",
            );
        }
        let Some(executable) =
            compile_generated(&directory, &generated, &format!("net-gate-{mode}-fault"))
        else {
            drop(peer);
            return;
        };
        assert_success(
            &run_native(&executable),
            "sync_error\nclient_closed\nclient_closed\n",
        );
        peer.join().expect("native net gate-fault peer");
    }
}

#[test]
fn native_net_gate_contention_has_a_bounded_deadline_and_recovers() {
    let directory = TempDir::new("net-gate");
    let generated = emit_c(
        directory.path(),
        r#"import net from "std.net"
fn main(): null! {
    client = net.client({ host: "127.0.0.1", port: 9 })?
    client.close()
    return ok(null)
}

"#,
    );
    assert!(generated.contains("return ReleaseSemaphore(gate->semaphore, 1, NULL) ? 0 : -1;"));
    assert!(generated.contains("return pthread_mutex_unlock(&gate->mutex) == 0 ? 0 : -1;"));
    assert_eq!(
        generated
            .matches(
                "if (acquired < 0) ku_net_atomic_flag_set(&client->poison_requested);",
            )
            .count(),
        2,
        "read and write must make synchronization failure terminal without touching an unowned transport"
    );
    let mut harness = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    harness.push_str(
        r#"
static KuNetGate ku_test_gate;
static int ku_test_gate_result = -99;
#if defined(_WIN32)
static unsigned __stdcall ku_test_gate_waiter(void* ignored) {
#else
static void* ku_test_gate_waiter(void* ignored) {
#endif
  (void)ignored;
  ku_test_gate_result = ku_net_gate_acquire(
      &ku_test_gate, ku_net_deadline_after_ms(50));
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

int main(void) {
  if (ku_net_gate_init(&ku_test_gate) != 0) return 10;
  if (ku_net_gate_acquire(&ku_test_gate, ku_net_deadline_after_ms(1000)) != 1) return 11;
#if defined(_WIN32)
  uintptr_t worker = _beginthreadex(NULL, 0, ku_test_gate_waiter, NULL, 0, NULL);
  if (!worker) return 12;
  DWORD joined = WaitForSingleObject((HANDLE)worker, 1000);
  CloseHandle((HANDLE)worker);
  if (joined != WAIT_OBJECT_0) return 13;
#else
  pthread_t worker;
  if (pthread_create(&worker, NULL, ku_test_gate_waiter, NULL) != 0) return 12;
  if (pthread_join(worker, NULL) != 0) return 13;
#endif
  if (ku_test_gate_result != 0) return 14;
  if (ku_net_gate_release(&ku_test_gate) != 0) return 15;
  if (ku_net_gate_acquire(&ku_test_gate, ku_net_deadline_after_ms(1000)) != 1) return 16;
  if (ku_net_gate_release(&ku_test_gate) != 0) return 17;
  ku_net_gate_destroy(&ku_test_gate);
  puts("net gate closed loop");
  return 0;
}
"#,
    );
    let Some(executable) = compile_generated(&directory, &harness, "net-gate") else {
        return;
    };
    assert_success(&run_native(&executable), "net gate closed loop\n");
}

#[test]
fn native_net_real_read_write_contention_obeys_the_write_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind contention peer");
    let port = listener
        .local_addr()
        .expect("contention peer address")
        .port();
    let peer = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept contention client");
        thread::sleep(Duration::from_millis(400));
        stream.shutdown(Shutdown::Both).ok();
    });
    let directory = TempDir::new("net-real-contention");
    let generated = emit_c(
        directory.path(),
        r#"import net from "std.net"
fn main(): null! {
    client = net.client({ host: "127.0.0.1", port: 9 })?
    client.close()
    return ok(null)
}
"#,
    );
    let mut harness = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    let read_start = harness
        .find("static KuResult_bytes ku_net_read(")
        .expect("net read helper");
    harness.insert_str(
        read_start,
        "static KuNetAtomicFlag ku_test_read_gate_acquired;\n",
    );
    let read_start = harness
        .find("static KuResult_bytes ku_net_read(")
        .expect("net read helper after declaration");
    let acquire_marker = "int acquired = ku_net_gate_acquire(&client->gate, deadline);";
    let acquire = read_start
        + harness[read_start..]
            .find(acquire_marker)
            .expect("net read gate acquisition");
    harness.insert_str(
        acquire + acquire_marker.len(),
        "\n  if (acquired == 1) ku_net_atomic_flag_set(&ku_test_read_gate_acquired);",
    );
    harness.push_str(&format!(
        r#"
static KuNetClient* ku_test_connect(uint16_t port) {{
  if (ku_net_socket_startup() != 0) return NULL;
  KuNetSocket socket_value = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (socket_value == KU_NET_INVALID_SOCKET) return NULL;
  struct sockaddr_in address;
  memset(&address, 0, sizeof(address));
  address.sin_family = AF_INET;
  address.sin_port = htons(port);
  address.sin_addr.s_addr = htonl(0x7f000001UL);
  if (connect(socket_value, (struct sockaddr*)&address, sizeof(address)) != 0
      || ku_net_socket_suppress_sigpipe(socket_value) != 0
      || ku_net_socket_set_nonblocking(socket_value) != 0) {{
    ku_net_socket_close(socket_value); return NULL;
  }}
  KuNetClient* client = (KuNetClient*)calloc(1, sizeof(*client));
  if (!client) {{ ku_net_socket_close(socket_value); return NULL; }}
  ku_net_atomic_flag_init(&client->poison_requested);
  client->socket_value = socket_value;
  client->read_timeout_ms = 250;
  client->write_timeout_ms = 50;
  client->max_read_bytes = 8;
  if (ku_net_gate_init(&client->gate) != 0) {{
    ku_net_socket_close(socket_value); free(client); return NULL;
  }}
  return client;
}}

static KuNetClient* ku_test_client;
static int ku_test_read_timed_out;
#if defined(_WIN32)
static unsigned __stdcall ku_test_reader(void* ignored) {{
#else
static void* ku_test_reader(void* ignored) {{
#endif
  (void)ignored;
  KuResult_bytes result = ku_net_read(ku_test_client, 1);
  ku_test_read_timed_out = !result.ok && result.error.code.len == 12
      && memcmp(result.error.code.ptr, "read_timeout", 12) == 0;
  ku_result_drop_bytes(&result);
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}}

int main(void) {{
  ku_net_atomic_flag_init(&ku_test_read_gate_acquired);
  ku_test_client = ku_test_connect({port});
  if (!ku_test_client) return 20;
#if defined(_WIN32)
  uintptr_t worker = _beginthreadex(NULL, 0, ku_test_reader, NULL, 0, NULL);
  if (!worker) return 21;
#else
  pthread_t worker;
  if (pthread_create(&worker, NULL, ku_test_reader, NULL) != 0) return 21;
#endif
  unsigned long long wait_deadline = ku_net_deadline_after_ms(1000);
  while (!ku_net_atomic_flag_load(&ku_test_read_gate_acquired)
      && ku_net_now_ms() < wait_deadline) {{
#if defined(_WIN32)
    Sleep(1);
#else
    struct timespec pause = {{0, 1000000L}};
    nanosleep(&pause, NULL);
#endif
  }}
  if (!ku_net_atomic_flag_load(&ku_test_read_gate_acquired)) return 22;
  uint8_t byte = 65;
  KuBytes payload = {{ &byte, 1, 0, KU_BYTES_STATIC }};
  KuResult_null write_result = ku_net_write(ku_test_client, payload);
  int write_timed_out = !write_result.ok && write_result.error.code.len == 13
      && memcmp(write_result.error.code.ptr, "write_timeout", 13) == 0;
  ku_result_drop_null(&write_result);
#if defined(_WIN32)
  DWORD joined = WaitForSingleObject((HANDLE)worker, 2000);
  CloseHandle((HANDLE)worker);
  if (joined != WAIT_OBJECT_0) return 23;
#else
  if (pthread_join(worker, NULL) != 0) return 23;
#endif
  if (!write_timed_out || !ku_test_read_timed_out) return 24;
  ku_net_close(ku_test_client);
  puts("net real contention closed loop");
  return 0;
}}
"#
    ));
    let Some(executable) = compile_generated(&directory, &harness, "net-real-contention") else {
        drop(peer);
        return;
    };
    assert_success(
        &run_native(&executable),
        "net real contention closed loop\n",
    );
    peer.join().expect("native net contention peer");
}

#[test]
fn native_net_partial_write_times_out_and_poisons_the_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind partial-write peer");
    let port = listener.local_addr().expect("partial-write address").port();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept partial-write client");
        thread::sleep(Duration::from_millis(250));
        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .expect("read bounded partial write");
        received.len()
    });
    let directory = TempDir::new("net-partial-write");
    let generated = emit_c(
        directory.path(),
        r#"import net from "std.net"
fn main(): null! {
    client = net.client({ host: "127.0.0.1", port: 9 })?
    client.close()
    return ok(null)
}
"#,
    );
    let mut harness = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    harness = harness.replace(
        "#define KU_NET_SOCKET_CHUNK 1073741824U",
        "#define KU_NET_SOCKET_CHUNK 1024U\nstatic int ku_test_send_calls = 0;",
    );
    harness = harness.replacen(
        "size_t chunk = len > KU_NET_SOCKET_CHUNK ? KU_NET_SOCKET_CHUNK : len;",
        r#"size_t chunk = len > KU_NET_SOCKET_CHUNK ? KU_NET_SOCKET_CHUNK : len;
  if (ku_test_send_calls++ != 0) {
#if defined(_WIN32)
    WSASetLastError(WSAEWOULDBLOCK);
#else
    errno = EWOULDBLOCK;
#endif
    return -1;
  }"#,
        1,
    );
    harness.push_str(&format!(
        r#"
static KuNetClient* ku_test_connect(uint16_t port) {{
  if (ku_net_socket_startup() != 0) return NULL;
  KuNetSocket socket_value = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (socket_value == KU_NET_INVALID_SOCKET) return NULL;
  struct sockaddr_in address;
  memset(&address, 0, sizeof(address));
  address.sin_family = AF_INET;
  address.sin_port = htons(port);
  address.sin_addr.s_addr = htonl(0x7f000001UL);
  if (connect(socket_value, (struct sockaddr*)&address, sizeof(address)) != 0)
    {{ ku_net_socket_close(socket_value); return NULL; }}
  int send_buffer = 1024;
  if (setsockopt(socket_value, SOL_SOCKET, SO_SNDBUF,
#if defined(_WIN32)
      (const char*)&send_buffer,
#else
      &send_buffer,
#endif
      sizeof(send_buffer)) != 0
      || ku_net_socket_suppress_sigpipe(socket_value) != 0
      || ku_net_socket_set_nonblocking(socket_value) != 0) {{
    ku_net_socket_close(socket_value); return NULL;
  }}
  KuNetClient* client = (KuNetClient*)calloc(1, sizeof(*client));
  if (!client) {{ ku_net_socket_close(socket_value); return NULL; }}
  ku_net_atomic_flag_init(&client->poison_requested);
  client->socket_value = socket_value;
  client->read_timeout_ms = 1000;
  client->write_timeout_ms = 10;
  client->max_read_bytes = 8;
  if (ku_net_gate_init(&client->gate) != 0) {{
    ku_net_socket_close(socket_value); free(client); return NULL;
  }}
  return client;
}}

int main(void) {{
  KuNetClient* client = ku_test_connect({port});
  if (!client) return 30;
  size_t length = 32U * 1024U * 1024U;
  uint8_t* data = (uint8_t*)malloc(length);
  if (!data) return 31;
  memset(data, 65, length);
  KuBytes payload = {{ data, length, length, KU_BYTES_OWNED }};
  KuResult_null result = ku_net_write(client, payload);
  int timed_out = !result.ok && result.error.code.len == 13
      && memcmp(result.error.code.ptr, "write_timeout", 13) == 0;
  if (!timed_out) {{
    if (!result.ok) ku_string_write(stderr, result.error.code);
    return 32;
  }}
  ku_result_drop_null(&result);
  ku_drop_bytes(&payload);
  if (client->socket_value != KU_NET_INVALID_SOCKET) return 33;
  ku_net_close(client);
  puts("net partial write poisoned");
  return 0;
}}
"#
    ));
    let Some(executable) = compile_generated(&directory, &harness, "net-partial-write") else {
        drop(peer);
        return;
    };
    assert_success(&run_native(&executable), "net partial write poisoned\n");
    let received = peer.join().expect("native net partial-write peer");
    assert!(received > 0, "test must observe a real partial write");
    assert!(
        received < 32 * 1024 * 1024,
        "write deadline must stop before the full payload"
    );
}

#[test]
fn native_net_tls_artifact_and_unavailable_runtime_are_fail_closed() {
    let directory = TempDir::new("net-tls-contract");
    let generated = emit_c(
        directory.path(),
        r#"import net from "std.net"
fn main(): null! {
    enabled = false
    try {
        client = net.client({ host: "127.0.0.1", port: 9, tls: enabled, tls_server_name: "localhost" })?
        client.close()
    } catch(err) {
        println(err.code)
    }
    try {
        client = net.client({ host: "127.0.0.1", port: 9, tls: true, tls_ca_pem: "test-ca" })?
        client.close()
    } catch(err) {
        println(err.code)
    }
    return ok(null)
}
"#,
    );
    for marker in [
        "#define KU_FEATURE_NATIVE_TLS 1",
        "#if defined(KU_NATIVE_TLS_ENABLED)",
        "typedef struct KuTlsConfig KuTlsConfig;",
        "typedef struct KuTlsClientSession KuTlsClientSession;",
        "extern uint32_t ku_tls_abi_version(void);",
        "extern uint32_t ku_tls_v1_build_id(",
        "extern uint32_t ku_tls_v1_config_new(",
        "extern uint32_t ku_tls_v1_config_drop(",
        "extern uint32_t ku_tls_v1_client_new(",
        "extern uint32_t ku_tls_v1_client_drop(",
        "extern uint32_t ku_tls_v1_client_wants_read(",
        "extern uint32_t ku_tls_v1_client_wants_write(",
        "extern uint32_t ku_tls_v1_client_is_handshaking(",
        "extern uint32_t ku_tls_v1_client_peer_closed(",
        "extern uint32_t ku_tls_v1_client_feed_ciphertext(",
        "extern uint32_t ku_tls_v1_client_process(",
        "extern uint32_t ku_tls_v1_client_drain_ciphertext(",
        "extern uint32_t ku_tls_v1_client_write_plaintext(",
        "extern uint32_t ku_tls_v1_client_read_plaintext(",
        "extern uint32_t ku_tls_v1_client_send_close_notify(",
        "extern uint32_t ku_tls_v1_client_notify_eof(",
        "KU_TLS_MAX_IO_BYTES 65536u",
        "#if !defined(KU_NATIVE_TLS_ENABLED)",
        "ku_net_error(\"tls_unavailable\"",
        "KU_TLS_ROOTS_WEBPKI : KU_TLS_ROOTS_CUSTOM_PEM",
        "if (tls_enabled && tls_server_name.len == 0) tls_server_name = host_value->as.s;",
        "KU_NET_TLS_MAX_HANDSHAKE_CIPHERTEXT_BYTES 1048576ULL",
        "KU_NET_TLS_MAX_NO_PROGRESS_CALLS 64U",
        "KU_NET_TLS_MAX_HANDSHAKE_DRIVER_CALLS 1048640ULL",
        "KU_NET_TLS_MAX_OPERATION_CIPHERTEXT_BYTES 524320ULL",
        "KU_NET_TLS_MAX_OPERATION_DRIVER_CALLS 524384ULL",
        "net TLS peer closed without close_notify",
        "ku-native-tls/0.1.0;abi=1;rustls=0.23.40;ring=0.17.14;",
        "record-staging=65540;resumption=disabled",
        "uint8_t ciphertext[KU_TLS_MAX_IO_BYTES];",
        "client->tls_pending = (uint8_t*)malloc(pending_len);",
        "feed_steps >= KU_TLS_MAX_IO_BYTES",
        "raw generated-C callers\n     must provide the same lifetime guarantee",
        "Queue, drain once, and attempt one nonblocking send.",
    ] {
        assert!(
            generated.contains(marker),
            "missing TLS contract marker {marker:?}"
        );
    }
    assert!(!generated.contains("skip_verify"));
    assert!(!generated.contains("insecure"));
    assert!(!generated.contains("KuTlsV1Session"));
    assert!(!generated.contains("ku_tls_v1_handshake"));
    assert_eq!(
        generated
            .matches("if (error.code.len == 0 && ku_net_now_ms() >= deadline)")
            .count(),
        2,
        "plain and TLS read/write success must share a final deadline fence"
    );

    let handshake_start = generated
        .find("static int ku_net_tls_handshake_until(")
        .expect("TLS handshake driver must be emitted");
    let handshake_end = generated[handshake_start..]
        .find("static int ku_net_tls_write_until(")
        .map(|offset| handshake_start + offset)
        .expect("TLS write driver must follow the handshake driver");
    let handshake = &generated[handshake_start..handshake_end];
    assert!(
        handshake
            .find("if (!handshaking)")
            .expect("handshake completion check")
            < handshake
                .find("if (driver_calls >= KU_NET_TLS_MAX_HANDSHAKE_DRIVER_CALLS)")
                .expect("handshake call budget check"),
        "completion must win over an exactly exhausted handshake budget"
    );
    assert_eq!(
        handshake.matches("no_progress_calls = 0").count(),
        1,
        "handshake no-progress calls must accumulate instead of resetting after progress"
    );

    let write_start = handshake_end;
    let write_end = generated[write_start..]
        .find("static int ku_net_tls_read_until(")
        .map(|offset| write_start + offset)
        .expect("TLS read driver must follow the write driver");
    let write_driver = &generated[write_start..write_end];
    assert_eq!(write_driver.matches("no_progress_calls = 0").count(), 1);
    assert!(
        write_driver
            .find("ku_tls_v1_client_write_plaintext(")
            .expect("write must first try buffered TLS state")
            < write_driver
                .find("if (driver_calls >= KU_NET_TLS_MAX_OPERATION_DRIVER_CALLS)")
                .expect("write driver call budget")
    );

    let read_start = write_end;
    let read_end = generated[read_start..]
        .find("static int ku_net_gate_init(")
        .map(|offset| read_start + offset)
        .expect("net gate runtime must follow the TLS read driver");
    let read_driver = &generated[read_start..read_end];
    assert_eq!(read_driver.matches("no_progress_calls = 0").count(), 1);
    assert!(
        read_driver
            .find("ku_tls_v1_client_read_plaintext(")
            .expect("read must first try buffered TLS state")
            < read_driver
                .find("if (driver_calls >= KU_NET_TLS_MAX_OPERATION_DRIVER_CALLS)")
                .expect("read driver call budget")
    );
    let Some(executable) = compile_generated(&directory, &generated, "net-tls-contract") else {
        return;
    };
    assert_success(
        &run_native(&executable),
        "invalid_config\ntls_unavailable\n",
    );
}

#[test]
fn bytes_and_net_checker_enforce_one_api_and_ownership_path() {
    let valid = r#"import bytes from "std.bytes"
import net from "std.net"
fn main(): null! {
    values = [1, 2, 3]
    payload = bytes.from_array(values)?
    client = net.client({ host: "127.0.0.1", port: 9 })?
    client.write(payload)?
    println(values.len())
    println(payload.len())
    client.close()
    return ok(null)
}
"#;
    check_source("net-valid.ku", valid).expect("single bytes/net API should check");

    for (label, field) in [
        ("net-tls.ku", "tls: true"),
        (
            "net-tls-name.ku",
            "tls: true, tls_server_name: \"localhost\"",
        ),
        ("net-tls-ca.ku", "tls: true, tls_ca_pem: \"test-ca\""),
    ] {
        let source = format!(
            "import net from \"std.net\"\nfn main(): null! {{ client = net.client({{ host: \"127.0.0.1\", port: 9, {field} }})? client.close() return ok(null) }}\n"
        );
        check_source(label, &source).expect("strict TLS net config should check");
    }

    for (label, source, expected) in [
        (
            "net-camel-case.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, readTimeoutMs: 1 })? return ok(null) }
"#,
            "unknown net client config field 'readTimeoutMs'",
        ),
        (
            "net-insecure-option.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, insecure: true })? return ok(null) }
"#,
            "unknown net client config field 'insecure'",
        ),
        (
            "net-wrong-tls-type.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, tls: "yes" })? return ok(null) }
"#,
            "net.client config field 'tls' must be bool",
        ),
        (
            "net-wrong-tls-ca-type.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, tls: true, tls_ca_pem: 1 })? return ok(null) }
"#,
            "net.client config field 'tls_ca_pem' must be str",
        ),
        (
            "net-wrong-tls-name-type.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, tls: true, tls_server_name: 1 })? return ok(null) }
"#,
            "net.client config field 'tls_server_name' must be str",
        ),
        (
            "net-tls-name-while-disabled.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, tls: false, tls_server_name: "localhost" })? return ok(null) }
"#,
            "field 'tls_server_name' requires 'tls' to be true",
        ),
        (
            "net-tls-ca-without-tls.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9, tls_ca_pem: "test-ca" })? return ok(null) }
"#,
            "field 'tls_ca_pem' requires 'tls' to be true",
        ),
        (
            "net-clone.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9 })? copy = client.clone() return ok(null) }
"#,
            "native resource handles cannot be cloned",
        ),
        (
            "net-use-after-close.ku",
            r#"import net from "std.net"
fn main(): null! { client = net.client({ host: "127.0.0.1", port: 9 })? client.close() client.read(1)? return ok(null) }
"#,
            "use of moved value 'client'",
        ),
        (
            "net-module-write.ku",
            r#"import net from "std.net"
fn main(): null! { net.connect("127.0.0.1", 9)? return ok(null) }
"#,
            "unknown stdlib function 'net.connect'",
        ),
        (
            "net-captured-receiver-effectful-arg.ku",
            r#"import net from "std.net"
fn main(): null! {
    client = net.client({ host: "127.0.0.1", port: 9 })?
    fn ReplaceClient(): int! {
        client = net.client({ host: "127.0.0.1", port: 10 })?
        return ok(1)
    }
    client.read(ReplaceClient()?)?
    return ok(null)
}
"#,
            "cannot call a move-only native receiver rooted at 'client' with an effectful argument",
        ),
    ] {
        let error = check_source(label, source).expect_err("invalid bytes/net contract must fail");
        assert!(
            error.message.contains(expected),
            "{label}: expected {expected:?}, got {:?}",
            error.message
        );
    }
}

#[test]
fn arrays_of_move_only_net_clients_reject_every_clone_requiring_method() {
    for (method, statement) in [
        ("first", "copy = clients.first()"),
        ("last", "copy = clients.last()"),
        ("try_get", "copy = clients.try_get(0)?"),
        ("push", "copy = clients.push(second)"),
        ("concat", "copy = clients.concat(others)"),
        ("map", "copy = clients.map(item => item)"),
    ] {
        let source = format!(
            r#"import net from "std.net"
fn main(): null! {{
    first = net.client({{ host: "127.0.0.1", port: 9 }})?
    second = net.client({{ host: "127.0.0.1", port: 10 }})?
    third = net.client({{ host: "127.0.0.1", port: 11 }})?
    clients = [first]
    others = [third]
    {statement}
    return ok(null)
}}
"#
        );
        let label = format!("net-array-{method}.ku");
        let error = check_source(&label, &source)
            .expect_err("clone-requiring array API must reject move-only elements");
        let expected = format!("array.{method} cannot clone move-only native resource elements");
        assert!(
            error.message.contains(&expected),
            "{label}: expected {expected:?}, got {:?}",
            error.message
        );
    }
}
