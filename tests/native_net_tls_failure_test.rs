//! Negative native std.net TLS end-to-end contracts. These tests require the
//! verified target pack and never substitute a mock TLS ABI.

#[path = "support/bounded_process.rs"]
#[allow(dead_code)]
mod bounded_process;

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bounded_process::{run_bounded, OutputLimits};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

const BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LIMITS: OutputLimits = OutputLimits::new(2 * 1024 * 1024, 3 * 1024 * 1024);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before Unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "ku-native-net-tls-negative-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create native TLS negative fixture");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = env::var("KU_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    let exe = if cfg!(windows) { "ku.exe" } else { "ku" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join(exe)
}

fn configured_pack() -> Option<PathBuf> {
    let Some(pack) = env::var_os("KU_NATIVE_TLS_PACK").map(PathBuf::from) else {
        assert_ne!(
            env::var("KU_NATIVE_TLS_LINK_REQUIRED").as_deref(),
            Ok("1"),
            "KU_NATIVE_TLS_LINK_REQUIRED=1 requires KU_NATIVE_TLS_PACK"
        );
        eprintln!("skip: KU_NATIVE_TLS_PACK is not configured");
        return None;
    };
    assert!(pack.is_absolute(), "KU_NATIVE_TLS_PACK must be absolute");
    Some(pack)
}

fn build_native(directory: &Path, pack: &Path, source: &str, label: &str) -> PathBuf {
    fs::write(directory.join("main.ku"), source).expect("write native TLS negative source");
    let output_name = if cfg!(windows) {
        format!("{label}.exe")
    } else {
        label.to_string()
    };
    let mut command = Command::new(ku_binary());
    command
        .current_dir(directory)
        .env("KU_NATIVE_TLS_PACK", pack)
        .args(["build", "--native", "main.ku", "-o", &output_name]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native TLS negative build was not bounded: {error}"));
    assert!(
        output.status.success(),
        "native TLS negative build failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    directory.join(output_name)
}

fn run_native(executable: &Path, expected: &str) {
    let mut command = Command::new(executable);
    command.current_dir(executable.parent().expect("native executable parent"));
    let output = run_bounded(&mut command, RUN_TIMEOUT, OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native TLS negative executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "native TLS negative executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        expected
    );
    assert!(output.stderr.is_empty(), "unexpected native TLS stderr");
}

fn accept_bounded(listener: &TcpListener) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("set bounded TLS accept nonblocking");
    let deadline = Instant::now() + SERVER_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("restore blocking TLS server stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set TLS server read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("set TLS server write timeout");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "TLS server accept timed out");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("TLS server accept failed: {error}"),
        }
    }
}

fn spawn_server(
    listener: TcpListener,
    operation: impl FnOnce(TcpStream) + Send + 'static,
) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        operation(accept_bounded(&listener));
        done_tx.send(()).ok();
    });
    (handle, done_rx)
}

fn finish_server(handle: thread::JoinHandle<()>, done: mpsc::Receiver<()>) {
    done.recv_timeout(SERVER_TIMEOUT)
        .expect("TLS server thread exceeded its hard deadline");
    handle.join().expect("TLS server thread panicked");
}

fn test_server() -> (String, Arc<ServerConfig>) {
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate self-signed TLS certificate");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("select safe TLS protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der())),
        )
        .expect("build TLS server config");
    (cert.pem(), Arc::new(config))
}

fn client_failure_source(port: u16, fields: &str) -> String {
    format!(
        r#"import net from "std.net"
fn main(): null! {{
    try {{
        client = net.client({{ host: "127.0.0.1", port: {port}, tls: true, {fields}, connect_timeout_ms: 300, read_timeout_ms: 300, write_timeout_ms: 300 }})?
        client.close()
    }} catch(err) {{
        println(err.code)
    }}
    return ok(null)
}}
"#
    )
}

#[test]
fn native_net_tls_negative_security_contracts_are_bounded() {
    let Some(pack) = configured_pack() else {
        return;
    };

    for (label, server_name, use_custom_ca) in [
        ("hostname", "wrong.invalid", true),
        ("untrusted", "localhost", false),
    ] {
        let directory = TempDir::new(label);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS validation server");
        let port = listener
            .local_addr()
            .expect("TLS validation address")
            .port();
        let (pem, config) = test_server();
        let fields = if use_custom_ca {
            format!("tls_server_name: {server_name:?}, tls_ca_pem: {pem:?}")
        } else {
            format!("tls_server_name: {server_name:?}")
        };
        let executable = build_native(
            &directory.0,
            &pack,
            &client_failure_source(port, &fields),
            label,
        );
        let (server, done) = spawn_server(listener, move |stream| {
            let connection = ServerConnection::new(config).expect("create validation TLS server");
            let mut tls = StreamOwned::new(connection, stream);
            let mut byte = [0u8; 1];
            let _ = tls.read(&mut byte);
        });
        run_native(&executable, "tls_error\n");
        finish_server(server, done);
    }

    let directory = TempDir::new("stall");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled TLS server");
    let port = listener.local_addr().expect("stalled TLS address").port();
    let source = client_failure_source(port, "tls_server_name: \"localhost\"");
    let executable = build_native(&directory.0, &pack, &source, "stall");
    let (server, done) = spawn_server(listener, |stream| {
        let _stream = stream;
        thread::sleep(Duration::from_millis(750));
    });
    let started = Instant::now();
    run_native(&executable, "connect_timeout\n");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "TLS handshake timeout exceeded its external bound"
    );
    finish_server(server, done);

    let directory = TempDir::new("truncated");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind truncated TLS server");
    let port = listener.local_addr().expect("truncated TLS address").port();
    let (pem, config) = test_server();
    let source = format!(
        r#"import net from "std.net"
fn main(): null! {{
    client = net.client({{ host: "127.0.0.1", port: {port}, tls: true, tls_server_name: "localhost", tls_ca_pem: {pem:?}, connect_timeout_ms: 1000, read_timeout_ms: 1000, write_timeout_ms: 1000 }})?
    first = client.read(1)?
    println(first.get(0)?)
    try {{
        client.read(1)?
    }} catch(err) {{
        println(err.code)
    }}
    return ok(null)
}}
"#
    );
    let executable = build_native(&directory.0, &pack, &source, "truncated");
    let (server, done) = spawn_server(listener, move |stream| {
        let connection = ServerConnection::new(config).expect("create truncated TLS server");
        let mut tls = StreamOwned::new(connection, stream);
        tls.write_all(&[7]).expect("write authenticated TLS byte");
        tls.flush().expect("flush authenticated TLS byte");
        // Drop without close_notify: the client must classify transport EOF as truncation.
    });
    run_native(&executable, "7\ntls_truncated\n");
    finish_server(server, done);
}
