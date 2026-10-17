//! Source witness for inference-time native zero crossing a cleanup-attempt
//! floor. It uses the existing timeout Safepoint, not new sync integer signals.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, IrInst, IrTerminator, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

const SOURCE: &str = r#"
fn Marker(n: int) { println(n) }
fn main(): null! {
    values = [1, -1].map(x => {
        try { if (x < 0) { return 9 } }
        finally { Marker(x) }
        return x + 2
    })
    println(values[0])
    println(values[1])
    return ok(null)
}
"#;

#[test]
fn native_closure_cleanup_zero_keeps_concrete_return_after_attempt_floor() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new()
        .check(&ast)
        .expect("legal closure with ordinary and return-finally paths");
    let lowered = ir::lower_program(&ast).unwrap();
    let closure = lowered
        .functions
        .iter()
        .find(|function| function.is_closure_body)
        .unwrap();
    assert_eq!(closure.return_type, IrType::Int);
    for block in &closure.blocks {
        for inst in &block.instructions {
            if let IrInst::Temp { ty, .. } = inst {
                assert_ne!(
                    *ty,
                    IrType::Unknown,
                    "synthetic timeout zero must remain identifiable"
                );
            }
        }
        if let IrTerminator::Return(Some(value)) = &block.terminator {
            assert_eq!(
                value.ty,
                IrType::Int,
                "cleanup floor cannot retain Unknown return payload"
            );
        }
    }
    let optimized = ir::optimize_program(&lowered);
    ir::verify_borrow_contract(&optimized).unwrap();
    let generated = c::generate_c_source(&optimized).expect("native zero is concretely typed");
    assert!(!generated.contains("<native-zero>"));
    assert!(!generated.contains("run_source"));
    let directory = TempDir::new("closure-cleanup-zero");
    let ku_path = directory.path().join("main.ku");
    fs::write(&ku_path, SOURCE).unwrap();
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .arg("run")
            .arg(&ku_path),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("interpreter source witness finishes");
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    let expected = "1\n-1\n3\n9\n";
    assert_eq!(
        String::from_utf8(interpreted.stdout)
            .unwrap()
            .replace('\r', ""),
        expected
    );
    assert!(interpreted.stderr.is_empty());
    let c_path = directory.path().join("program.c");
    fs::write(&c_path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires actual native execution"
        );
        return;
    };
    fs::remove_file(c_path).unwrap();
    fs::remove_file(ku_path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("native source witness finishes without source/C artifacts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected
    );
    assert!(output.stderr.is_empty());
}
