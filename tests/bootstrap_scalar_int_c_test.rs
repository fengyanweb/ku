//! The hosted Ku compiler lowers a checked integer subset to independent C11.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SEED: &str = r#"import "std.fs"
import { ParseProgram } from "../stage3/parser.ku"
import { BuildNameTable } from "./names.ku"
import { SuppliedModule } from "./imports.ku"
import { BindTypeNames } from "./type_names.ku"
import { EmitScalarIntC } from "./scalar_int_c.ku"

fn main(): null! {
    named = BuildNameTable(ParseProgram(fs.read("input.ku")?)?)?
    supplied: [SuppliedModule] = []
    bindings = BindTypeNames(named, supplied)?
    function = fs.read("function.txt")?
    output = EmitScalarIntC(named, bindings, function)?
    println(output.symbol)
    println(output.source)
    return ok(null)
}
"#;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ku-bootstrap-scalar-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create private scalar test root");
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn execute(command: &mut Command, cwd: &Path, seconds: u64) -> BoundedOutput {
    run_bounded(
        command.current_dir(cwd),
        Duration::from_secs(seconds),
        OutputLimits::new(256 * 1024, 512 * 1024),
    )
    .expect("bounded scalar command")
}

#[track_caller]
fn assert_success(result: &BoundedOutput) {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn copy_seed_sources(target: &Path) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for stage in ["stage1", "stage2", "stage3", "semantics"] {
        let path = target.join(stage);
        fs::create_dir_all(&path).expect("stage directory");
        for entry in fs::read_dir(root.join("bootstrap").join(stage)).expect("read stage") {
            let entry = entry.expect("stage source");
            if entry.path().extension().is_some_and(|ext| ext == "ku") {
                fs::copy(entry.path(), path.join(entry.file_name())).expect("copy stage source");
            }
        }
    }
    fs::write(target.join("semantics/seed.ku"), SEED).expect("write test seed");
}

fn compile_c(source: &Path, output: &Path, cwd: &Path) {
    let cc = std::env::var("KU_CC")
        .unwrap_or_else(|_| if cfg!(windows) { "clang" } else { "cc" }.into());
    let tokens: Vec<&str> = if Path::new(&cc).is_file() {
        vec![&cc]
    } else {
        cc.split_whitespace().collect()
    };
    let (program, args) = tokens.split_first().expect("a C compiler");
    let mut command = Command::new(program);
    command.args(args);
    let stem = Path::new(program)
        .file_stem()
        .and_then(|name| name.to_str());
    if stem.is_some_and(|stem| {
        stem.eq_ignore_ascii_case("cl") || stem.eq_ignore_ascii_case("clang-cl")
    }) {
        command
            .args(["/nologo", "/std:c11", "/O2", "/WX"])
            .arg(source)
            .arg(format!("/Fe{}", output.display()))
            .arg(format!("/Fo{}", output.with_extension("obj").display()));
    } else {
        command
            .args([
                "-std=c11",
                "-pedantic-errors",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-O2",
            ])
            .arg(source)
            .arg("-o")
            .arg(output);
    }
    assert_success(&execute(&mut command, cwd, 30));
}

fn ku_int(value: i64) -> String {
    if value == i64::MIN {
        "(-9223372036854775807 - 1)".to_owned()
    } else {
        value.to_string()
    }
}

fn build_function(ku_seed: &Path, cwd: &Path, source: &str, name: &str) -> BoundedOutput {
    fs::write(cwd.join("input.ku"), source).expect("write function input");
    fs::write(cwd.join("function.txt"), name).expect("write function selection");
    execute(&mut Command::new(ku_seed), cwd, 15)
}

type Case = (&'static [i64], Option<i64>);

fn compare_function(
    ku: &Path,
    seed: &Path,
    cwd: &Path,
    source: &str,
    name: &str,
    cases: &[Case],
) -> String {
    let generated = build_function(seed, cwd, source, name);
    assert_success(&generated);
    let generated = String::from_utf8(generated.stdout).expect("UTF-8 C program");
    let (symbol, mut c) = generated
        .split_once('\n')
        .map(|(symbol, c)| (symbol.trim(), c.to_owned()))
        .expect("symbol line and standalone C");
    assert!(symbol
        .strip_prefix("ku_int_")
        .is_some_and(|id| { !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) }));
    let arity = cases[0].0.len();
    assert!(cases.iter().all(|(args, _)| args.len() == arity));
    c.push_str("#include <inttypes.h>\n#include <stdio.h>\n");
    write!(
        c,
        "int main(int argc, char **argv) {{\n    if (argc != {}) return 2;\n",
        arity + 1
    )
    .expect("format C driver");
    if arity == 0 {
        c.push_str("    (void)argv;\n");
    }
    write!(c, "    int64_t result = {symbol}(").expect("write C call");
    let args: Vec<_> = (1..=arity)
        .map(|index| format!("(int64_t)strtoimax(argv[{index}], NULL, 10)"))
        .collect();
    c.push_str(&args.join(", "));
    c.push_str(");\n    printf(\"%\" PRId64 \"\\n\", result);\n    return 0;\n}\n");
    let c_path = cwd.join(format!("{name}.c"));
    let app = cwd.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c_path, c).expect("save independent C program");
    fs::remove_file(cwd.join("input.ku")).expect("no input source available to emitted program");
    fs::remove_file(cwd.join("function.txt")).expect("remove selection file");
    compile_c(&c_path, &app, cwd);

    let declarations = source
        .split_once("fn main")
        .map_or(source, |(body, _)| body);
    let oracle = cwd.join("oracle.ku");
    for (args, expected) in cases {
        let oracle_args = args
            .iter()
            .copied()
            .map(ku_int)
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            &oracle,
            format!("{declarations}\nfn main() {{ println({name}({oracle_args})) }}\n"),
        )
        .expect("write interpreter oracle");
        let expected_output = execute(&mut Command::new(ku).arg("run").arg(&oracle), cwd, 10);
        fs::remove_file(&oracle).expect("remove oracle before running independent executable");
        let actual = execute(
            Command::new(&app).args(args.iter().map(i64::to_string)),
            cwd,
            10,
        );
        match expected {
            Some(value) => {
                assert_success(&expected_output);
                assert_success(&actual);
                assert_eq!(
                    String::from_utf8_lossy(&actual.stdout).trim(),
                    value.to_string()
                );
                assert_eq!(
                    String::from_utf8_lossy(&actual.stdout).replace("\r\n", "\n"),
                    String::from_utf8_lossy(&expected_output.stdout).replace("\r\n", "\n"),
                    "{name}{args:?}"
                );
            }
            None => {
                for failure in [&expected_output, &actual] {
                    assert!(!failure.status.success(), "{name}{args:?}");
                    assert!(failure.stdout.is_empty(), "{name}{args:?}");
                    assert!(
                        String::from_utf8_lossy(&failure.stderr).contains("integer overflow"),
                        "{name}{args:?}: {}",
                        String::from_utf8_lossy(&failure.stderr)
                    );
                }
            }
        }
    }
    generated
}

const ALLOCATOR_WATCH: &str = r#"
#include <assert.h>
#include <stdlib.h>
#include <string.h>
static void *input_to_drop;
static size_t input_drops;
static size_t output_allocations;
static int fail_allocation;
static void *watch_malloc(size_t size) {
    ++output_allocations;
    return fail_allocation ? NULL : malloc(size);
}
static void watch_free(void *ptr) {
    assert(ptr == input_to_drop);
    ++input_drops;
    free(ptr);
}
static void verify_input_drop(void) { assert(input_drops == 1); }
#define malloc watch_malloc
#define free watch_free
"#;

fn assert_string_composition(ku: &Path, seed: &Path, cwd: &Path) {
    let lexer = include_str!("../bootstrap/stage1/lexer.ku");
    let (_, at) = lexer.split_once("fn At(").expect("real lexer At");
    let at = at.split_once("\n}").expect("At closing brace").0;
    let input = format!("fn At({at}\n}}\n");
    let generated = build_function(seed, cwd, &input, "At");
    assert_success(&generated);
    let generated = String::from_utf8(generated.stdout).expect("C output");
    let (name, source) = generated.split_once('\n').expect("C symbol and source");
    assert!(name.trim().starts_with("ku_str_"));
    let driver = r#"
#undef malloc
#undef free
int main(int argc, char **argv) {
    static const char input[] = "\344\270\255\360\237\231\202\000z";
    static const char expected[] =
        "-9223372036854775808:0@9223372036854775807: \344\270\255\360\237\231\202\000z";
    assert(argc == 2);
    fail_allocation = strcmp(argv[1], "oom") == 0;
    size_t len = strcmp(argv[1], "overflow") == 0 ? SIZE_MAX : sizeof(input) - 1;
    input_to_drop = malloc(sizeof(input));
    assert(input_to_drop && atexit(verify_input_drop) == 0);
    memcpy(input_to_drop, input, sizeof(input));
    KuScalarString message = { input_to_drop, len, sizeof(input), 1 };
    KuBootstrapString result = CALL(message, INT64_MIN, 0, INT64_MAX);
    assert(result.storage == 1 && result.len == sizeof(expected) - 1);
    assert(result.capacity == result.len && input_drops == 1 && output_allocations == 1);
    assert(memcmp(result.ptr, expected, result.len) == 0);
    assert(fwrite(result.ptr, 1, result.len, stdout) == result.len);
    free(result.ptr);
    return 0;
}
"#;
    let c_path = cwd.join("at.c");
    let app = cwd.join(format!("at{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &c_path,
        format!(
            "{ALLOCATOR_WATCH}{source}{}",
            driver.replace("CALL", name.trim())
        ),
    )
    .expect("standalone At C");
    fs::remove_file(cwd.join("input.ku")).expect("retire input before running emitted At");
    compile_c(&c_path, &app, cwd);

    let expected_source = cwd.join("at-oracle.ku");
    fs::write(
        &expected_source,
        format!(
            "{input}\nfn main() {{ print(At(\"中🙂\0z\", {}, 0, {})) }}\n",
            ku_int(i64::MIN),
            i64::MAX
        ),
    )
    .expect("At interpreter oracle");
    let expected = execute(
        &mut Command::new(ku).arg("run").arg(&expected_source),
        cwd,
        10,
    );
    assert_success(&expected);
    fs::remove_file(&expected_source).expect("source-free standalone At");
    let actual = execute(Command::new(&app).arg("success"), cwd, 10);
    assert_success(&actual);
    assert_eq!(actual.stdout, expected.stdout);

    for (mode, diagnostic) in [
        ("oom", "out of memory"),
        ("overflow", "string allocation is too large"),
    ] {
        let failed = execute(Command::new(&app).arg(mode), cwd, 10);
        assert_eq!(failed.status.code(), Some(1), "{mode}: {failed:?}");
        assert!(failed.stdout.is_empty(), "{mode}: {failed:?}");
        assert_eq!(String::from_utf8_lossy(&failed.stderr).trim(), diagnostic);
    }

    let borrowed = build_function(
        seed,
        cwd,
        r#"fn Show(&value: str): str {
            if (value == "") { return "empty" }
            return "[" + value + "]"
        }"#,
        "Show",
    );
    assert_success(&borrowed);
    let borrowed = String::from_utf8(borrowed.stdout).expect("C output");
    let (name, body) = borrowed.split_once('\n').expect("C symbol and source");
    let driver = r#"
#include <assert.h>
int main(void) {
    uint8_t original[] = {'a', 0, 'b'};
    KuScalarString input = { original, sizeof(original), sizeof(original), 1 };
    KuBootstrapString result = CALL(&input);
    assert(result.storage == 1 && result.len == 5 && memcmp(result.ptr, "[a\000b]", 5) == 0);
    assert(input.ptr == original && input.len == 3 && memcmp(input.ptr, "a\000b", 3) == 0);
    free(result.ptr);
    KuScalarString empty = {0};
    result = CALL(&empty);
    assert(result.storage == 0 && result.len == 5 && memcmp(result.ptr, "empty", 5) == 0);
    return 0;
}
"#;
    let c_path = cwd.join("show.c");
    let app = cwd.join(format!("show{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &c_path,
        format!("{body}{}", driver.replace("CALL", name.trim())),
    )
    .expect("standalone Show C");
    fs::remove_file(cwd.join("input.ku")).expect("retire input before running emitted Show");
    compile_c(&c_path, &app, cwd);
    assert_success(&execute(&mut Command::new(&app), cwd, 10));
}

#[test]
fn ku_emits_checked_c11_for_real_math_functions() {
    let work = Temp::new();
    let compiler = work.0.join("compiler");
    let cwd = work.0.join("consumer");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let executable = format!("seed{}", std::env::consts::EXE_SUFFIX);
    let built_seed = compiler.join("semantics").join(&executable);
    let seed = cwd.join(executable);
    fs::create_dir(&cwd).expect("isolated invocation directory");
    copy_seed_sources(&compiler);
    assert_success(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(compiler.join("semantics/seed.ku"))
            .arg("-o")
            .arg(&built_seed),
        &cwd,
        150,
    ));
    fs::rename(&built_seed, &seed).expect("relocate seed with executable-relative filesystem base");
    fs::remove_dir_all(&compiler).expect("retire hosted compiler source before using seed");

    let math = include_str!("../examples/math.ku");
    compare_function(
        &ku,
        &seed,
        &cwd,
        math,
        "Add",
        &[
            (&[1, 2], Some(3)),
            (&[i64::MAX, 0], Some(i64::MAX)),
            (&[i64::MIN, i64::MAX], Some(-1)),
            (&[i64::MAX, 1], None),
            (&[i64::MIN, -1], None),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        math,
        "Twice",
        &[
            (&[7], Some(14)),
            (&[i64::MIN / 2], Some(i64::MIN)),
            (&[i64::MAX / 2], Some(i64::MAX - 1)),
            (&[i64::MIN / 2 - 1], None),
            (&[i64::MAX / 2 + 1], None),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        math,
        "hidden",
        &[
            (&[0], Some(-1)),
            (&[i64::MIN + 1], Some(i64::MIN)),
            (&[i64::MIN], None),
        ],
    );
    let varied = "fn Combine(a: int, b: int, c: int): int { return (a + b) * (c - 2) }\n\
                  fn Product(a: int, b: int): int { return a * b }\n\
                  fn Negate(a: int): int { return -a }\n\
                  fn Limit(): int { return 9223372036854775807 }\n";
    compare_function(
        &ku,
        &seed,
        &cwd,
        varied,
        "Combine",
        &[
            (&[4, 5, 9], Some(63)),
            (&[-8, 2, 5], Some(-18)),
            (&[i64::MAX, 1, 2], None),
            (&[0, 0, i64::MIN], None),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        varied,
        "Product",
        &[
            (&[i64::MIN, 0], Some(0)),
            (&[-2, -3], Some(6)),
            (&[i64::MAX, -1], Some(-i64::MAX)),
            (&[-1, i64::MIN], None),
            (&[i64::MIN, -1], None),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        varied,
        "Negate",
        &[(&[0], Some(0)), (&[i64::MIN], None)],
    );
    compare_function(&ku, &seed, &cwd, varied, "Limit", &[(&[], Some(i64::MAX))]);

    // The declaration order is deliberately different from the call order.
    // Two branches share Twice, and Shifted also calls a zero-argument helper.
    let composed = format!(
        "fn Composed(a: int, b: int): int {{ return Add(Shifted(a), hidden(b)) - Twice(b) }}\n\
         fn Irrelevant(a: int): int {{ return a / 2 }}\n\
         fn Shifted(a: int): int {{ return Twice(a) + Scale() }}\n\
         fn Scale(): int {{ return 5 }}\n{math}"
    );
    let generated = compare_function(
        &ku,
        &seed,
        &cwd,
        &composed,
        "Composed",
        &[
            (&[3, 4], Some(6)),
            (&[-3, -4], Some(2)),
            (&[i64::MAX, 0], None),
            (&[0, i64::MIN], None),
            (&[0, i64::MAX], None),
        ],
    );
    assert_eq!(
        generated
            .lines()
            .filter(|line| line.starts_with("static int64_t ku_int_"))
            .count(),
        5,
        "only reachable local helpers, each emitted once"
    );
    assert!(!generated.contains("Irrelevant"));
    assert_string_composition(&ku, &seed, &cwd);

    for (input, diagnostic) in [
        (
            "fn Example(str: int): str { return str(1) }",
            "unsupported_string_int_call",
        ),
        (
            "fn Example(a: str): str { return \"x\" + str(a) }",
            "unsupported_string_int_operand",
        ),
        (
            "fn Example(a: str): str { return a }",
            "unsupported_string_return",
        ),
        (
            "fn Example(a: float): int { return 1 }",
            "unsupported_int_type",
        ),
        (
            "fn Example(a: int, a: int): int { return a }",
            "duplicate_int_parameter",
        ),
        (
            "fn Example(a: int): int { return absent }",
            "undefined_int_operand",
        ),
        (
            "fn Example(a: int): int { return a / 2 }",
            "unsupported_int_expression",
        ),
        (
            "fn Example(a: int): int { return Missing(a) }",
            "unknown_int_callee",
        ),
        (
            "fn Example(a: int): int { return Example(a) }",
            "recursive_int_call",
        ),
        (
            "fn Other(a: int): int { return Example(a) }\n\
             fn Example(a: int): int { return Other(a) }",
            "recursive_int_call",
        ),
        (
            "fn Example(Helper: int): int { return Helper(1) }\n\
             fn Helper(a: int): int { return a }",
            "unsupported_int_callee",
        ),
        (
            "fn Helper(a: int): int { return a }\n\
             fn Example(): int { return Helper() }",
            "int_call_arity",
        ),
        (
            "fn Helper(a: float): int { return 1 }\n\
             fn Example(a: int): int { return Helper(a) }",
            "unsupported_int_type",
        ),
        (
            "fn Helper(a: int): int { return a }\n\
             fn Example(): int { return Helper }",
            "undefined_int_operand",
        ),
    ] {
        let rejected = build_function(&seed, &cwd, input, "Example");
        assert!(!rejected.status.success(), "{input}");
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            stderr.contains(diagnostic) && stderr.contains("<source>"),
            "{input}: {stderr}"
        );
        assert!(rejected.stdout.is_empty(), "{input}");
    }
}
