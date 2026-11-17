//! Public native synchronous HTTP responses and the optional HTTP pack.
#[path = "support/native_allocation_harness.rs"]
mod allocation;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod harness;
use harness::{
    compile_harness, run_bounded, TempDir, BUILD_LIMITS, BUILD_TIMEOUT, RUN_LIMITS, RUN_TIMEOUT,
};
use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use std::{
    env, fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn checked(source: &str) -> ku::ast::Program {
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    for name in ["std:http", "std:json", "std:string"] {
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: name.into(),
                span: Default::default(),
            }),
        );
    }
    Checker::new().check(&ast).unwrap();
    ast
}
fn generated(source: &str) -> String {
    let ast = checked(source);
    let lowered = ir::lower_program(&ast).unwrap();
    ir::verify_borrow_contract(&lowered).unwrap();
    c::generate_c_source(&lowered).unwrap()
}
fn run_c(label: &str, source: &str) -> Option<String> {
    let dir = TempDir::new(label);
    let path = dir.path().join("test.c");
    fs::write(&path, source).unwrap();
    let exe = compile_harness(dir.path(), &path, "test")?;
    let out = run_bounded(&mut Command::new(exe), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).unwrap())
}
#[test]
fn native_http_public_response_headers_alias_clone_annotations_and_json() {
    let source = r#"
import {http,json,string} from "std"
fn Make(): HttpResponse { return http.text("hello 中文🙂") }
fn Read(&r:HttpResponse):int { if(r.headers["content-type"]!="text/plain; charset=utf-8") return -1 return r.status }
fn Keep(r:HttpResponse):HttpResponse { return r }
fn main():null! {
 r=Make()
 if (Read(r)!=200 || r.headers["content-type"]!="text/plain; charset=utf-8") fail "annotation or direct header"
 headers=r.headers
 if (headers["content-type"]!="text/plain; charset=utf-8" || headers["content-type"]?!="text/plain; charset=utf-8") fail "alias header"
 caught=false
 try { absent=headers["missing"]? } catch(e) { caught=e.code=="missing_key" }
 if (!caught) fail "optional missing header"
 kept=Keep(r.clone())
 r.headers["x-extra"]="yes"
 if (r.headers["x-extra"]!="yes" || kept.headers.get_or("x-extra","missing")!="missing") fail "clone aliases"
 r.headers={"only":"new"}
 if (r.headers.get_or("content-type","absent")!="absent" || r.headers["only"]!="new") fail "authoritative replacement"
 encoded=json.encode(kept)?
 if (!string.contains(encoded,"\"headers\"") || string.contains(encoded,"content_type") || string.contains(encoded,"location")) fail "public JSON"
 println(kept)
 println("headers-ok")
 return ok(null)
}
"#;
    let c = generated(source);
    assert!(!c.contains("#define KU_FEATURE_NATIVE_HTTP_CLIENT 1"));
    if let Some(output) = run_c("http-response-headers", &c) {
        assert_eq!(output, "[object]\nheaders-ok\n");
    }
}
#[test]
fn native_task_outbound_http_is_rejected_directly_and_through_sync_wrappers() {
    for (label,functions,operation) in [
        ("direct","","r=http.get(\"http://127.0.0.1:1/\")?"),
        ("one-hop","fn Fetch():HttpResponse! {return http.get(\"http://127.0.0.1:1/\")}","r=Fetch()?"),
        ("two-hop","fn Fetch():HttpResponse! {return http.get(\"http://127.0.0.1:1/\")} fn Next():HttpResponse! {return Fetch()}","r=Next()?")
    ] {
        let text=format!("import http from \"std.http\"\n{functions}\nasync fn main():null! {{{operation} return ok(null)}}");
        let ast=checked(&text);
        assert!(ir::task_lower::lower_program(&ast).is_err(),"{label} could block a native Task");
    }
}
#[test]
fn native_http_sync_client_has_immediate_deadline_guard() {
    let c=generated("import http from \"std.http\" fn main():null! { r=http.get(\"http://127.0.0.1:1/\") println(\"must be guarded\") return ok(null) }");
    let root = c.rsplit_once("int main(void)").unwrap().1;
    assert!(
        !root.contains("ku_service_output_init("),
        "client-only program started an unrelated service output thread"
    );
    let at = c
        .rfind("= __ku_http_client_get(")
        .expect("actual outbound call");
    let continuation = &c[at..];
    let timeout = continuation
        .find("if (__ku_handler_timeout_poll())")
        .unwrap();
    let user = continuation.find("must be guarded").unwrap();
    assert!(
        timeout < user,
        "outbound call did not join existing structured timeout edge"
    );
    let clone=generated("import http from \"std.http\" fn main() {r=ok(http.text(\"owned\")) copy=r.clone() println(\"after-clone\")} ");
    let at = clone
        .rfind("= ku_result_clone_struct___ku_http_response(")
        .unwrap();
    assert!(
        clone[at..].find("__ku_sync_take()").unwrap() < clone[at..].find("after-clone").unwrap()
    );
}
#[test]
fn native_http_client_calendar_real_release_loopback() {
    if env::var_os("KU_NATIVE_HTTP_CLIENT_PACK").is_none()
        || env::var_os("KU_NATIVE_CALENDAR_PACK").is_none()
    {
        eprintln!("skip: matching HTTP/calendar packs required; final installed-SDK consumer is mandatory");
        return;
    }
    let dir = TempDir::new("http-client-calendar");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let source = include_str!("../examples/native_http_client.ku")
        .replace("http://127.0.0.1:8080", &address);
    fs::write(dir.path().join("main.ku"), source).unwrap();
    let cli = env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let program = dir.path().join(if cfg!(windows) {
        "program.exe"
    } else {
        "program"
    });
    let built = run_bounded(
        Command::new(cli)
            .current_dir(dir.path())
            .args(["build", "--release", "--verbose", "main.ku", "-o"])
            .arg(&program),
        BUILD_TIMEOUT,
        BUILD_LIMITS,
    )
    .unwrap();
    assert!(
        built.status.success(),
        "{}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let stop = Instant::now() + Duration::from_secs(10);
    let peer = thread::spawn(move || {
        for expected in ["GET /hello ", "POST /post ", "GET /missing "] {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < stop =>
                    {
                        thread::sleep(Duration::from_millis(1))
                    }
                    other => panic!("bounded HTTP peer: {other:?}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            while !bytes.windows(4).any(|x| x == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0 && bytes.len() + n <= 16384);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let split = bytes.windows(4).position(|x| x == b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
            assert!(head.starts_with(expected), "{head}");
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length <= 4096);
            let mut body = bytes[split + 4..].to_vec();
            while body.len() < length {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                body.extend_from_slice(&chunk[..n]);
            }
            let (status, payload) = if expected.starts_with("POST") {
                (201, body)
            } else if expected.contains("hello") {
                (200, "hello 中文🙂".as_bytes().to_vec())
            } else {
                (404, b"missing".to_vec())
            };
            stream.write_all(format!("HTTP/1.1 {status} Result\r\nX-Ku: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",payload.len()).as_bytes()).unwrap();
            stream.write_all(&payload).unwrap();
        }
    });
    let result = run_bounded(&mut Command::new(program), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    let peer_result = peer.join();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    peer_result.unwrap();
    assert_eq!(result.stdout, b"[object]\nnative-http-client-ok\n");
    assert!(result.stderr.is_empty());
}

#[test]
fn native_task_http_headers_guard_ignores_inactive_results_and_rejects_owned_map() {
    use ir::task::*;
    let sync = ir::lower_program(&checked(
        "fn Anchor(r:HttpResponse):HttpResponse {return r} fn main() {} ",
    ))
    .unwrap();
    let response = ir::IrType::Named("__ku_http_response".into());
    let output = ir::IrType::Result(Box::new(response.clone()));
    let mut tasks = TaskProgram { functions: vec![] };
    for (id, input) in [response.clone(), output.clone()].into_iter().enumerate() {
        let values = if id == 0 {
            vec![input.clone(), output.clone()]
        } else {
            vec![input.clone()]
        };
        tasks.functions.push(TaskFunction {
            id: TaskFunctionId(id),
            name: format!("Response{id}"),
            slots: values
                .into_iter()
                .map(|ty| TaskSlot {
                    ty: TaskSlotType::Value {
                        ty,
                        borrowed: false,
                    },
                })
                .collect(),
            parameters: vec![SlotId(0)],
            entry: StateId(0),
            result: output.clone(),
            states: vec![
                TaskState {
                    operations: vec![],
                    terminator: TaskTerminator::Suspend {
                        resume: StateId(1),
                        cleanup: StateId(2),
                    },
                },
                TaskState {
                    operations: if id == 0 {
                        vec![TaskOp::WrapOk {
                            dst: SlotId(1),
                            src: SlotId(0),
                        }]
                    } else {
                        vec![]
                    },
                    terminator: TaskTerminator::Complete {
                        value: SlotId(if id == 0 { 1 } else { 0 }),
                    },
                },
                TaskState {
                    operations: vec![TaskOp::DropIfInit { slot: SlotId(0) }],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        });
    }
    let c = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
    let c = c.replacen("int main(", "int ku_unused_main(", 1)
        + r#"
int main(void) {
  size_t bytes=99;
  KuStruct___ku_http_response input={0}; input.headers=(KuObject*)(uintptr_t)1;
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 81;
  input.headers=NULL;
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_OK || bytes)return 82;
  KuResult_struct___ku_http_response wrapped={0};wrapped.value.headers=(KuObject*)(uintptr_t)1;
  if(ku_task_1_inputs(NULL,NULL,0,1,&wrapped,&bytes)!=KU_TASK_DRIVER_OK || bytes)return 83;
  wrapped.ok=true;
  if(ku_task_1_inputs(NULL,NULL,0,1,&wrapped,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 84;
  puts("task-http-active-payload-ok");return 0;
}
"#;
    if let Some(out) = run_c("task-http-headers", &c) {
        assert_eq!(out, "task-http-active-payload-ok\n");
    }
}

#[test]
fn native_http_response_clone_and_public_value_oom_roll_back_only_new_owners() {
    let c = generated("fn Keep(r:HttpResponse):HttpResponse {return r} fn main() {} ");
    let hook = allocation::ALLOCATION_HOOK.replacen(
        "  ku_perf_calls++;",
        "  if(ku_perf_calls++ == ku_test_fail_at)return NULL;",
        1,
    );
    let c = c.replacen(
        "typedef struct KuString {",
        &format!("static size_t ku_test_fail_at=SIZE_MAX;\n{hook}\ntypedef struct KuString {{"),
        1,
    );
    let c = c.replacen("int main(", "int ku_unused_main(", 1)
        + r#"
int main(void) {
  KuStruct___ku_http_response source={.status=200};
  if(!ku_object_string_try_concat(ku_string_static((const uint8_t*)"owned",5),ku_string_static((const uint8_t*)" body",5),&source.body))return 61;
  source.content_type=ku_string_static((const uint8_t*)"text/plain",10);
  source.headers=__ku_http_response_headers_read(source);
  if(!source.headers)return 62;
  size_t original_bytes=ku_perf_live_bytes,original_allocations=ku_perf_live_allocations;
  for(size_t n=0;n<24;n++) {
    ku_test_fail_at=ku_perf_calls+n;
    KuStruct___ku_http_response cloned={0};
    bool success=__ku_http_response_try_clone(source,&cloned);
    ku_test_fail_at=SIZE_MAX;
    if(!success && (cloned.body.ptr || cloned.headers || cloned.status))return 63;
    ku_drop_struct___ku_http_response(&cloned);
    if(ku_perf_live_bytes!=original_bytes || ku_perf_live_allocations!=original_allocations)return 64;
    ku_test_fail_at=ku_perf_calls+n;
    KuValue value=__ku_http_response_public_value(source);
    KuSyncExitSignal signal=__ku_sync_take();
    ku_test_fail_at=SIZE_MAX;
    if(signal.kind!=KU_SYNC_EXIT_NONE && value.tag!=KU_NULL)return 65;
    ku_value_drop(&value);
    if(ku_perf_live_bytes!=original_bytes || ku_perf_live_allocations!=original_allocations)return 66;
    if(source.body.len!=10 || memcmp(source.body.ptr,"owned body",10))return 67;
  }
  ku_drop_struct___ku_http_response(&source);
  if(ku_perf_live_bytes || ku_perf_live_allocations || ku_perf_overflow)return 68;
  puts("http-response-oom-owners-ok");return 0;
}
"#;
    if let Some(out) = run_c("http-response-oom", &c) {
        assert_eq!(out, "http-response-oom-owners-ok\n");
    }
}

#[test]
fn native_http_client_timeout_runs_finally_and_preserves_cleanup_budget() {
    let c = generated(
        r#"
import http from "std.http"
fn Work():null! {
 try {response=http.get("http://loopback.invalid/") println("must-not-continue")}
 finally {println("cleanup")}
 return ok(null)
}
fn main() {}
"#,
    );
    let c = c.replacen("int main(", "int ku_unused_main(", 1)
        + r#"
static int test_mode,test_calls,test_releases;
uint32_t __ku_http_client_abi_version(void){return 1;}
uint32_t __ku_http_client_call_v1(const __ku_http_client_input_v1* input,__ku_http_client_output_v1* output) {
  test_calls++;
  if(!input->caller_budget_ms || (test_mode && input->caller_budget_ms>500u))return 2;
  output->http_status=200;
  output->body=(__ku_http_client_slice_v1){(const uint8_t*)"late",4};
  if(!test_mode)__ku_handler_deadline=__ku_handler_now_ms()-1u;
  return 0;
}
uint32_t __ku_http_client_release_v1(__ku_http_client_output_v1* output) {
  test_releases++;*output=(__ku_http_client_output_v1){.size=sizeof(*output)};return 0;
}
int main(void) {
  __ku_handler_deadline=__ku_handler_now_ms()+10000u;
  KuResult_null result=Work();ku_result_drop_null(&result);
  if(!__ku_handler_timed_out || test_calls!=1 || test_releases!=1 || __ku_handler_unwind_depth)return 71;
  test_mode=1;__ku_handler_unwind_depth=1;__ku_handler_timed_out=1;
  __ku_handler_deadline=__ku_handler_now_ms()+10000u;
  __ku_handler_cleanup_deadline=__ku_handler_now_ms()+500u;
  KuResult_struct___ku_http_response response=__ku_http_client_get(ku_string_static((const uint8_t*)"http://loopback.invalid/",24));
  if(!response.ok || test_calls!=2 || test_releases!=2)return 72;
  ku_result_drop_struct___ku_http_response(&response);
  __ku_handler_unwind_depth=0;__ku_handler_timed_out=0;__ku_handler_deadline=0;__ku_handler_cleanup_deadline=0;
  puts("http-client-deadline-cleanup-ok");return 0;
}
"#;
    if let Some(out) = run_c("http-client-deadline", &c) {
        assert_eq!(out, "cleanup\nhttp-client-deadline-cleanup-ok\n");
    }
}

#[test]
fn native_http_request_preserves_header_error_priority_and_input_ownership() {
    let c=generated("import http from \"std.http\" fn Work():HttpResponse! {return http.request({url:\"http://loopback.invalid/\"})} fn main() {} ");
    let c = c.replacen("int main(", "int ku_unused_main(", 1)
        + r#"
static int test_calls,test_releases;
uint32_t __ku_http_client_abi_version(void){return 1;}
uint32_t __ku_http_client_call_v1(const __ku_http_client_input_v1* i,__ku_http_client_output_v1* o){
 test_calls++;if(i->timeout_ms!=100u || i->header_count!=1u)return 2;
 o->http_status=200;return 0;
}
uint32_t __ku_http_client_release_v1(__ku_http_client_output_v1* o){test_releases++;*o=(__ku_http_client_output_v1){.size=sizeof(*o)};return 0;}
static void field(KuObject* o,const char* key,KuValue value) {
 if(!ku_object_try_set_copy_key(o,ku_string_static((const uint8_t*)key,strlen(key)),&value))abort();
}
int main(void) {
 KuObject* headers=ku_object_new(0);KuObject* config=ku_object_new(0);
 field(headers,"x-ku",ku_v_str(ku_string_static((const uint8_t*)"bad\r",4)));
 field(config,"headers",ku_v_object(headers));field(config,"url",ku_v_str(ku_string_static((const uint8_t*)"http://loopback.invalid/",24)));
 field(config,"timeout_ms",ku_v_int(0));
 KuResult_struct___ku_http_response result=__ku_http_client_request(config);
 KuSyncExitSignal signal=__ku_sync_take();
 if(result.ok || test_calls || signal.kind!=KU_SYNC_EXIT_HTTP_JSON_FATAL || !strstr((const char*)signal.static_message,"header contains"))return 51;
 ku_result_drop_struct___ku_http_response(&result);
 field(headers,"x-ku",ku_v_str(ku_string_static((const uint8_t*)"good",4)));
 result=__ku_http_client_request(config);signal=__ku_sync_take();
 if(result.ok || test_calls || signal.kind!=KU_SYNC_EXIT_HTTP_JSON_FATAL || !strstr((const char*)signal.static_message,"positive int"))return 52;
 ku_result_drop_struct___ku_http_response(&result);
 field(config,"timeout_ms",ku_v_int(100));
 result=__ku_http_client_request(config);signal=__ku_sync_take();
 if(!result.ok || result.value.status!=200 || test_calls!=1 || test_releases!=1 || signal.kind)return 53;
 if(ku_object_get(headers,ku_string_static((const uint8_t*)"x-ku",4))->as.s.len!=4)return 54;
 ku_result_drop_struct___ku_http_response(&result);ku_object_drop(config);
 puts("http-request-priority-ownership-ok");return 0;
}
"#;
    if let Some(out) = run_c("http-request-priority", &c) {
        assert_eq!(out, "http-request-priority-ownership-ok\n");
    }
}
