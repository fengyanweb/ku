//! HTTP constant objects are ordinary independent owned values. The checker,
//! interpreter, synchronous C and native HTTP Task roots must agree.

#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    cli::{check_source, run_source},
    ir,
    lexer::Lexer,
    parser::Parser,
    value::Value,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn parse(source: &str) -> ku::ast::Program {
    Parser::new(Lexer::new(source).lex().expect("lex HTTP constants"))
        .parse_program()
        .expect("parse HTTP constants")
}

fn generated(source: &str) -> String {
    // Use the same standard-module resolution as an actual Ku build. A raw
    // Checker does not register the imported http/json namespaces.
    let directory = TempDir::new("native-http-constant-emission");
    emit_c(directory.path(), source)
}

fn execute(name: &str, source: String) -> Option<String> {
    let temp = TempDir::new(name);
    let input = temp.path().join("constants.c");
    fs::write(&input, source).expect("write HTTP constant C");
    let Some(binary) = compile_harness(temp.path(), &input, name) else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires actual C execution"
        );
        return None;
    };
    let output = run_bounded(
        Command::new(binary).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("HTTP constants finish within the existing harness bounds");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).unwrap().replace('\r', ""))
}

#[test]
fn native_http_all_status_and_legacy_code_members_match_interpreter() {
    let mut source = String::from(
        "import http from \"std.http\"\nfn main() {\nstatuses=http.status\ncodes=http.code\n",
    );
    for (table, value) in [
        ("status", ku::stdlib::http::status_object_value()),
        ("code", ku::stdlib::http::code_object_value()),
    ] {
        let Value::Object(values) = value else {
            panic!("HTTP constants must be objects")
        };
        assert_eq!(values.len(), if table == "status" { 32 } else { 10 });
        let mut values = values.into_iter().collect::<Vec<_>>();
        values.sort_by(|a, b| a.0.cmp(&b.0));
        let alias = if table == "status" {
            "statuses"
        } else {
            "codes"
        };
        for (name, value) in values {
            let Value::Int(code) = value else {
                panic!("HTTP constants must be integers")
            };
            source.push_str(&format!("if(http.{table}.{name}!={code} || {alias}.{name}!={code}) {{ panic(\"HTTP constant mismatch\") }}\n"));
        }
    }
    source.push_str("if(http.status.expectationFailed!=417 || http.code.CREATED!=201) {panic(\"HTTP regression\")}\nprintln(\"http-constants-ok\")\n}\n");
    run_source("http-status-members.ku", &source).expect("interpreter checks the same source");
    if let Some(output) = execute("http-status-members", generated(&source)) {
        assert_eq!(output, "http-constants-ok\n");
    }
}

#[test]
fn native_http_status_aliases_clone_reassign_mutate_and_encode_without_leaks() {
    let source = r#"
import http from "std.http"
import json from "std.json"
fn Constants() { return http.status }
fn main():null! {
    statuses=Constants()
    copied=statuses.clone()
    statuses.created=202
    if(statuses.created!=202 || copied.created!=201 || http.status.created!=201) {
        panic("status mutation reached a clone or the module table")
    }
    statuses=http.status
    statuses["created"]=203
    if(statuses.created!=203 || copied.created!=201 || http.status.created!=201) {
        panic("status reassignment or indexed mutation changed the wrong owner")
    }
    key="created"
    got=copied[key]?
    if(got.as_int()?!=201) { panic("status dynamic index") }
    encoded=json.encode(copied)?
    decoded=json.decode(encoded)?
    if(decoded["expectationFailed"]?.as_int()?!=417) { panic("status JSON owner") }
    read=fn() { return copied.created }
    if(read()!=201 || copied.created!=201) {panic("status capture changed ownership")}
    aliases=http.code
    aliases.CREATED=205
    aliases=http.code
    if(aliases.CREATED!=201 || http.code.CREATED!=201) {panic("legacy alias reassignment")}
    for code in [201,417,418,-1,1000] {
        text=http.statusText(code)
        if(code==201 && text!="Created") {panic("status text")}
        if(code==417 && text!="Expectation Failed") {panic("status text 417")}
        if((code==418 || code==-1 || code==1000) && text!="Unknown") {panic("unknown status")}
    }
    println("http-status-aliases-ok")
    return ok(null)
}
"#;
    run_source("http-status-aliases.ku", source).expect("interpreter alias/mutation contract");
    let mut code = generated(source);
    assert_eq!(code.matches("typedef struct KuString {").count(), 1);
    code = code.replacen(
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
        1,
    );
    assert_eq!(code.matches("int main(void) {").count(), 1);
    code = code.replacen("int main(void) {", "static int original_main(void) {", 1);
    code.push_str("\nint main(void) { int status=original_main(); if(status)return status; if(ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow) {fprintf(stderr,\"HTTP constants retained allocations\\n\");return 91;} return 0;}\n");
    if let Some(output) = execute("http-status-aliases", code) {
        assert_eq!(output, "http-status-aliases-ok\n");
    }
}

#[test]
fn native_http_constants_preserve_lexical_shadowing_and_reject_unknown_members() {
    let source = r#"
import http from "std.http"
struct Codes { created:int }
struct Local { status:Codes }
fn Read(http:Local):int { return http.status.created }
fn main() {
    if(Read(Local {status:Codes {created:299}})!=299) {panic("parameter shadow")}
    if(true) {
        http=Local {status:Codes {created:298}}
        if(http.status.created!=298) {panic("block shadow")}
    }
    if(http.status.created!=201) {panic("expired local shadow")}
    println("http-status-shadow-ok")
}
"#;
    run_source("http-status-shadow.ku", source).expect("interpreter module shadowing");
    if let Some(output) = execute("http-status-shadow", generated(source)) {
        assert_eq!(output, "http-status-shadow-ok\n");
    }
    let invalid_source =
        "import http from \"std.http\"\nfn main(){println(http.status.notAStatus)}";
    assert!(check_source("http-status-unknown.ku", invalid_source).is_err());
    let invalid = parse(invalid_source);
    assert!(
        ir::lower_program(&invalid).is_err(),
        "unchecked IR entry must not fabricate an unknown constant"
    );
    let module_source = "import http from \"std.http\"\nfn main(){ module_alias=http }";
    assert!(check_source("http-module-value.ku", module_source).is_err());
    let module_value = parse(module_source);
    assert!(
        ir::lower_program(&module_value).is_err(),
        "do not fabricate a partial module object"
    );
}

fn address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn request(address: SocketAddr, request: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut socket = loop {
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(socket) => break socket,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "HTTP constants listener: {error}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    socket
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    socket.write_all(request.as_bytes()).unwrap();
    let mut output = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "HTTP constants response deadline");
        socket.set_read_timeout(Some(left)).unwrap();
        let mut bytes = [0u8; 1024];
        let n = socket.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        output.extend_from_slice(&bytes[..n]);
        assert!(output.len() <= 16 * 1024, "HTTP constants response bound");
    }
    String::from_utf8(output).unwrap()
}

#[test]
fn native_http_sync_and_async_handlers_use_status_constants_over_real_sockets() {
    let directory = TempDir::new("http-status-task");
    let listener = address();
    let admin = address();
    let source = r#"
import http from "std.http"
import time from "std.time"
async fn Created(req:HttpRequest):HttpResponse! {
    status=http.status.created
    time.sleep(1)?
    return ok(http.text(status,req.params.id.clone()))
}
fn Sync():HttpResponse {
    codes=http.code
    return http.text(codes.CREATED,"sync")
}
fn main():null! {
    app=http.server({max_active_requests:8,max_pending_requests:8,max_connections:16,
        handler_timeout_ms:1000,read_header_timeout_ms:1000,read_body_timeout_ms:1000,
        write_timeout_ms:1000})
    app.get("/created/{id}",Created)
    app.get("/sync",Sync)
    app.get("/empty",fn(){return http.empty(http.code.NO_CONTENT)})
    app.listen("__ADDRESS__")?
    return ok(null)
}
"#
    .replace("__ADDRESS__", &listener.to_string());
    let generated = emit_c(directory.path(), &source);
    let input = directory.path().join("status-server.c");
    fs::write(&input, generated).unwrap();
    let Some(binary) = compile_harness(directory.path(), &input, "status-server") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires actual HTTP C execution"
        );
        return;
    };
    let controller = thread::spawn(move || {
        let synchronous = request(
            listener,
            "GET /sync HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            synchronous.starts_with("HTTP/1.1 201 Created\r\n") && synchronous.ends_with("sync"),
            "{synchronous}"
        );
        let asynchronous = request(
            listener,
            "GET /created/owned HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            asynchronous.starts_with("HTTP/1.1 201 Created\r\n") && asynchronous.ends_with("owned"),
            "{asynchronous}"
        );
        let empty = request(
            listener,
            "GET /empty HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(empty.starts_with("HTTP/1.1 204 No Content\r\n"), "{empty}");
        let drain=request(admin,"POST /__ku/drain HTTP/1.1\r\nHost: localhost\r\nX-Ku-Control-Token: http-status-test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert!(drain.starts_with("HTTP/1.1 200 OK\r\n"), "{drain}");
    });
    let mut command = Command::new(binary);
    command
        .current_dir(directory.path())
        .env("KU_EVENT_LOOPS", "1")
        .env("KU_BLOCKING_THREADS", "2")
        .env("KU_SERVICE_CONTROL_TOKEN", "http-status-test")
        .env("KU_SERVICE_PROBE_ADDR", admin.to_string());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .expect("HTTP constant service exits and reclaims normally");
    controller
        .join()
        .expect("actual HTTP statuses and owned response body");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
