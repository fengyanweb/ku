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
            "KuHttpConnection* connections = (KuHttpConnection*)calloc((size_t)slot_count, sizeof(KuHttpConnection));",
            "KuHttpConnection* connections = owner->index == 0 ? NULL : (KuHttpConnection*)calloc((size_t)slot_count, sizeof(KuHttpConnection));",
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
