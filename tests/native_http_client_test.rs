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
#[test]
fn native_http_dynamic_and_view_handlers_use_their_actual_checked_abi() {
    let c = generated(
        r#"
fn Dynamic() { box={value:{status:200,headers:{},body:"raw"}}; return box["value"] }
fn Reply(&req:HttpRequest):HttpResponse { return http.text(req.path.clone()) }
fn main() { app=http.server(); app.get("/dynamic",Dynamic); app.get("/",Reply) }
"#,
    );
    assert!(c.contains("ku_http_server_add_view_route("));
    assert!(c.contains("(KuHttpViewInvoke){.response="));
    assert!(c.contains("__ku_http_response_from_value_result("));
    assert!(!c.contains("View callbacks are not implemented"));
}

fn run_c(label: &str, source: &str) -> Option<String> {
    let dir = TempDir::new(label);
    let path = dir.path().join("test.c");
    fs::write(&path, source).unwrap();
    let exe = compile_harness(dir.path(), &path, "test")?;
    let out = run_bounded(&mut Command::new(exe), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        out.status.success(),
        "status={} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).unwrap().replace("\r\n", "\n"))
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
fn native_task_outbound_http_uses_real_net_wait_and_sync_wrappers_remain_rejected() {
    let source="import http from \"std.http\" async fn main():null! {r=http.get(\"http://127.0.0.1:1/\")? return ok(null)}";
    let mut ast = checked(source);
    // The shared synchronous helper adds std:string even when unused; it is
    // outside the documented Task module subset and must not mask this check.
    ast.items
        .retain(|item| !matches!(item,ku::ast::Item::Module(m)if m.name=="std:string"));
    let native = ir::task_lower::lower_program(&ast)
        .unwrap_or_else(|e| panic!("direct HTTP lowering failed: {e}"));
    assert!(native
        .tasks
        .functions
        .iter()
        .any(|f| f.states.iter().any(|s| matches!(
            s.terminator,
            ir::task::TaskTerminator::BlockingNet {
                operation: ir::task::TaskNetOperation::HttpGet,
                ..
            }
        ))));
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    assert!(generated.contains("#define KU_TASK_OUTBOUND_HTTP 1"));
    assert!(generated.contains("ku_task_host_http("));
    assert!(generated.contains("__ku_http_client_async_step_v1"));
    let mut configured = checked(
        r#"import http from "std.http" async fn main():null! {r=http.request({url:"http://127.0.0.1:1/",method:null,body:null,timeout_ms:null,max_body_bytes:null,headers:{"x-ok":"yes"}})? return ok(null)}"#,
    );
    configured
        .items
        .retain(|item| !matches!(item,ku::ast::Item::Module(m)if m.name=="std:string"));
    let configured = ir::task_lower::lower_program(&configured)
        .expect("literal null HTTP defaults must remain legal");
    let inputs = configured
        .tasks
        .functions
        .iter()
        .flat_map(|f| &f.states)
        .find_map(|state| match &state.terminator {
            ir::task::TaskTerminator::BlockingNet {
                operation: ir::task::TaskNetOperation::HttpRequest,
                inputs,
                ..
            } => Some(inputs),
            _ => None,
        })
        .unwrap();
    assert!(inputs[1..5].iter().all(Option::is_none));
    assert!(inputs[5].is_some() && inputs[6].is_some());
    let code = c::generate_native_task_c_source(&configured, &Default::default()).unwrap();
    let fatal = code
        .split("if (net_status==KU_TASK_HTTP_CONFIG_ERROR)")
        .nth(1)
        .unwrap();
    assert!(fatal
        .split("if (net_status==KU_TASK_CONTROL_PENDING)")
        .next()
        .unwrap()
        .contains("KU_TASK_EXIT_RUNTIME_FAILURE"));

    let mut null_headers = checked(
        r#"import http from "std.http" async fn main():null! {r=http.request({url:"http://127.0.0.1:1/",timeout_ms:0,headers:null})? return ok(null)}"#,
    );
    null_headers
        .items
        .retain(|item| !matches!(item,ku::ast::Item::Module(m)if m.name=="std:string"));
    let null_headers = ir::task_lower::lower_program(&null_headers)
        .expect("invalid null headers must retain checked fatal exit");
    assert!(null_headers.tasks.functions.iter().flat_map(|f|&f.states).any(|state|matches!(&state.terminator,ir::task::TaskTerminator::BlockingNet{operation:ir::task::TaskNetOperation::HttpRequest,inputs,..}if inputs[8].is_some())));
    for functions in ["fn Fetch():HttpResponse! {return http.get(\"http://127.0.0.1:1/\")}","fn Fetch():HttpResponse! {return http.get(\"http://127.0.0.1:1/\")} fn Next():HttpResponse! {return Fetch()}"] {
        let operation=if functions.contains("fn Next"){"Next"}else{"Fetch"};
        let text=format!("import http from \"std.http\"\n{functions}\nasync fn main():null! {{r={operation}()? return ok(null)}}");
        let mut ast=checked(&text);ast.items.retain(|item|!matches!(item,ku::ast::Item::Module(m)if m.name=="std:string"));
        assert!(ir::task_lower::lower_program(&ast).is_err(),"ordinary sync wrapper could block a native Task");
    }
}

#[test]
fn native_task_http_value_and_client_only_c11_o2_compile_without_server() {
    let programs = [
        (
            "value",
            r#"import http from "std.http" async fn Echo(r:HttpResponse):HttpResponse! {return ok(r)} async fn main():null! {t=Echo(http.text("basic-http")) r=await t? println(r.body.clone()) return ok(null)}"#,
        ),
        (
            "get",
            r#"import http from "std.http" async fn main():null! {r=http.get("http://127.0.0.1:1/")? return ok(null)}"#,
        ),
        (
            "post",
            r#"import http from "std.http" async fn main():null! {r=http.post("http://127.0.0.1:1/","body")? return ok(null)}"#,
        ),
        (
            "request",
            r#"import http from "std.http" async fn main():null! {try {r=http.request({url:"http://127.0.0.1:1/",headers:{"bad":"line\r\n"},timeout_ms:0})?} catch(e){println("must-not-catch-fatal")} return ok(null)}"#,
        ),
    ];
    for (label, source) in programs {
        let mut ast = checked(source);
        ast.items
            .retain(|item| !matches!(item,ku::ast::Item::Module(m)if m.name=="std:string"));
        let native = ir::task_lower::lower_program(&ast).unwrap_or_else(|e| panic!("{label}: {e}"));
        let code = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
        assert!(
            !code.contains("#define KU_NATIVE_RUNTIME_HTTP_SOCKET"),
            "{label} emitted a server"
        );
        assert!(
            !code.contains("#define KU_SERVICE_OUTPUT_SUPPORTED"),
            "{label} emitted a service-output worker"
        );
        if label == "value" {
            if let Some(stdout) = run_c("task-http-values-no-server", &code) {
                assert_eq!(stdout, "basic-http\n");
            }
            continue;
        }
        // Compile the real C11/O2 translation unit. Optional archive linking
        // and execution are covered by the installed SDK consumer, separately.
        let dir = TempDir::new("task-http-client-only-o2");
        let input = dir.path().join("main.c");
        let output = dir.path().join("main.o");
        fs::write(&input, &code).unwrap();
        let mut candidates = Vec::new();
        if let Ok(spec) = env::var("KU_CC") {
            if PathBuf::from(&spec).is_file() {
                candidates.push((PathBuf::from(spec), Vec::new()));
            } else {
                let mut words = spec.split_whitespace();
                if let Some(cc) = words.next() {
                    candidates.push((
                        PathBuf::from(cc),
                        words.map(str::to_owned).collect::<Vec<_>>(),
                    ));
                }
            }
        }
        candidates.extend([
            (PathBuf::from("clang"), Vec::new()),
            (PathBuf::from("gcc"), Vec::new()),
            (PathBuf::from("cc"), Vec::new()),
        ]);
        let mut compiled = false;
        for (cc, prefix) in candidates {
            let mut command = Command::new(cc);
            command
                .args(prefix)
                .args(["-std=c11", "-O2", "-c"])
                .arg(&input)
                .arg("-o")
                .arg(&output);
            match run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS) {
                Ok(done) => {
                    assert!(
                        done.status.success(),
                        "{label}: {}",
                        String::from_utf8_lossy(&done.stderr)
                    );
                    compiled = true;
                    break;
                }
                Err(e) if e.kind() == harness::bounded_process::FailureKind::Spawn => continue,
                Err(e) => panic!("{label} C compilation failed: {e}"),
            }
        }
        if env::var_os("CI").is_some() {
            assert!(compiled, "C compiler required in CI");
        }
    }
}

#[test]
fn native_http_sync_client_has_immediate_deadline_guard() {
    let c=generated("import http from \"std.http\" fn main():null! { r=http.get(\"http://127.0.0.1:1/\") println(\"must be guarded\") return ok(null) }");
    assert!(!c.contains("#define KU_NATIVE_RUNTIME_HTTP_SOCKET"));
    assert!(!c.contains("#define KU_SERVICE_OUTPUT_SUPPORTED"));
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
    assert_eq!(
        String::from_utf8(result.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        "[object]\nnative-http-client-ok\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn native_task_http_headers_guard_validates_owners_aliases_and_active_results() {
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
    let request = ir::IrType::Named("__ku_http_request".into());
    tasks.functions.push(TaskFunction {
        id: TaskFunctionId(2),
        name: "ResponseAndRequest".into(),
        slots: vec![response.clone(), request, output.clone()]
            .into_iter()
            .map(|ty| TaskSlot {
                ty: TaskSlotType::Value {
                    ty,
                    borrowed: false,
                },
            })
            .collect(),
        parameters: vec![SlotId(0), SlotId(1)],
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
                operations: vec![
                    TaskOp::DropIfInit { slot: SlotId(1) },
                    TaskOp::WrapOk {
                        dst: SlotId(2),
                        src: SlotId(0),
                    },
                ],
                terminator: TaskTerminator::Complete { value: SlotId(2) },
            },
            TaskState {
                operations: vec![
                    TaskOp::DropIfInit { slot: SlotId(0) },
                    TaskOp::DropIfInit { slot: SlotId(1) },
                ],
                terminator: TaskTerminator::Terminate,
            },
        ],
    });
    let c = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
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
    // This setter boundary test stubs only driver reservation, not map
    // allocation/mutation. Full adapter admission and alias checks above stay
    // real; the stub also proves rejected growth never requests a reservation.
    let marker = "/* Source Task HTTP maps remain";
    assert!(c.contains(marker));
    let c = c.replacen(marker, &format!(r#"
static size_t test_header_growth_calls;
static int test_header_reject_reservation;
static uint32_t test_header_reconcile(void* raw) {{ return raw ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_INVALID_ARGUMENT; }}
static uint32_t test_header_grow(const KuTaskDriverTicketV1* ticket,size_t bytes) {{
  (void)ticket;(void)bytes;test_header_growth_calls++;return test_header_reject_reservation ? KU_TASK_DRIVER_LIMIT : KU_TASK_DRIVER_OK;
}}
{marker}"#), 1);
    let reservation =
        "return status==KU_TASK_DRIVER_OK ? ku_task_driver_grow_charge(host->ticket,bytes):status;";
    assert_eq!(c.matches(reservation).count(), 1);
    let c = c.replace(
        reservation,
        "return status==KU_TASK_DRIVER_OK ? test_header_grow(host->ticket,bytes):status;",
    );
    let c = c.replacen("int main(", "int ku_unused_main(", 1)
        + r#"
int main(void) {
  size_t bytes=99;
  KuStruct___ku_http_response input={0};
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_OK || bytes)return 81;
  input.headers=ku_object_new(8);
  KuString key={0},value={0};
  if(!ku_object_string_try_concat(ku_string_static((const uint8_t*)"x-ku",4),(KuString){0},&key)
      || !ku_object_string_try_concat(ku_string_static((const uint8_t*)"owned",5),(KuString){0},&value))return 93;
  ku_object_set(input.headers,key,ku_v_str(value));
  size_t expected=sizeof(KuObject)+input.headers->cap*sizeof(KuEntry)+key.capacity+value.capacity;
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_OK || bytes!=expected)return 82;
  KuTaskAdapterBufferV1 forbidden={input.headers,sizeof(*input.headers),0};
  if(ku_task_0_inputs(NULL,&forbidden,1,1,&input,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 83;
  forbidden=(KuTaskAdapterBufferV1){input.headers->entries,input.headers->cap*sizeof(KuEntry),0};
  if(ku_task_0_inputs(NULL,&forbidden,1,1,&input,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 84;
  size_t original_capacity=input.headers->cap;input.headers->cap=3;
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 85;
  input.headers->cap=original_capacity;
  input.body=key;
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 86;
  input.body=value;
  if(ku_task_0_inputs(NULL,NULL,0,1,&input,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 87;
  input.body=(KuString){0};
  KuStruct___ku_http_request request={0};request.headers=input.headers;
  if(ku_task_2_inputs(NULL,NULL,0,1,&input,&request,&bytes)!=KU_TASK_DRIVER_INVALID_ARGUMENT)return 88;
  request.headers=NULL;
  if(ku_task_2_inputs(NULL,NULL,0,1,&input,&request,&bytes)!=KU_TASK_DRIVER_OK || bytes!=expected)return 89;
  KuResult_struct___ku_http_response wrapped={0};wrapped.value.headers=(KuObject*)(uintptr_t)1;
  if(ku_task_1_inputs(NULL,NULL,0,1,&wrapped,&bytes)!=KU_TASK_DRIVER_OK || bytes)return 90;
  wrapped.ok=true;wrapped.value.headers=input.headers;
  if(ku_task_1_inputs(NULL,NULL,0,1,&wrapped,&bytes)!=KU_TASK_DRIVER_OK || bytes!=expected)return 91;
  KuTaskAdapterOutcomeV1 outcome={0};outcome.value.http_response.value.headers=input.headers;
  if(ku_task_outcome_empty(&outcome,8u))return 92;
  ku_drop_struct___ku_http_response(&input);
  KuStruct___ku_http_response boundary={0};boundary.headers=ku_object_new(65536u);
  for(size_t i=0;i<49151u;i++) {
    char text[32];int n=snprintf(text,sizeof(text),"key%zu",i);
    if(n<=0 || (size_t)n>=sizeof(text))return 93;
    KuString owned_key={0};
    if(!ku_object_string_try_concat(ku_string_static((const uint8_t*)text,(size_t)n),(KuString){0},&owned_key))return 93;
    ku_object_set(boundary.headers,owned_key,ku_v_str(ku_string_static((const uint8_t*)"old",3)));
  }
  if(boundary.headers->cap!=65536u || boundary.headers->len!=49151u)return 94;
  KuTaskDriverTicketV1 ticket={0};KuTaskControlV1 control={0};
  KuTaskAdapterHostV1 host={0};host.ticket=&ticket;host.control=&control;
  host.allocation_host_bytes=sizeof(host);host.allocation_host_abi=KU_TASK_ALLOCATION_HOST_ABI_VERSION;
  host.allocation_context=&test_header_growth_calls;host.reconcile_charge=test_header_reconcile;
  KuString new_key=ku_string_static((const uint8_t*)"x-next",6),new_value=ku_string_static((const uint8_t*)"new",3);
  if(ku_task_host_http_header_set(&host,&boundary,new_key,new_value)!=KU_TASK_DRIVER_LIMIT
      || test_header_growth_calls || boundary.headers->cap!=65536u || boundary.headers->len!=49151u
      || ku_object_get(boundary.headers,new_key))return 95;
  KuString existing=ku_string_static((const uint8_t*)"key0",4);
  if(ku_task_host_http_header_set(&host,&boundary,existing,new_value)!=KU_TASK_DRIVER_OK
      || test_header_growth_calls!=1u || boundary.headers->cap!=65536u || boundary.headers->len!=49151u)return 96;
  KuValue* updated=ku_object_get(boundary.headers,existing);
  if(!updated || updated->tag!=KU_STR || !ku_string_equal(updated->as.s,new_value))return 97;
  ku_drop_struct___ku_http_response(&boundary);
  if(ku_perf_live_bytes || ku_perf_live_allocations || ku_perf_overflow)return 98;
  /* Exercise each real allocation in three mutation shapes. Only the driver's
   * reserve/reconcile callbacks are deterministic test doubles: all map,
   * table, key, value and cached-default allocations/frees use the real code.
   * Eight fixed failure positions include every current allocation site;
   * position eight rejects the byte reservation before any allocation. */
  for(size_t mode=0;mode<3u;mode++) {
    size_t rejected_allocations=0,successful_updates=0;
    for(size_t point=0;point<9u;point++) {
      KuStruct___ku_http_response original={.status=200};
      KuString caller_key={0},caller_value={0};
      if(!ku_object_string_try_concat(ku_string_static((const uint8_t*)"owned-body",10),(KuString){0},&original.body)
          || !ku_object_string_try_concat(ku_string_static((const uint8_t*)"application/test",16),(KuString){0},&original.content_type)
          || !ku_object_string_try_concat(ku_string_static((const uint8_t*)"/owned-next",11),(KuString){0},&original.location)
          || !ku_object_string_try_concat(ku_string_static((const uint8_t*)"target",6),(KuString){0},&caller_key)
          || !ku_object_string_try_concat(ku_string_static((const uint8_t*)"after-owned",11),(KuString){0},&caller_value))return 101;
      if(mode) {
        original.headers=ku_object_new(8u);
        size_t count=mode==1u?1u:5u;
        for(size_t i=0;i<count;i++) {
          char text[32];int n=mode==1u?snprintf(text,sizeof(text),"target"):snprintf(text,sizeof(text),"h%zu",i);
          KuString entry_key={0},entry_value={0};
          if(n<=0 || (size_t)n>=sizeof(text)
              || !ku_object_string_try_concat(ku_string_static((const uint8_t*)text,(size_t)n),(KuString){0},&entry_key)
              || !ku_object_string_try_concat(ku_string_static((const uint8_t*)"before-owned",12),(KuString){0},&entry_value))return 102;
          ku_object_set(original.headers,entry_key,ku_v_str(entry_value));
        }
      }
      KuObject* old_map=original.headers;KuEntry* old_table=old_map?old_map->entries:NULL;
      KuEntry old_entries[8];if(old_map)memcpy(old_entries,old_table,sizeof(old_entries));
      size_t old_len=old_map?old_map->len:0,old_cap=old_map?old_map->cap:0;
      uint8_t* old_body=original.body.ptr;uint8_t* old_type=original.content_type.ptr;uint8_t* old_location=original.location.ptr;
      uint8_t* old_caller_key=caller_key.ptr;uint8_t* old_caller_value=caller_value.ptr;
      size_t baseline_bytes=ku_perf_live_bytes,baseline_allocations=ku_perf_live_allocations;
      size_t calls_before=ku_perf_calls,growth_before=test_header_growth_calls;
      test_header_reject_reservation=point==8u;
      ku_test_fail_at=point==8u?SIZE_MAX:ku_perf_calls+point;
      uint32_t stored=ku_task_host_http_header_set(&host,&original,caller_key,caller_value);
      ku_test_fail_at=SIZE_MAX;test_header_reject_reservation=0;
      if(test_header_growth_calls!=growth_before+1u || original.status!=200
          || original.body.ptr!=old_body || original.body.len!=10u || memcmp(original.body.ptr,"owned-body",10)
          || original.content_type.ptr!=old_type || original.content_type.len!=16u || memcmp(original.content_type.ptr,"application/test",16)
          || original.location.ptr!=old_location || original.location.len!=11u || memcmp(original.location.ptr,"/owned-next",11)
          || caller_key.ptr!=old_caller_key || caller_key.len!=6u || memcmp(caller_key.ptr,"target",6)
          || caller_value.ptr!=old_caller_value || caller_value.len!=11u || memcmp(caller_value.ptr,"after-owned",11))return 103;
      if(point==8u || stored==KU_TASK_HOST_ALLOCATION_FAILED) {
        if(stored!=(point==8u?KU_TASK_DRIVER_LIMIT:KU_TASK_HOST_ALLOCATION_FAILED)
            || original.headers!=old_map || (old_map && (old_map->entries!=old_table || old_map->len!=old_len
              || old_map->cap!=old_cap || memcmp(old_table,old_entries,sizeof(old_entries))))
            || ku_perf_live_bytes!=baseline_bytes || ku_perf_live_allocations!=baseline_allocations)return 104;
        if(point==8u && ku_perf_calls!=calls_before)return 105;
        if(point!=8u)rejected_allocations++;
      } else {
        if(stored!=KU_TASK_DRIVER_OK || !original.headers)return 106;
        KuValue* changed=ku_object_get(original.headers,caller_key);
        if(!changed || changed->tag!=KU_STR || changed->as.s.ptr==caller_value.ptr
            || !ku_string_equal(changed->as.s,caller_value))return 107;
        if(mode==0u) {
          KuValue* content_type=ku_object_get(original.headers,ku_string_static((const uint8_t*)"content-type",12));
          KuValue* location=ku_object_get(original.headers,ku_string_static((const uint8_t*)"location",8));
          if(!content_type || content_type->tag!=KU_STR || !ku_string_equal(content_type->as.s,original.content_type)
              || content_type->as.s.ptr==original.content_type.ptr || !location || location->tag!=KU_STR
              || !ku_string_equal(location->as.s,original.location) || location->as.s.ptr==original.location.ptr)return 108;
        } else if(original.headers!=old_map || original.headers->len!=(mode==1u?1u:6u)
            || original.headers->cap!=(mode==1u?8u:16u))return 109;
        successful_updates++;
      }
      ku_drop_struct___ku_http_response(&original);ku_string_drop(&caller_key);ku_string_drop(&caller_value);
      if(ku_perf_live_bytes || ku_perf_live_allocations || ku_perf_overflow)return 110;
    }
    if(rejected_allocations<(mode==0u?6u:mode==1u?1u:3u) || !successful_updates)return 111;
  }
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

#[test]
fn native_http_structural_response_objects_convert_only_at_typed_boundaries() {
    let c = generated(
        r#"
fn Make():HttpResponse {return {status:201,headers:{"x-ku":"yes"},body:"literal"}}
fn MakeResult():HttpResponse! {return ok({status:202,headers:{"x-ku":"yes"},body:"result"})}
fn Keep(r:HttpResponse):HttpResponse {return r}
fn Read(&r:HttpResponse):int {return r.status}
fn main():null! {
 r=Make()
 if(r.status!=201 || r.headers["x-ku"]!="yes" || r.body!="literal") fail "literal return"
 dynamic={status:203,headers:{"x-ku":"yes"},body:"alias"}
 if(Read(dynamic)!=203 || dynamic["body"]?!="alias") fail "View conversion consumed object"
 moved=Keep(dynamic)
 if(moved.status!=203 || moved.body!="alias") fail "owned conversion"
 result=MakeResult()?
 if(result.status!=202 || result.body!="result") fail "Result payload"
 println("structural-response-ok")
 return ok(null)
}
"#,
    );
    assert!(c.contains("__ku_http_response_from_object_owned("));
    assert!(c.contains("__ku_http_response_from_object_view("));
    if let Some(out) = run_c("http-structural-response", &c) {
        assert_eq!(out, "structural-response-ok\n");
    }
}

#[test]
fn native_http_view_request_maps_remain_read_only_at_the_ir_boundary() {
    use ir::{IrCallKind, IrExpr, IrExprKind, IrInst, IrType, TempId};
    let ast = checked(
        r#"
fn Read(&req:HttpRequest):HttpResponse {return http.text(req.headers.host+req.params.id+req.query.q)}
fn main() {}
"#,
    );
    let valid = ir::lower_program(&ast).unwrap();
    let actual_ir = valid.to_string();
    assert!(actual_ir.contains("__ku_http_map_get"), "{actual_ir}");
    assert!(actual_ir.contains("borrowed req"), "{actual_ir}");
    ir::verify_borrow_contract(&valid).unwrap();
    let mut invalid = valid.clone();
    let reader = invalid
        .functions
        .iter_mut()
        .find(|function| function.name == "Read")
        .unwrap();
    let object = IrType::Named("__ku_object".into());
    let response = IrType::Named("__ku_http_response".into());
    reader.blocks[0].instructions.push(IrInst::Temp {
        id: TempId(10000),
        ty: response.clone(),
        value: IrExpr {
            ty: response,
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    ty: IrType::Function,
                    kind: IrExprKind::Local("__ku_http_response_from_object_owned".into()),
                }),
                args: vec![IrExpr {
                    ty: object,
                    kind: IrExprKind::Field {
                        target: Box::new(IrExpr {
                            ty: IrType::Named("__ku_http_request".into()),
                            kind: IrExprKind::BorrowedParam("req".into()),
                        }),
                        name: "headers".into(),
                    },
                }],
                kind: IrCallKind::Intrinsic("__ku_http_response_from_object_owned".into()),
            },
        },
    });
    let error = ir::verify_borrow_contract(&invalid)
        .expect_err("owned conversion must not consume a borrowed map");
    assert!(
        error
            .to_string()
            .contains("borrowed value passed to an owning intrinsic"),
        "{error}"
    );
}
