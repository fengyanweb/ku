//! Native generic specialization must use the same expanded import graph.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod harness;

use harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

#[test]
fn native_generic_diamond_imports_run_after_the_complete_source_graph_is_removed() {
    let directory = TempDir::new("native-generic-import");
    let sources = directory.path().join("sources");
    fs::create_dir(&sources).expect("create isolated source graph");
    fs::write(
        sources.join("library.ku"),
        r#"
struct T { value: int }
fn Read<T>(value: T): T { local: T = value return local }
fn Copy<T>(&value: T): T { return value.clone() }
"#,
    )
    .expect("write generic library");
    fs::write(sources.join("left.ku"), "import { Read as LeftRead } from \"./library.ku\"\nfn Left(): int { return LeftRead(1) }\n").unwrap();
    fs::write(sources.join("right.ku"), "import { Read as RightRead } from \"./library.ku\"\nfn Right(): str { return RightRead(\"right\") }\n").unwrap();
    let source = r#"
import { Left } from "./left.ku"
import { Right } from "./right.ku"
import lib from "./library.ku"
fn main() {
    println(Left())
    println(Right())
    println(lib.Read(1))
    text = "owned"
    println(lib.Copy(text))
    println(text)
}
"#;
    let c = emit_c(&sources, source);
    assert!(c.contains("__ku_ns_generic_"));
    assert!(!c.contains("run_source") && !c.contains("const SOURCE"));
    let artifact = directory.path().join("imported.c");
    fs::write(&artifact, c).expect("write independent C artifact");
    // Even C compilation may not depend on the original Ku modules.
    fs::remove_dir_all(&sources).expect("remove this fixture's complete source graph");
    let Some(executable) = compile_harness(directory.path(), &artifact, "imported") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("bounded source-free native execution");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        "1\nright\n1\nowned\nowned\n"
    );
}
