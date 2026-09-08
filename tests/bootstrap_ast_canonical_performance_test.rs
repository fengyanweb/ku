//! Actual bootstrap AstCanonical interpreter allocation-scaling gate.
//! Counts successful allocation/reallocation calls and requested bytes on the
//! executing thread, not live storage or leaks. Parsing/checking is excluded;
//! run includes linear fixture construction, function clones and golden check.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    ast::{Item, Program},
    checker::Checker,
    interpreter::Interpreter,
    lexer::Lexer,
    parser::Parser,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    enabled: bool,
    calls: usize,
    requested: usize,
    overflow: bool,
}
thread_local! {
    static COUNTS: Cell<Counts> = const { Cell::new(Counts {
        enabled: false, calls: 0, requested: 0, overflow: false,
    }) };
}
struct CountingAllocator;
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record(size: usize) {
    // A const Cell, no formatting, allocation or panicking arithmetic in this
    // allocator callback. TLS teardown is outside the explicitly armed region.
    let _ = COUNTS.try_with(|cell| {
        let mut counts = cell.get();
        if !counts.enabled {
            return;
        }
        match (
            counts.calls.checked_add(1),
            counts.requested.checked_add(size),
        ) {
            (Some(calls), Some(requested)) => {
                counts.calls = calls;
                counts.requested = requested;
            }
            _ => counts.overflow = true,
        }
        cell.set(counts);
    });
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record(layout.size());
        }
        pointer
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, size) };
        if !pointer.is_null() {
            record(size);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}
struct Measurement;
impl Measurement {
    fn begin() -> Self {
        COUNTS.with(|cell| {
            assert!(!cell.get().enabled, "nested allocation measurement");
            cell.set(Counts {
                enabled: true,
                ..Counts::default()
            });
        });
        Self
    }
    fn finish(self) -> Counts {
        let counts = COUNTS.with(Cell::get);
        drop(self);
        counts
    }
}
impl Drop for Measurement {
    fn drop(&mut self) {
        COUNTS.with(|cell| cell.set(Counts::default()));
    }
}
fn parse(source: &str) -> Program {
    Parser::new(Lexer::new(source).lex().expect("fixture lex"))
        .parse_program()
        .expect("fixture parse")
}
fn quote(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\r', "\\r")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}
fn prepared(node_count: usize) -> Program {
    assert!((2..=4096).contains(&node_count));
    // Canonical postorder star: N-1 leaves followed by their single root.
    // Edges 1..N-1 are unique; empty leaf slices precede the root's full slice.
    let mut nodes = String::new();
    let mut edges = String::new();
    let mut golden = format!("ROOT|{node_count}");
    for id in 1..=node_count {
        let root = id == node_count;
        let kind = if root { "Root" } else { "Leaf" };
        let text = if root { "" } else { "a|b\\c\r\n\t世" };
        let escaped = if root { "" } else { r"a\pb\\c\r\n\t世" };
        let children = if root { node_count - 1 } else { 0 };
        if id != 1 {
            nodes.push_str(",\n");
        }
        write!(nodes, "Node {{ kind: {}, text: {}, int_value: {id}, line: 1, column: 1, offset: 0, end_line: 1, end_column: 1, end_offset: 0, first_edge: 0, edge_count: {children} }}",
            quote(kind), quote(text)).unwrap();
        write!(
            golden,
            "\nNODE|{id}|{kind}|{escaped}|{id}|1:1@0..1:1@0|0|{children}"
        )
        .unwrap();
    }
    for index in 0..node_count - 1 {
        if index != 0 {
            edges.push(',');
        }
        write!(edges, "{}", index + 1).unwrap();
        write!(golden, "\nEDGE|{index}|{}", index + 1).unwrap();
    }
    let source = format!(
        "fn main() {{
        actual = AstCanonical(ParseOutput {{
            arena: Arena {{ nodes: [{nodes}], edges: [{edges}] }}, root: {node_count}
        }})
        if (actual != {}) {{ panic(\"AstCanonical exact golden mismatch\") }}
    }}",
        quote(&golden)
    );
    let mut program = parse(include_str!("../bootstrap/stage1/token.ku"));
    program
        .items
        .extend(parse(include_str!("../bootstrap/stage2/ast.ku")).items);
    program
        .items
        .retain(|item| !matches!(item, Item::Import(_)));
    program.items.extend(parse(&source).items);
    Checker::new()
        .check(&program)
        .expect("actual modules and fixture check");
    program
}
fn measure(program: Program) -> Counts {
    let mut interpreter = Interpreter::new();
    let started = Instant::now();
    let measurement = Measurement::begin();
    let result = interpreter.run(program);
    let counts = measurement.finish();
    result.expect("actual AstCanonical execution and exact golden");
    assert!(!counts.overflow, "allocation counter overflow");
    assert!(counts.calls > 0 && counts.requested > 0);
    eprintln!(
        "AstCanonical metric: {counts:?}, elapsed={:?}",
        started.elapsed()
    );
    counts
}
#[test]
fn bootstrap_ast_canonical_interpreter_allocation_scaling() {
    const CHILD: &str = "KU_AST_CANONICAL_ALLOCATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "bootstrap_ast_canonical_interpreter_allocation_scaling",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1");
        let output = native_harness::run_bounded(
            &mut command,
            Duration::from_secs(30),
            native_harness::OutputLimits::new(128 * 1024, 256 * 1024),
        )
        .expect("AstCanonical child must finish within the unchanged 30-second bound");
        assert!(
            output.status.success(),
            "AstCanonical allocation child failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("AstCanonical allocation gate passed")
        );
        // Report measurements, not the nested Rust harness's test-result lines:
        // CI must count this parent test exactly once.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let metrics = stdout
            .lines()
            .find_map(|line| {
                line.split_once("AstCanonical allocation gate passed: ")
                    .map(|(_, metrics)| metrics)
            })
            .expect("child allocation metrics");
        eprintln!("AstCanonical allocation gate passed: {metrics}");
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            if line.starts_with("AstCanonical metric:") {
                eprintln!("{line}");
            }
        }
        return;
    }
    // Source, parser AST, checker and warm-up stay outside the measured regions.
    // No async source/worker jobs: this TLS gate measures the interpreter thread.
    let warmup = prepared(4);
    let small = prepared(128);
    let large = prepared(256);
    Interpreter::new().run(warmup).expect("warm-up golden");
    let a = measure(small);
    let b = measure(large);
    // Doubling a bounded graph must remain below 3x in both signals.
    // Small fixed allowances absorb one-off runtime bookkeeping, not O(N^2).
    let max_calls = a
        .calls
        .checked_mul(3)
        .and_then(|n| n.checked_add(1024))
        .unwrap();
    let max_bytes = a
        .requested
        .checked_mul(3)
        .and_then(|n| n.checked_add(64 * 1024))
        .unwrap();
    assert!(
        b.calls <= max_calls,
        "quadratic allocation calls: {a:?} -> {b:?}"
    );
    assert!(
        b.requested <= max_bytes,
        "quadratic requested bytes: {a:?} -> {b:?}"
    );
    println!("AstCanonical allocation gate passed: {a:?} -> {b:?}");
}

#[test]
fn native_bootstrap_ast_canonical_owned_locals_close_allocations() {
    // Reuse the actual checked modules and independent 256-node golden. This
    // native gate proves normal cleanup, not OOM or interpreter live storage.
    let lowered = ku::ir::lower_program(&prepared(256)).expect("canonical IR");
    let optimized = ku::ir::optimize_program(&lowered);
    ku::ir::verify_borrow_contract(&optimized).expect("canonical borrow contract");
    let generated = ku::backend::c::generate_c_source(&optimized).expect("canonical native C");
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    for anchor in ["typedef struct KuString {", "int main(void) {"] {
        assert_eq!(
            generated.matches(anchor).count(),
            1,
            "native anchor: {anchor}"
        );
    }
    let mut harness = generated
        .replacen(
            "typedef struct KuString {",
            &format!(
                "{}\ntypedef struct KuString {{",
                native_allocation_harness::ALLOCATION_HOOK
            ),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int fixture_canonical_main(void) {",
            1,
        );
    harness.push_str(r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
int main(void) {
  int status=fixture_canonical_main();
  if (status || !ku_perf_calls || !ku_perf_total_bytes || !ku_perf_peak_bytes
      || ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow) {
    fprintf(stderr,"canonical lifecycle failed status=%d calls=%zu live=%zu bytes=%zu overflow=%d\n",
        status,ku_perf_calls,ku_perf_live_allocations,ku_perf_live_bytes,ku_perf_overflow);
    return 1;
  }
  printf("native AstCanonical lifecycle closed calls=%zu total=%zu peak=%zu\n",
      ku_perf_calls,ku_perf_total_bytes,ku_perf_peak_bytes);
  return 0;
}
"#);
    let directory = native_harness::TempDir::new("ast-canonical-native-lifecycle");
    let source = directory.path().join("program.c");
    std::fs::write(&source, harness).expect("write instrumented native canonical");
    let Some(executable) = native_harness::compile_harness(directory.path(), &source, "program")
    else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native AstCanonical"
        );
        return;
    };
    // No Ku source is written; remove the linked C before real binary execution.
    std::fs::remove_file(source).expect("remove native canonical C");
    let output = native_harness::run_bounded(
        Command::new(executable).current_dir(directory.path()),
        Duration::from_secs(30),
        native_harness::OutputLimits::new(128 * 1024, 256 * 1024),
    )
    .expect("native canonical execution must remain bounded");
    assert!(
        output.status.success(),
        "native canonical lifecycle failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&output.stdout)
        .starts_with("native AstCanonical lifecycle closed "));
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
}
