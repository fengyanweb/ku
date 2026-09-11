//! Import expansion must preserve lexical type parameters independently of
//! the module's concrete types and value symbols.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use ku::cli::{check_source, run_source};
use native_pg_harness::TempDir;
use std::fs;

const LIBRARY: &str = r#"
struct T { value: int }
fn Read<T>(value: T): T { return value }
fn Inspect<T>(&value: T) { println(value) }
fn Concrete(): T { return T { value: 9 } }
"#;

#[test]
fn namespace_import_keeps_generic_types_and_borrowed_modes() {
    let directory = TempDir::new("generic-import-namespace");
    fs::write(directory.path().join("library.ku"), LIBRARY).expect("write library");
    let file = directory.path().join("main.ku");
    let source = r#"import lib from "./library.ku"
fn main() {
    println(lib.Read(7))
    text = lib.Read("owned")
    lib.Inspect(text)
    lib.Inspect(text)
    println(text)
    concrete = lib.Concrete()
    println(concrete.value)
}
"#;
    fs::write(&file, source).expect("write main");
    check_source(file.to_str().expect("path"), source).expect("generic namespace check");
    run_source(file.to_str().expect("path"), source).expect("generic namespace run");
}

#[test]
fn diamond_import_aliases_do_not_change_generic_type_identity() {
    let directory = TempDir::new("generic-import-diamond");
    fs::write(directory.path().join("library.ku"), LIBRARY).expect("write library");
    fs::write(
        directory.path().join("left.ku"),
        "import { Read as ReadLeft } from \"./library.ku\"\nfn Left(): int { return ReadLeft(1) }\n",
    ).expect("write left");
    fs::write(
        directory.path().join("right.ku"),
        "import { Read as ReadRight } from \"./library.ku\"\nfn Right(): str { return ReadRight(\"right\") }\n",
    ).expect("write right");
    let file = directory.path().join("main.ku");
    let source = r#"import { Left } from "./left.ku"
import { Right } from "./right.ku"
fn main() { println(Left()) println(Right()) }
"#;
    fs::write(&file, source).expect("write main");
    check_source(file.to_str().expect("path"), source).expect("generic diamond check");
    run_source(file.to_str().expect("path"), source).expect("generic diamond run");
}

#[test]
fn imported_generic_body_annotation_keeps_the_lexical_type_parameter() {
    let directory = TempDir::new("generic-import-body-type");
    fs::write(
        directory.path().join("library.ku"),
        "struct T { value: int }\nfn Read<T>(value: T): T { local: T = value return local }\n",
    )
    .expect("write library");
    let file = directory.path().join("main.ku");
    let source = "import lib from \"./library.ku\"\nfn main() { println(lib.Read(7)) println(lib.Read(\"owned\")) }\n";
    fs::write(&file, source).expect("write main");
    check_source(file.to_str().expect("path"), source).expect("generic body annotation check");
    run_source(file.to_str().expect("path"), source).expect("generic body annotation run");
}
