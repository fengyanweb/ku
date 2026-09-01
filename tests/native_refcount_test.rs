//! Native closure reference-count regression tests.
//!
//! The generic test compiles on every native platform with an available C
//! toolchain and pins both generated-C atomic implementations. The Windows-only
//! HTTP test drives many workers through shared and nested closure clone/drop
//! paths, which used to race on plain `size_t` counters.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use std::env;
use std::fs;
#[cfg(windows)]
use std::io::{Read, Write};
#[cfg(windows)]
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
#[cfg(windows)]
use std::process::{Child, Stdio};
#[cfg(windows)]
use std::sync::{Arc, Barrier};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bounded_process::{run_bounded, OutputLimits};

const BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const RUN_TIMEOUT: Duration = Duration::from_secs(20);
const BUILD_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(8 * 1024 * 1024, 12 * 1024 * 1024);
const RUN_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(4 * 1024 * 1024, 6 * 1024 * 1024);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ku_binary() -> PathBuf {
    if let Ok(path) = env::var("KU_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_ku") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    let exe = if cfg!(windows) { "ku.exe" } else { "ku" };
    let target_dir = env::var("CARGO_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    [
        target_dir.join("debug").join(exe),
        target_dir.join("release").join(exe),
        repo_root().join("target").join("debug").join(exe),
        repo_root().join("target").join("release").join(exe),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("ku binary not found; set KU_BIN or build the ku binary first")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "ku-native-refcount-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create native refcount temp dir");
    dir
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

struct NativeBuild {
    dir: PathBuf,
    exe: PathBuf,
    c_source: PathBuf,
}

impl NativeBuild {
    fn generated_c_path(&self) -> PathBuf {
        self.c_source.clone()
    }

    fn generated_c(&self) -> String {
        fs::read_to_string(self.generated_c_path()).expect("read generated native C")
    }
}

impl Drop for NativeBuild {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).ok();
    }
}

/// Returns `None` only when no native C compiler is installed.
fn native_build(name: &str, source: &str) -> Option<NativeBuild> {
    let dir = unique_temp_dir(name);
    let entry = "main.ku";
    fs::write(dir.join(entry), source).expect("write Ku refcount source");
    let output_name = exe_name("program");
    let mut command = Command::new(ku_binary());
    command
        .current_dir(&dir)
        .args(["build", "--native", entry, "-o", &output_name]);
    let output = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native refcount build was not bounded: {error}"));
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() && diagnostics.contains("C compiler not found") {
        eprintln!("skip: no C compiler available for native refcount test");
        fs::remove_dir_all(&dir).ok();
        return None;
    }
    if !output.status.success() {
        fs::remove_dir_all(&dir).ok();
        panic!("ku build --native failed for {name}:\n{diagnostics}");
    }
    let c_source = diagnostics
        .lines()
        .find_map(|line| line.strip_prefix("native c ok: "))
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| panic!("native refcount build did not report C output:\n{diagnostics}"));
    let exe = dir.join(output_name);
    assert!(exe.exists(), "native refcount executable was not produced");
    Some(NativeBuild { dir, exe, c_source })
}

#[test]
fn native_closure_refcounts_emit_atomics_and_compile() {
    let source = r#"
fn main(): null! {
    label = "atomic"
    original = () => {
        return label.clone()
    }
    copy = original.clone()
    println(copy())
    middle = () => {
        nested = () => {
            return label.clone()
        }
        return nested()
    }
    println(middle())
    return ok(null)
}
"#;
    let Some(build) = native_build("codegen", source) else {
        return;
    };
    let generated = build.generated_c();

    // Both branches are emitted into portable C; the active toolchain selects
    // MSVC Interlocked or the C11 atomic implementation at preprocessing time.
    assert!(generated.contains("typedef volatile __int64 KuAtomicRefcount;"));
    assert!(generated.contains("_InterlockedCompareExchange64"));
    assert!(generated.contains("typedef _Atomic size_t KuAtomicRefcount;"));
    assert!(generated.contains("atomic_compare_exchange_weak_explicit"));
    assert!(generated.contains("memory_order_acq_rel"));
    assert!(generated.contains("KuAtomicRefcount rc;"));
    assert!(generated.contains("ku_refcount_retain(&c->rc, \"closure cell\")"));
    assert!(generated.contains("ku_refcount_release(&e->rc, \"closure environment\")"));
    assert!(
        generated.contains("_new(__e->label)"),
        "a nested closure must forward its parent's captured cell from __e"
    );
    assert!(!generated.contains("c->rc++"));
    assert!(!generated.contains("--c->rc"));
    assert!(!generated.contains("e->rc++"));
    assert!(!generated.contains("--e->rc"));

    // Optional local/CI validation of the non-MSVC C11 branch. The generated C
    // always contains both implementations; undefining `_MSC_VER` makes Clang's
    // parser select `<stdatomic.h>` even when this test runs on Windows.
    if let Ok(clang_tidy) = env::var("KU_CLANG_TIDY") {
        let mut command = Command::new(clang_tidy);
        command
            .arg(build.generated_c_path())
            .arg("--")
            .args(["-x", "c", "-std=c11", "-U_MSC_VER"]);
        let check = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_OUTPUT_LIMITS)
            .unwrap_or_else(|error| panic!("clang-tidy C11 atomic check was not bounded: {error}"));
        assert!(
            check.status.success(),
            "generated C failed the C11 atomic check:\n{}{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr)
        );
    }

    let mut command = Command::new(&build.exe);
    command.current_dir(&build.dir);
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS)
        .unwrap_or_else(|error| panic!("native atomic refcount program was not bounded: {error}"));
    assert!(
        output.status.success(),
        "native atomic refcount program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "atomic\natomic\n"
    );
}

#[test]
fn native_assignment_only_captures_share_outer_cells_and_missing_names_stay_local() {
    let source = r#"
fn Build(label: str): str {
    println(label)
    return "new-" + label
}

fn main(): null! {
    direct = 1
    set_direct = () => {
        direct = 7
        return null
    }
    set_direct()
    println(direct)

    compound = 1
    add_compound = () => {
        compound += 2
        return null
    }
    add_compound()
    println(compound)

    boxed = 10
    read_boxed = () => {
        return boxed
    }
    boxed += 5
    println(read_boxed())

    left = 2
    right = 3
    set_pair = () => {
        left, right = right, left
        return null
    }
    set_pair()
    println(left)
    println(right)

    owned_left = "old-" + "left"
    owned_right = "old-" + "right"
    set_owned_pair = () => {
        owned_left, owned_right = Build("left"), Build("right")
        return null
    }
    set_owned_pair()
    println(owned_left)
    println(owned_right)

    named_value = 4
    run_named = () => {
        fn set_named() {
            named_value = 12
            return null
        }
        set_named()
        return null
    }
    run_named()
    println(named_value)

    literal_value = 5
    run_literal = () => {
        set_literal = () => {
            literal_value = 13
            return null
        }
        set_literal()
        return null
    }
    run_literal()
    println(literal_value)

    named_parent_local = () => {
        state: int = 14
        fn set_state() {
            state = 22
            return null
        }
        set_state()
        return state
    }
    println(named_parent_local())

    literal_parent_local = () => {
        state: int = 15
        set_state = () => {
            state = 23
            return null
        }
        set_state()
        return state
    }
    println(literal_parent_local())

    local_writer = () => {
        fresh = 21
        return fresh
    }
    println(local_writer())
    fresh = 34
    println(fresh)
    return ok(null)
}
"#;
    let Some(build) = native_build("assignment-captures", source) else {
        return;
    };

    let mut command = Command::new(&build.exe);
    command.current_dir(&build.dir);
    let output =
        run_bounded(&mut command, RUN_TIMEOUT, RUN_OUTPUT_LIMITS).unwrap_or_else(|error| {
            panic!("native assignment capture program was not bounded: {error}")
        });
    assert!(
        output.status.success(),
        "native assignment capture program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "7\n3\n15\n3\n2\nleft\nright\nnew-left\nnew-right\n12\n13\n22\n23\n21\n34\n"
    );
}

#[cfg(windows)]
struct ChildGuard(Option<Child>);

#[cfg(windows)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(windows)]
fn unused_local_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind temporary HTTP port");
    let address = listener.local_addr().expect("temporary HTTP address");
    drop(listener);
    address.to_string()
}

#[cfg(windows)]
fn concurrent_http_request(address: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    let request = b"GET /atomic HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match TcpStream::connect(address) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set HTTP read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .expect("set HTTP write timeout");
                if let Err(error) = stream.write_all(request) {
                    last_error = error.to_string();
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let _ = stream.shutdown(Shutdown::Write);
                let mut response = Vec::new();
                match stream.read_to_end(&mut response) {
                    Ok(_) if !response.is_empty() => {
                        return String::from_utf8_lossy(&response).into_owned();
                    }
                    Ok(_) => last_error = "empty HTTP response".to_string(),
                    Err(error) => last_error = error.to_string(),
                }
            }
            Err(error) => last_error = error.to_string(),
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("native HTTP refcount server did not respond: {last_error}");
}

#[test]
#[cfg(windows)]
fn native_http_workers_concurrently_clone_and_drop_nested_closures() {
    const CLIENTS: usize = 20;
    const ROUNDS: usize = 24;

    let address = unused_local_address();
    let source = r#"
import "std.http"

fn main(): null! {
    prefix = "stable"
    base = () => {
        return prefix.clone()
    }
    app = http.server({
        idle_timeout_ms: 5000,
        read_header_timeout_ms: 5000,
        write_timeout_ms: 5000,
        max_connections: 128,
        max_active_requests: 64,
        max_pending_requests: 64
    })
    app.get("/atomic", fn() {
        base_copy = base.clone()
        nested = () => {
            return prefix.clone() + ":" + base_copy()
        }
        nested_copy = nested.clone()
        spin = 0
        while (spin < 20000) {
            spin = spin + 1
        }
        return http.text(nested_copy())
    })
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &address);
    let Some(build) = native_build("http-workers", &source) else {
        return;
    };
    let generated = build.generated_c();
    assert!(generated.contains("ku_refcount_retain(&c->rc, \"closure cell\")"));
    assert!(generated.contains("ku_refcount_retain(&e->rc, \"closure environment\")"));
    assert!(
        generated.contains("base_copy, __e->prefix"),
        "nested HTTP closure must retain the handler's captured prefix cell"
    );

    let child = Command::new(&build.exe)
        .current_dir(&build.dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn native HTTP refcount server");
    let mut server = ChildGuard(Some(child));

    let barrier = Arc::new(Barrier::new(CLIENTS));
    let clients: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let address = address.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..ROUNDS {
                    let response = concurrent_http_request(&address);
                    assert!(
                        response.starts_with("HTTP/1.1 200 OK"),
                        "unexpected HTTP status: {response}"
                    );
                    assert!(
                        response.ends_with("\r\n\r\nstable:stable"),
                        "unexpected HTTP body: {response}"
                    );
                }
            })
        })
        .collect();
    for client in clients {
        client.join().expect("HTTP refcount client thread");
    }

    let child = server.0.as_mut().expect("HTTP refcount child");
    assert!(
        child
            .try_wait()
            .expect("poll HTTP refcount server")
            .is_none(),
        "native HTTP refcount server exited during concurrent closure traffic"
    );
}
