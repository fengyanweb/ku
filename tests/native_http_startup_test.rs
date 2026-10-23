//! Actual generated HTTP startup failures must retire external Task owners.
//! Faults are injected into temporary generated C, never through product knobs.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

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
            "if (!ku_http_queue_init(&queue, slot_count, &wake)) {",
            "if (owner->index == 0 || !ku_http_queue_init(&queue, slot_count, &wake)) {",
            "handler queue initialization failed",
        ),
    ];
    for (name, needle, replacement, error) in failures {
        assert_eq!(original.matches(needle).count(), 1, "fault site {name}");
        let directory = TempDir::new(&format!("http-startup-{name}"));
        let source = directory.path().join("startup.c");
        fs::write(&source, original.replacen(needle, replacement, 1)).unwrap();
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
  uint32_t status=ku_http_task_start_0(runtime,0u,&request,&task);
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
  status=ku_http_task_start_0(runtime,0u,&request,&task);
  CHECK(status==KU_TASK_DRIVER_LIMIT && !task && !runtime->active_handles);
  ku_http_task_driver_budget(runtime,budget);
  CHECK(budget[1]==1024u && budget[7]==1u && budget[8]==1u);
  for(size_t i=0;i<1024u;i++)CHECK(ku_task_driver_rollback(&tickets[i])==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.fixed_bytes+runtime->driver.reserved_bytes==baseline);
  CHECK(ku_http_task_runtime_close(runtime,deadline)==KU_TASK_DRIVER_OK);
  uint64_t next=UINT64_MAX;CHECK(ku_http_task_pump(runtime,0u,64u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(ku_http_task_runtime_free(runtime,deadline)==KU_TASK_DRIVER_OK);
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
