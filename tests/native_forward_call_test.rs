//! Declaration order must not change a statically checked function's C ABI.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod harness;

use harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

#[test]
fn native_forward_calls_and_mutual_recursion_preserve_owned_and_borrowed_abis() {
    let directory = TempDir::new("native-forward-calls");
    let source = r#"
fn main() {
    text = First(3)
    println(Read(text))
    println(text)
    values = LaterArray()
    println(values[1])
}
fn First(count: int): str {
    if (count == 0) { return "done" }
    return Second(count - 1)
}
fn Second(count: int): str {
    if (count == 0) { return "done" }
    return First(count - 1)
}
fn Read(&text: str): int { return text.len() }
fn LaterArray(): [int] { return [1, 2] }
"#;
    let c = emit_c(directory.path(), source);
    let declaration = "KuString Second(int64_t count);";
    let definition = "KuString Second(int64_t count) {";
    assert!(c.find(declaration).unwrap() < c.find("KuString First(int64_t count) {").unwrap());
    assert_eq!(c.matches(declaration).count(), 1);
    assert_eq!(c.matches(definition).count(), 1);
    assert!(c.contains("int64_t Read(const KuString* text);"));
    assert!(c.contains("int64_t Read(const KuString* text) {"));
    assert!(!c.contains("run_source") && !c.contains("const SOURCE"));
    let file = directory.path().join("forward.c");
    fs::write(&file, c).expect("write C artifact");
    let Some(executable) = compile_harness(directory.path(), &file, "forward") else {
        return;
    };
    fs::remove_file(directory.path().join("main.ku")).expect("remove Ku source");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("bounded native execution");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        "4\ndone\n2\n"
    );
}
