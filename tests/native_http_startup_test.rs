//! Actual generated HTTP startup failures must retire external Task owners.
//! Faults are injected into temporary generated C, never through product knobs.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn native_http_pg_capture_cannot_be_closed_or_moved_while_an_async_handler_is_registered() {
    let template = r#"
import {http, time} from "std"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({conninfo:"hostaddr=127.0.0.1",max_connections:1})?
    async fn database(req:HttpRequest):HttpResponse! {
        time.sleep(10)?
        result = client.query("SELECT $1::text",[req.query.key.clone()])?
        return ok(http.text(result.value(0,0)?))
    }
    __HELPER__
    app = http.server()
    app.get("/db",database)
    probe = client.query("SELECT 1",[])?
    if (probe.rows() != 1) { fail "ordinary pooled query failed" }
    __OTHER_OPERATION__
    app.listen("127.0.0.1:0")?
    return ok(null)
}
"#;
    ku::cli::check_source(
        "pg-capture-close-baseline.ku",
        &template
            .replace("__OTHER_OPERATION__", "")
            .replace("__HELPER__", ""),
    )
    .unwrap();
    for (label, operation) in [
        (
            "sibling-close",
            "app.get(\"/close\",fn(){client.close() return http.text(\"closed\")})",
        ),
        ("owner-close", "client.close()"),
        ("owner-move", "moved = client moved.close()"),
        (
            "indirect-close",
            "app.get(\"/close\",fn(){close_client() return http.text(\"closed\")})",
        ),
    ] {
        let helper = if label == "indirect-close" {
            "fn close_client(): null { client.close() return null }"
        } else {
            ""
        };
        let source = template
            .replace("__OTHER_OPERATION__", operation)
            .replace("__HELPER__", helper);
        let error = ku::cli::check_source("pg-capture-close.ku", &source).unwrap_err();
        assert!(
            error.message.contains("captur") || error.message.contains("shared"),
            "{label}: {error}"
        );
        eprintln!("{label}: {}", error.message);
    }
}

fn parsed(source: &str) -> ku::ast::Program {
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    for module in ["std:http", "std:time"] {
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: module.into(),
                span: Default::default(),
            }),
        );
    }
    ast
}

fn startup_source() -> String {
    let source = r#"
import { http, time } from "std"
async fn ready(): HttpResponse! { time.sleep(1)? return ok(http.text("yes")) }
fn main(): null! {
    app = http.server({max_connections: 16, max_active_requests: 8, max_pending_requests: 8})
    app.get("/", ready)
    app.listen("127.0.0.1:0")?
    return ok(null)
}

"#;
    let ast = parsed(source);
    Checker::new().check(&ast).unwrap();
    let tasks = ir::task_lower::lower_service_handlers(&ast).unwrap();
    // Same signature-only setup representation used by the CLI. The actual
    // body above is compiled into the Task program and cannot run in setup.
    let setup_source = source.replace(
        "async fn ready(): HttpResponse! { time.sleep(1)? return ok(http.text(\"yes\")) }",
        "fn ready(): HttpResponse! { fail \"signature only\" }",
    );
    let setup = parsed(&setup_source);
    let lowered = ir::lower_program(&setup).unwrap();
    c::generate_mixed_http_c_source(&lowered, &tasks, &Default::default()).unwrap()
}

#[test]
fn native_http_blocking_default_reserves_background_worker_within_thread_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    // Merely registering this handler enables the real blocking Task runtime.
    // It is never requested, so this regression requires no peer or database.
    let asynchronous = r#"async fn network(): HttpResponse! {
    client=net.client({host:"127.0.0.1",port:9,tls:false,connect_timeout_ms:100})?
    client.close()
    return ok(http.text("unused"))
}"#;
    let source = format!(
        r#"
import {{http, net}} from "std"
fn ready(): HttpResponse {{return http.text("ready")}}
{asynchronous}
fn main(): null! {{
    app=http.server({{max_connections:16,max_active_requests:8,max_pending_requests:8}})
    app.get("/ok",ready)
    app.get("/net",network)
    app.listen("{address}")?
    return ok(null)
}}
"#
    );
    let with_net = |source: &str| {
        let mut ast = parsed(source);
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: "std:net".into(),
                span: Default::default(),
            }),
        );
        ast
    };
    let ast = with_net(&source);
    Checker::new().check(&ast).unwrap();
    let tasks = ir::task_lower::lower_service_handlers(&ast).unwrap();
    let setup = with_net(&source.replace(
        asynchronous,
        "fn network(): HttpResponse! { fail \"signature only\" }",
    ));
    let lowered = ir::lower_program(&setup).unwrap();
    let generated = c::generate_mixed_http_c_source(&lowered, &tasks, &Default::default()).unwrap();
    let generated = generated.replacen(
        "int main(void) {",
        "static int fixture_original_main(void) {",
        1,
    );
    // The ordinary Task-root/default-count interface remains unchanged, even
    // though this HTTP group needs a separate background-worker reservation.
    let wrapper = r#"
int main(void) {
  if(ku_runtime_blocking_count()!=(size_t)strtoul(getenv("KU_FIXTURE_BASE_BLOCKING"),NULL,10))return 94;
  return fixture_original_main();
}
"#;
    let directory = TempDir::new("http-blocking-default");
    let file = directory.path().join("startup.c");
    fs::write(&file, generated + wrapper).unwrap();
    let Some(executable) = compile_harness(directory.path(), &file, "startup") else {
        return;
    };
    for (blocking, budget, success) in [
        (None, "16", true),
        (Some("3"), "16", true),
        (Some("2"), "16", false),
        (None, "6", false),
        (None, "7", true),
        (Some("17"), "64", true),
    ] {
        let mut command = Command::new(&executable);
        command
            .current_dir(directory.path())
            .env("KU_EVENT_LOOPS", "2")
            .env_remove("KU_BLOCKING_THREADS")
            .env("KU_THREAD_BUDGET", budget)
            .env("KU_FIXTURE_BASE_BLOCKING", blocking.unwrap_or("2"))
            .env("KU_HTTP_MAX_REQUESTS", "1")
            .env("KU_HTTP_DRAIN_TIMEOUT_MS", "1000")
            .env("KU_TASK_MAX_BYTES", "16777216")
            .env("KU_SERVICE_OUTPUT", "bounded");
        if let Some(blocking) = blocking {
            command.env("KU_BLOCKING_THREADS", blocking);
        }
        let (result, response) = std::thread::scope(|scope| {
            let request = success.then(|| {
                scope.spawn(|| {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let mut stream = loop {
                        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
                            Ok(stream) => break stream,
                            Err(_) if Instant::now() < deadline => {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => panic!("HTTP startup did not accept: {error}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .write_all(
                            b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    let mut response = Vec::new();
                    stream.take(4096).read_to_end(&mut response).unwrap();
                    response
                })
            });
            let result = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS).unwrap();
            let response = request.map(|request| request.join().unwrap());
            (result, response)
        });
        assert_eq!(
            result.status.code(),
            Some(if success { 0 } else { 1 }),
            "B={blocking:?}, budget={budget}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        if success {
            let response = response.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response.ends_with(b"\r\n\r\nready"));
            assert!(result.stdout.is_empty() && result.stderr.is_empty());
        } else {
            assert!(String::from_utf8_lossy(&result.stderr)
                .contains("blocking_threads must cover every loop"));
        }
    }
}

#[test]
fn native_http_early_loop_startup_failures_retire_on_the_original_thread() {
    let original = startup_source();
    let failures = [
        (
            "connections",
            "KuHttpConnection* connections=(KuHttpConnection*)calloc((size_t)slot_count,sizeof(KuHttpConnection));",
            "KuHttpConnection* connections=owner->index == 0 ? NULL : (KuHttpConnection*)calloc((size_t)slot_count,sizeof(KuHttpConnection));",
            "reactor allocation failed",
        ),
        (
            "wake",
            "if (!ku_http_wake_init(&wake)) {",
            "if (owner->index == 0 || !ku_http_wake_init(&wake)) {",
            "reactor wake initialization failed",
        ),
        (
            "queue",
            "if (!ku_http_queue_init(&queue, request_limit, &wake)) {",
            "if (owner->index == 0 || !ku_http_queue_init(&queue, request_limit, &wake)) {",
            "handler queue initialization failed",
        ),
    ];
    for (name, needle, replacement, error) in failures {
        assert_eq!(original.matches(needle).count(), 1, "fault site {name}");
        let directory = TempDir::new(&format!("http-startup-{name}"));
        let source = directory.path().join("startup.c");
        let injected = original.replacen(needle, replacement, 1).replacen(
            "int main(void) {",
            "static int fixture_original_main(void) {",
            1,
        );
        // Owner startup failure must never close zero-filled, unadmitted
        // connection entries. Descriptor zero may already belong to stdin.
        let input_descriptor = r#"
int main(void) {
#if !defined(_WIN32)
  int input_flags=fcntl(0,F_GETFD,0);
#endif
  int status=fixture_original_main();
#if !defined(_WIN32)
  if(input_flags>=0 && fcntl(0,F_GETFD,0)<0) return 94;
#endif
  return status;
}
"#;
        fs::write(&source, injected + input_descriptor).unwrap();
        let Some(executable) = compile_harness(directory.path(), &source, "startup") else {
            return;
        };
        for loops in [1, 2, 4] {
            let mut command = Command::new(&executable);
            command
                .current_dir(directory.path())
                .env("KU_EVENT_LOOPS", loops.to_string())
                .env("KU_BLOCKING_THREADS", "4")
                .env("KU_THREAD_BUDGET", "12")
                .env("KU_HTTP_DRAIN_TIMEOUT_MS", "250");
            let result = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
                .unwrap_or_else(|failure| panic!("{name}, N={loops}: {failure}"));
            assert_eq!(
                result.status.code(),
                Some(1),
                "{name}, N={loops} must return the listen error, not force process retirement: {}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                String::from_utf8_lossy(&result.stderr).contains(error),
                "{name}, N={loops}: expected actual startup error {error}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}

#[test]
fn native_http_root_admission_rejects_before_execution_and_reports_exact_limit() {
    let generated =
        startup_source().replacen("int main(void) {", "static int ku_unused_main(void) {", 1);
    let checks = r#"
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"HTTP root admission line %d: %s\n",__LINE__,#c); return 93; } } while(0)
static void fixture_wake(void* context,size_t owner) { (void)context;(void)owner; }
int main(void) {
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)ku_http_task_runtime_new(1u,1u,64u*1024u*1024u,NULL,fixture_wake,deadline);
  CHECK(runtime && runtime->driver.capacity==1024u);
  size_t baseline=runtime->driver.fixed_bytes+runtime->driver.reserved_bytes;
  CHECK(!ku_task_driver_lock(&runtime->driver));
  runtime->driver.byte_limit=baseline+sizeof(KuTaskInstance_0)-1u;
  ku_task_driver_unlock(&runtime->driver);
  KuStruct___ku_http_request request={0};void* task=NULL;
  uint32_t status=ku_http_task_start_0(runtime,0u,NULL,deadline,&request,&task);
  fprintf(stderr,"root_byte_admission_status=%u task_published=%d resident=%zu\n",status,task!=NULL,runtime->driver.resident);
  CHECK(status==KU_TASK_DRIVER_LIMIT && !task && !runtime->active_handles);
  uint64_t budget[9]={0};ku_http_task_driver_budget(runtime,budget);
  CHECK(!budget[1] && !budget[7] && budget[8]==1u);
  CHECK(runtime->driver.fixed_bytes+runtime->driver.reserved_bytes==baseline);
  CHECK(!ku_task_driver_lock(&runtime->driver));runtime->driver.byte_limit=64u*1024u*1024u;ku_task_driver_unlock(&runtime->driver);
  /* Actual BUILDING reservations occupy each slot, without executing or
   * publishing a child. A rejected root must not change any held receipt. */
  KuTaskDriverTicketV1 tickets[1024]={0};
  for(size_t i=0;i<1024u;i++)CHECK(ku_task_driver_reserve(&runtime->driver,1u,&tickets[i])==KU_TASK_DRIVER_OK);
  status=ku_http_task_start_0(runtime,0u,NULL,deadline,&request,&task);
  CHECK(status==KU_TASK_DRIVER_LIMIT && !task && !runtime->active_handles);
  ku_http_task_driver_budget(runtime,budget);
  CHECK(budget[1]==1024u && budget[7]==1u && budget[8]==1u);
  for(size_t i=0;i<1024u;i++)CHECK(ku_task_driver_rollback(&tickets[i])==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.fixed_bytes+runtime->driver.reserved_bytes==baseline);
  CHECK(ku_http_task_runtime_close(runtime,deadline)==KU_TASK_DRIVER_OK);
  uint64_t next=UINT64_MAX;CHECK(ku_http_task_pump(runtime,0u,64u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(ku_http_task_runtime_free(runtime,deadline)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  /* Exercise the actual admission/close functions with an owned fd zero,
     while every other calloc slot remains uninitialized and unowned. */
  int saved_input=dup(0),pair[2];CHECK(socketpair(AF_UNIX,SOCK_STREAM,0,pair)==0);
  CHECK(dup2(pair[0],0)==0);if(pair[0]!=0)CHECK(close(pair[0])==0);
  KuHttpConnection* connections=(KuHttpConnection*)calloc(16u,sizeof(*connections));
  int indices[16]={0};CHECK(connections);
  KuHttpActiveSet active={connections,indices,0,16,-1,0,0};
  KuHttpBufferBudget http_budget={0};http_budget.limit=4096u;CHECK(ku_http_mutex_init(&http_budget.lock));
  KuHttpServer server={0};server.buffer_budget=&http_budget;server.request_timeout_ms=1000;
  KuHttpPollConnection empty={0,0};CHECK(!ku_http_polled_connection(&active,&empty,0,0));
  ku_http_atomic_store(&ku_http_active_conns,1);
  ku_http_connection_start(&connections[0],&active,0,&server);
  CHECK(active.count==1 && active.high_water==1 && connections[0].socket_value==0);
  ku_http_connection_close(&connections[0]);
  CHECK(!active.count && !active.sockets && active.high_water==1 && !http_budget.current);
  CHECK(fcntl(0,F_GETFD,0)<0 && errno==EBADF);
  for(size_t i=1;i<16u;i++) {
    const unsigned char* bytes=(const unsigned char*)&connections[i];
    for(size_t j=0;j<sizeof(*connections);j++)CHECK(!bytes[j]);
  }
  CHECK(close(pair[1])==0);
  if(saved_input>=0){CHECK(dup2(saved_input,0)==0);CHECK(close(saved_input)==0);}
  ku_http_mutex_destroy(&http_budget.lock);free(connections);
#endif
  puts("http-root-admission-ok");return 0;
}
"#;
    let directory = TempDir::new("http-root-admission");
    let path = directory.path().join("admission.c");
    fs::write(&path, generated + checks).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "admission") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "http-root-admission-ok"
    );
}

#[test]
fn native_http_local_async_pg_capture_uses_the_existing_cell_and_typed_result() {
    let source = r#"
import {http, time} from "std"
import pg from "std.pg"
fn main(): null! {
    client = pg.client({conninfo: "hostaddr=127.0.0.1", max_connections: 1})?
    async fn database(req: HttpRequest): HttpResponse! {
        params = [req.query.key.clone()]
        result = client.query("SELECT id,label FROM items WHERE id=$1", params)?
        count = result.rows()
        columns = result.cols()
        number = result.value_int(0, 0)?
        label = result.value(0, 1)?
        missing = result.is_null(0, 1)?
        time.sleep(1)?
        if (count != 1 || columns != 2 || missing || params.len() != 1) { fail "invalid row" }
        return ok(http.text(str(number) + ":" + label))
    }
    app = http.server()
    app.get("/db", database)
    app.listen("127.0.0.1:0")?
    return ok(null)
}
"#;
    let mut ast = parsed(source);
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:pg".into(),
            span: Default::default(),
        }),
    );
    Checker::new().check(&ast).unwrap();
    let (setup, bindings) = ir::lower_mixed_http_setup(&ast).unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].captures.len(), 1);
    let tasks = ir::task_lower::lower_bound_service_handlers(&ast, &bindings).unwrap();
    let plan = ir::task::verify_and_plan(&tasks, Default::default()).unwrap();
    let handler = &tasks.functions[0];
    assert_eq!(handler.parameters.len(), 2);
    assert_eq!(
        handler
            .states
            .iter()
            .filter(|state| matches!(state.terminator, ir::task::TaskTerminator::PgQuery { .. }))
            .count(),
        1
    );
    assert_eq!(
        handler
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|operation| matches!(operation, ir::task::TaskOp::PgResult { .. }))
            .count(),
        5
    );
    let capture = handler.parameters[1];
    assert!(plan.functions[0].slots.contains(&capture));
    let generated = c::generate_mixed_http_c_source(&setup, &tasks, &Default::default()).unwrap();
    assert!(generated.contains("ku_task_host_pg_query("));
    assert!(generated.contains("ku_task_blocking_submit_pg("));
    assert!(generated.contains("KuTaskPgCapture capture_0="));
    assert!(generated.contains("handle->environment=environment"));
    assert!(generated.contains("ku_task_pg_capture_drop(&frame->s_"));
    assert!(!generated.contains("ku_clone_pg_client(captured"));
    // The captured pool cannot be moved into another logical owner.
    let mut invalid = tasks.clone();
    invalid.functions[0].states[handler.entry.0]
        .operations
        .insert(
            0,
            ir::task::TaskOp::Move {
                dst: ir::task::SlotId(handler.slots.len()),
                src: capture,
            },
        );
    invalid.functions[0]
        .slots
        .push(handler.slots[capture.0].clone());
    assert!(ir::task::verify_and_plan(&invalid, Default::default())
        .unwrap_err()
        .message
        .contains("capture is read-only"));
    // The ordinary source checker retains its existing captured-close rule.
    let invalid = source.replacen(
        "params = [req.query.key.clone()]",
        "client.close()\nparams = [req.query.key.clone()]",
        1,
    );
    let mut invalid = parsed(&invalid);
    invalid.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:pg".into(),
            span: Default::default(),
        }),
    );
    assert!(Checker::new().check(&invalid).is_err());
    // Compile and link the actual bridge when development libpq is available;
    // this performs no connection and never runs the generated setup function.
    let libdir = std::env::var_os("KU_PG_LIB")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let mut discovery = Command::new("pg_config");
            discovery.arg("--libdir");
            let found = run_bounded(&mut discovery, RUN_TIMEOUT, RUN_LIMITS).ok()?;
            found
                .status
                .success()
                .then(|| std::path::PathBuf::from(String::from_utf8(found.stdout).unwrap().trim()))
        });
    if let Some(libdir) = libdir {
        // Use the same explicit directory as the real CLI/CI. A pg_config
        // earlier on Windows PATH can describe a different installation.
        assert!(libdir.is_dir(), "libpq fixture directory must exist");
        let directory = TempDir::new("async-pg-bridge-link");
        let path = directory.path().join("async-pg.c");
        fs::write(&path, &generated).unwrap();
        assert!(native_harness::compile_harness_with_libpq(
            directory.path(),
            &path,
            "async-pg",
            &libdir
        )
        .is_some());
    }
}
