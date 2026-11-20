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
import { Span } from "../stage2/span.ku"
import { BuildNameTable } from "./names.ku"
import { SuppliedModule } from "./imports.ku"
import { ManifestPaths, ReadProjectModules } from "./project_manifest.ku"
import { BindTypeNames } from "./type_names.ku"
import { EmitScalarIntC, ScalarIntC } from "./scalar_int_c.ku"
import { EmitScalarProjectC, EmitScalarProjectProgramC } from "./scalar_project_c.ku"

fn Emit(&function: str): ScalarIntC! {
    if (fs.exists("modules.txt")) {
        at = Span { line: 1, column: 1, offset: 0, end_line: 1, end_column: 1, end_offset: 0 }
        paths = ManifestPaths(fs.read("modules.txt")?, at)?
        supplied = ReadProjectModules(paths)?
        if (function == "") { return ok(ScalarIntC { symbol: "", source: EmitScalarProjectProgramC(supplied)? }) }
        return EmitScalarProjectC(supplied, function)
    }
    named = BuildNameTable(ParseProgram(fs.read("input.ku")?)?)?
    supplied: [SuppliedModule] = []
    bindings = BindTypeNames(named, supplied)?
    return EmitScalarIntC(named, bindings, function)
}

fn main(): null! {
    output = Emit(fs.read("function.txt")?)?
    if (output.symbol == "") { print(output.source) }
    else {
        println(output.symbol)
        println(output.source)
    }
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

type Case = (&'static [i64], Result<i64, &'static str>);
const OVERFLOW: Result<i64, &str> = Err("integer overflow");
const ZERO_DIVISOR: Result<i64, &str> = Err("division by zero");

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
        let expected_output = execute(Command::new(ku).arg("run").arg(&oracle), cwd, 10);
        fs::remove_file(&oracle).expect("remove oracle before running independent executable");
        let actual = execute(
            Command::new(&app).args(args.iter().map(i64::to_string)),
            cwd,
            10,
        );
        match expected {
            Ok(value) => {
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
            Err(diagnostic) => {
                for failure in [&expected_output, &actual] {
                    assert_eq!(failure.status.code(), Some(1), "{name}{args:?}");
                    assert!(failure.stdout.is_empty(), "{name}{args:?}");
                    assert!(
                        String::from_utf8_lossy(&failure.stderr).contains(diagnostic),
                        "{name}{args:?}: {}",
                        String::from_utf8_lossy(&failure.stderr)
                    );
                }
            }
        }
    }
    generated
}

fn assert_checked_division(ku: &Path, seed: &Path, cwd: &Path) {
    let (_, project_emitter) = include_str!("../bootstrap/semantics/scalar_project_c.ku")
        .split_once("fn scalarProjectPlans(")
        .expect("the project emitter");
    let (_, module_slot) = project_emitter
        .split_once("module_slot = ")
        .expect("the compiler's module slot expression");
    let module_slot = module_slot.lines().next().unwrap();
    let source = format!("fn ModuleSlot(key: int): int {{ return {module_slot} }}");
    let division = compare_function(
        ku,
        seed,
        cwd,
        &source,
        "ModuleSlot",
        &[
            (&[1], Ok(0)),
            (&[4096], Ok(0)),
            (&[4097], Ok(1)),
            (&[8193], Ok(2)),
            (&[0], Ok(0)),
            (&[i64::MIN], OVERFLOW),
        ],
    );
    assert!(division.contains("static int64_t ku_scalar_div("));
    assert!(!division.contains("static int64_t ku_scalar_rem("));

    let source = r#"fn Div(a: int, b: int): int { return a / b }
fn Rem(a: int, b: int): int { return a % b }
fn Both(a: int, b: int): int { return Div(a, b) + Rem(a - 1, b) }"#;
    for (name, expected, min_expected) in [("Div", -2, i64::MIN / 2), ("Rem", -1, 0)] {
        let generated = compare_function(
            ku,
            seed,
            cwd,
            source,
            name,
            &[
                (&[-7, 3], Ok(expected)),
                (&[i64::MIN, 2], Ok(min_expected)),
                (&[1, 0], ZERO_DIVISOR),
                (&[i64::MIN, -1], OVERFLOW),
            ],
        );
        if name == "Rem" {
            assert!(generated.contains("static int64_t ku_scalar_rem("));
            assert!(!generated.contains("static int64_t ku_scalar_div("));
        }
    }
    let both = compare_function(
        ku,
        seed,
        cwd,
        source,
        "Both",
        &[
            (&[-7, 3], Ok(-4)),
            // The left call traps before evaluating the overflowing right argument.
            (&[i64::MIN, 0], ZERO_DIVISOR),
        ],
    );
    assert_eq!(
        both.matches("static void ku_scalar_check_divisor(").count(),
        1
    );
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
    let expected = execute(Command::new(ku).arg("run").arg(&expected_source), cwd, 10);
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

fn assert_capacity_results(ku: &Path, seed: &Path, cwd: &Path) {
    let lexer = include_str!("../bootstrap/stage1/lexer.ku");
    for (name, last_ok, code, message) in [
        (
            "CheckTokenCapacity",
            4095,
            "too_many_tokens",
            "stage-1 lexer accepts at most 4096 tokens including EOF",
        ),
        (
            "CheckStringCapacity",
            4096,
            "string_too_large",
            "stage-1 string literals accept at most 4096 decoded characters",
        ),
    ] {
        let (_, declaration) = lexer
            .split_once(&format!("fn {name}("))
            .expect("real capacity helper");
        let body = declaration
            .split_once("\n}")
            .expect("capacity helper closing brace")
            .0;
        let input = format!("fn {name}({body}\n}}\n");
        let emitted = build_function(seed, cwd, &input, name);
        assert_success(&emitted);
        let emitted = String::from_utf8(emitted.stdout).expect("capacity C source");
        let (symbol, c) = emitted
            .split_once('\n')
            .expect("capacity C symbol and source");
        assert!(symbol.trim().starts_with("ku_result_null_"));
        assert!(c.contains("struct KuResult_null { bool ok; uint8_t value; KuError error; }"));
        assert!(!c.contains("malloc("));
        let driver = r#"
#include <assert.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>

static void print_static(KuString value) {
    assert(value.storage == 0 && value.capacity == 0 && value.ptr != NULL);
    assert(fwrite(value.ptr, 1, value.len, stdout) == value.len);
    putchar('\n');
}

static void check_empty(KuString value) {
    assert(value.storage == 0 && value.capacity == 0 && value.ptr == NULL && value.len == 0);
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    KuResult_null result = CALL(strtoimax(argv[1], NULL, 10));
    assert(result.value == 0);
    if (result.ok) {
        check_empty(result.error.domain);
        check_empty(result.error.code);
        check_empty(result.error.message);
        puts("ok");
    } else {
        (void)CALL(0);
        print_static(result.error.domain);
        print_static(result.error.code);
        print_static(result.error.message);
    }
    return 0;
}
"#;
        let c_path = cwd.join(format!("{name}.c"));
        let app = cwd.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        fs::write(
            &c_path,
            format!("{c}{}", driver.replace("CALL", symbol.trim())),
        )
        .expect("save standalone capacity C");
        fs::remove_file(cwd.join("input.ku")).expect("retire input before C execution");
        fs::remove_file(cwd.join("function.txt")).expect("retire function selection");
        compile_c(&c_path, &app, cwd);

        let oracle = cwd.join("capacity-oracle.ku");
        for value in [i64::MIN, 4095, 4096, 4097, i64::MAX] {
            fs::write(
                &oracle,
                format!(
                    r#"{input}
fn main(): null! {{
    try {{
        {name}({})?
        println("ok")
    }} catch (err) {{
        println(err.domain)
        println(err.code)
        println(err.message)
    }}
    return ok(null)
}}
"#,
                    ku_int(value)
                ),
            )
            .expect("Stage0 interpreter capacity oracle");
            let interpreted = execute(Command::new(ku).arg("run").arg(&oracle), cwd, 10);
            assert_success(&interpreted);
            fs::remove_file(&oracle).expect("no Ku source during the independent C run");
            let actual = execute(Command::new(&app).arg(value.to_string()), cwd, 10);
            assert_success(&actual);
            let expected = if value <= last_ok {
                String::from("ok\n")
            } else {
                format!("bootstrap.lexer\n{code}\n{message}\n")
            };
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout).replace("\r\n", "\n"),
                expected
            );
            assert_eq!(
                String::from_utf8_lossy(&interpreted.stdout).replace("\r\n", "\n"),
                expected
            );
        }
    }
}

const OWNED_STRING_WITNESS: &str = r#"
#include <assert.h>
#include <stdlib.h>
#include <string.h>
static void *live[6];
static size_t calls, fail_at;
static void verify_clean(void) {
    for (size_t i = 0; i < 6; ++i) assert(live[i] == NULL);
}
static void *watch_malloc(size_t size) {
    ++calls;
    if (calls == fail_at) return NULL;
    assert(calls <= 5);
    live[calls] = malloc(size);
    assert(live[calls]);
    return live[calls];
}
static void watch_free(void *ptr) {
    for (size_t i = 0; i < 6; ++i) {
        if (live[i] == ptr) { live[i] = NULL; free(ptr); return; }
    }
    assert(0 && "owner already dropped or unrecognized");
}
#define malloc watch_malloc
#define free watch_free
"#;

fn assert_owned_replacement(ku: &Path, seed: &Path, cwd: &Path) {
    let token_module = include_str!("../bootstrap/stage1/token.ku");
    let generated = build_function(seed, cwd, token_module, "EscapeCanonical");
    assert_success(&generated);
    let generated = String::from_utf8(generated.stdout).expect("standalone C");
    let (name, body) = generated.split_once('\n').expect("symbol line and C");
    let driver = r#"
#undef malloc
#undef free
int main(int argc, char **argv) {
    static const uint8_t input[] = {'\\','\r','\n','\t','|',0,0xe4,0xb8,0xad,0xf0,0x9f,0x99,0x82};
    assert(argc == 2);
    fail_at = (size_t)strtoul(argv[1], NULL, 10);
    live[0] = malloc(sizeof(input));
    assert(live[0] && atexit(verify_clean) == 0);
    memcpy(live[0], input, sizeof(input));
    KuScalarString value = { live[0], sizeof(input), sizeof(input), 1u };
    KuBootstrapString result = CALL(value);
    assert(fail_at == 0 && calls == 5 && result.ptr == live[5] && result.storage == 1u);
    assert(fwrite(result.ptr, 1, result.len, stdout) == result.len);
    watch_free(result.ptr);
    return 0;
}
"#;
    let c_path = cwd.join("replace.c");
    let app = cwd.join(format!("replace{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &c_path,
        format!(
            "{OWNED_STRING_WITNESS}{body}{}",
            driver.replace("CALL", name.trim())
        ),
    )
    .expect("independent replacement C");
    fs::remove_file(cwd.join("input.ku")).expect("retire fixture source");
    compile_c(&c_path, &app, cwd);
    let oracle = cwd.join("escape-oracle.ku");
    fs::write(
        &oracle,
        format!(
            "{token_module}\nfn main() {{ print(EscapeCanonical(\"\\\\\\r\\n\\t|\0中🙂\")) }}\n"
        ),
    )
    .expect("unchanged interpreter leaf");
    let expected = execute(Command::new(ku).arg("run").arg(&oracle), cwd, 10);
    fs::remove_file(&oracle).expect("no Ku source needed by emitted program");
    assert_success(&expected);
    let actual = execute(Command::new(&app).arg("0"), cwd, 10);
    assert_success(&actual);
    assert_eq!(actual.stdout, expected.stdout);
    for attempt in 1..=5 {
        let failed = execute(Command::new(&app).arg(attempt.to_string()), cwd, 10);
        assert_eq!(failed.status.code(), Some(1), "{failed:?}");
        assert!(failed.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&failed.stderr).trim(),
            "out of memory"
        );
    }
    for source in [
        r#"fn Invalid(&text: str): str { return text.replace("x", "y") }"#,
        r#"fn Invalid(text: str): str { output = output.replace("x", "y")
           return output }"#,
    ] {
        let failed = build_function(seed, cwd, source, "Invalid");
        assert_eq!(failed.status.code(), Some(1), "{failed:?}");
        assert!(failed.stdout.is_empty());
        assert!(String::from_utf8_lossy(&failed.stderr).contains("unsupported_replace_operand"));
    }
}

fn assert_owned_composition(ku: &Path, seed: &Path, cwd: &Path) {
    let source = r#"
fn Compose(name: str): str {
    out = "[" + name
    tail = "字" + name
    out += tail
    tail = "]" + name
    return out + tail
}
"#;
    let generated = build_function(seed, cwd, source, "Compose");
    assert_success(&generated);
    let generated = String::from_utf8(generated.stdout).expect("standalone C");
    let (name, body) = generated.split_once('\n').expect("symbol line and C");
    assert!(!body.contains("ku_scalar_string_replace"));
    let driver = r#"
#undef malloc
#undef free
int main(int argc, char **argv) {
    static const uint8_t input[] = {0xf0,0x9f,0x99,0x82};
    assert(argc == 2);
    fail_at = (size_t)strtoul(argv[1], NULL, 10);
    live[0] = malloc(sizeof(input));
    assert(live[0] && atexit(verify_clean) == 0);
    memcpy(live[0], input, sizeof(input));
    KuBootstrapString result = CALL((KuScalarString){live[0], sizeof(input), sizeof(input), 1u});
    assert(fail_at == 0 && calls == 5 && result.ptr == live[5] && result.storage == 1u);
    assert(fwrite(result.ptr, 1, result.len, stdout) == result.len);
    watch_free(result.ptr);
    return 0;
}
"#;
    let c_path = cwd.join("join.c");
    let app = cwd.join(format!("join{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &c_path,
        format!(
            "{OWNED_STRING_WITNESS}{body}{}",
            driver.replace("CALL", name.trim())
        ),
    )
    .expect("independent composition C");
    compile_c(&c_path, &app, cwd);
    let oracle = cwd.join("join-oracle.ku");
    fs::write(
        &oracle,
        format!("{source}\nfn main() {{ print(Compose(\"🙂\")) }}\n"),
    )
    .unwrap();
    let expected = execute(Command::new(ku).arg("run").arg(&oracle), cwd, 10);
    assert_success(&expected);
    assert_eq!(expected.stdout, "[🙂字🙂]🙂".as_bytes());
    let actual = execute(Command::new(&app).arg("0"), cwd, 10);
    assert_success(&actual);
    assert_eq!(actual.stdout, expected.stdout);
    for attempt in 1..=5 {
        let failed = execute(Command::new(&app).arg(attempt.to_string()), cwd, 10);
        assert_eq!(failed.status.code(), Some(1), "{failed:?}");
        assert!(failed.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&failed.stderr).trim(),
            "out of memory"
        );
    }
    let invalid = source.replace("tail = \"]\" + name", "out = out + tail");
    let failed = build_function(seed, cwd, &invalid, "Compose");
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("moved_scalar_string"));
}

fn assert_imported_calls(ku: &Path, seed: &Path, cwd: &Path) {
    let entry = cwd.join("entry.ku");
    let source = r#"import { Aggregate } from "./bridge.ku"
import { Score as Primary } from "./left.ku"
import { Score as Secondary } from "./right.ku"
fn TestScore(a: int): int { return Aggregate(Primary(a), Secondary(a)) }
fn main() { println(TestScore(5)) }
"#;
    fs::write(&entry, source).unwrap();
    fs::write(
        cwd.join("bridge.ku"),
        "import { Add as MathAdd } from \"./math.ku\"\n\
         fn Aggregate(a: int, b: int): int { return MathAdd(a, b) }\n",
    )
    .unwrap();
    // Both functions have the same local node ID but distinct module identities.
    for (file, expression) in [("left.ku", "a + 1"), ("right.ku", "a * 2")] {
        fs::write(
            cwd.join(file),
            format!("fn Score(a: int): int {{ return {expression} }}\n"),
        )
        .unwrap();
    }
    fs::write(cwd.join("math.ku"), include_str!("../examples/math.ku")).unwrap();
    fs::write(cwd.join("function.txt"), "TestScore").unwrap();
    let manifest = cwd.join("modules.txt");
    let oracle = execute(Command::new(ku).arg("run").arg(&entry), cwd, 15);
    assert_success(&oracle);
    assert_eq!(String::from_utf8_lossy(&oracle.stdout).trim(), "16");
    for order in [
        "entry.ku\nright.ku\nmath.ku\nbridge.ku\nleft.ku\n",
        "entry.ku\nleft.ku\nbridge.ku\nright.ku\nmath.ku\n",
    ] {
        fs::write(&manifest, order).unwrap();
        let output = execute(&mut Command::new(seed), cwd, 20);
        assert_success(&output);
        let result = String::from_utf8(output.stdout).unwrap();
        let (symbol, code) = result.split_once('\n').expect("symbol and complete C");
        let symbol = symbol.trim();
        assert!(symbol.starts_with("ku_int_") && !code.contains("MathAdd"));
        assert_eq!(
            code.lines()
                .filter(|line| line.starts_with("static int64_t ku_int_"))
                .count(),
            4
        );
        let c = cwd.join("project.c");
        fs::write(
            &c,
            format!(
                "{code}\n#include <stdio.h>\nint main(void) {{ printf(\"%lld\\n\", (long long){symbol}(5)); return 0; }}\n"
            ),
        )
        .unwrap();
        let executable = cwd.join(format!("project{}", std::env::consts::EXE_SUFFIX));
        compile_c(&c, &executable, cwd);
        let observed = execute(&mut Command::new(&executable), cwd, 10);
        assert_success(&observed);
        assert_eq!(observed.stdout, oracle.stdout);
    }

    for (mutated, diagnostic) in [
        (source.replace("Primary(a)", "Primary()"), "call_arity"),
        (
            source.replace("Primary(a)", "Secondary(a, a)"),
            "call_arity",
        ),
        (
            source
                .replace("TestScore(a: int)", "TestScore(Primary: int, a: int)")
                .replace("TestScore(5)", "TestScore(0, 5)"),
            "unsupported_int_callee",
        ),
    ] {
        fs::write(&entry, mutated).unwrap();
        let result = execute(&mut Command::new(seed), cwd, 20);
        assert!(!result.status.success() && result.stdout.is_empty());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(diagnostic), "{stderr}");
    }
    fs::write(&entry, source).unwrap();
    fs::write(&manifest, "entry.ku\nright.ku\nbridge.ku\nleft.ku\n").unwrap();
    let failure = execute(&mut Command::new(seed), cwd, 20);
    assert!(!failure.status.success() && failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("unresolved_import"));
    fs::remove_file(manifest).unwrap();
}

fn assert_project_executable(ku: &Path, seed: &Path, cwd: &Path) {
    let main = cwd.join("entry.ku");
    let numbers = cwd.join("numbers.ku");
    let manifest = cwd.join("modules.txt");
    let symbol = cwd.join("function.txt");
    let import = "import { Increment as Answer } from \"./numbers.ku\"\n";
    let body = "fn main(): null! { println(Answer(41));";
    let helper = "fn Increment(value: int): int { return value + 1 }\n";
    fs::write(&symbol, "").unwrap();
    for (name, exit, status, stdout, stderr) in [
        ("success", "return ok(null)", 0, "42\n", ""),
        (
            "failure",
            "fail { domain: \"demo\", code: \"stopped\", message: \"停止🙂\0末\" }",
            1,
            "42\n",
            "停止🙂\0末\n",
        ),
        (
            "overflow",
            "println(Answer(9223372036854775807)); return ok(null)",
            1,
            "42\n",
            "integer overflow\n",
        ),
        (
            "branches-rejoin",
            r#"if (40 < 41 && !false) {
                println(Answer(9))
                println(Answer(19))
            } else {
                fail { domain: "flow", code: "wrong", message: "unreachable" }
            }
            println(90)
            return ok(null)"#,
            0,
            "42\n10\n20\n90\n",
            "",
        ),
        (
            "branches-exit",
            r#"if (false) {
                println(Answer(9223372036854775807))
                return ok(null)
            } else if (true) {
                println(Answer(-2))
                fail { domain: "flow", code: "expected", message: "chosen🙂" }
            } else {
                return ok(null)
            }"#,
            1,
            "42\n-1\n",
            "chosen🙂\n",
        ),
    ] {
        fs::write(&main, format!("{import}{body} {exit} }}\n")).unwrap();
        fs::write(&numbers, helper).unwrap();
        fs::write(&manifest, "entry.ku\nnumbers.ku\n").unwrap();
        let generated = execute(&mut Command::new(seed), cwd, 20);
        assert_success(&generated);
        let code = String::from_utf8(generated.stdout).unwrap();
        assert_eq!(code.matches("int main(void)").count(), 1);
        assert_eq!(code.matches("static int64_t ku_int_").count(), 1);
        let c_path = cwd.join(format!("{name}.c"));
        fs::write(&c_path, code).unwrap();
        let suffix = std::env::consts::EXE_SUFFIX;
        let native = cwd.join(format!("native-{name}{suffix}"));
        let standalone = cwd.join(format!("standalone-{name}{suffix}"));
        assert_success(&execute(
            Command::new(ku)
                .args(["build", "--release", "--offline"])
                .arg(&main)
                .arg("-o")
                .arg(&native),
            cwd,
            45,
        ));
        compile_c(&c_path, &standalone, cwd);
        for path in [&main, &numbers, &manifest, &c_path] {
            fs::remove_file(path).unwrap();
        }

        let native = execute(&mut Command::new(native), cwd, 10);
        let actual = execute(&mut Command::new(standalone), cwd, 10);
        assert_eq!(actual.status.code(), Some(status), "{actual:?}");
        assert_eq!(actual.status.code(), native.status.code(), "{native:?}");
        assert_eq!(actual.stdout, native.stdout);
        assert_eq!(actual.stderr, native.stderr);
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout).replace("\r\n", "\n"),
            stdout
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stderr).replace("\r\n", "\n"),
            stderr
        );
    }

    fs::write(&numbers, helper).unwrap();
    fs::write(&manifest, "entry.ku\nnumbers.ku\n").unwrap();
    for (source, diagnostic) in [
        (
            "fn main(value: int): null! { return ok(null) }",
            "unsupported_program_entry",
        ),
        (
            "fn main(): null! { println(Answer(\"wrong\")); return ok(null) }",
            "call_argument_type",
        ),
        (
            "fn println(value: int): int { return value }\nfn main(): null! { println(Answer(41)); return ok(null) }",
            "unsupported_program_statement",
        ),
        (
            "fn main(): null! { println(Answer(41)) }",
            "unsupported_program_exit",
        ),
        (
            "fn main(): null! { if (false) { return ok(null) } else { println(Answer(41)) } }",
            "unsupported_program_exit",
        ),
        (
            "fn main(): null! { if (\"str\") { return ok(null) } return ok(null) }",
            "unsupported_bool_expression",
        ),
    ] {
        fs::write(&main, format!("{import}{source}\n")).unwrap();
        let blocked = execute(&mut Command::new(seed), cwd, 20);
        assert_eq!(blocked.status.code(), Some(1), "{blocked:?}");
        assert!(blocked.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&blocked.stderr).contains(diagnostic),
            "{blocked:?}"
        );
    }
    fs::remove_file(manifest).unwrap();
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
            (&[1, 2], Ok(3)),
            (&[i64::MAX, 0], Ok(i64::MAX)),
            (&[i64::MIN, i64::MAX], Ok(-1)),
            (&[i64::MAX, 1], OVERFLOW),
            (&[i64::MIN, -1], OVERFLOW),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        math,
        "Twice",
        &[
            (&[7], Ok(14)),
            (&[i64::MIN / 2], Ok(i64::MIN)),
            (&[i64::MAX / 2], Ok(i64::MAX - 1)),
            (&[i64::MIN / 2 - 1], OVERFLOW),
            (&[i64::MAX / 2 + 1], OVERFLOW),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        math,
        "hidden",
        &[
            (&[0], Ok(-1)),
            (&[i64::MIN + 1], Ok(i64::MIN)),
            (&[i64::MIN], OVERFLOW),
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
            (&[4, 5, 9], Ok(63)),
            (&[-8, 2, 5], Ok(-18)),
            (&[i64::MAX, 1, 2], OVERFLOW),
            (&[0, 0, i64::MIN], OVERFLOW),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        varied,
        "Product",
        &[
            (&[i64::MIN, 0], Ok(0)),
            (&[-2, -3], Ok(6)),
            (&[i64::MAX, -1], Ok(-i64::MAX)),
            (&[-1, i64::MIN], OVERFLOW),
            (&[i64::MIN, -1], OVERFLOW),
        ],
    );
    compare_function(
        &ku,
        &seed,
        &cwd,
        varied,
        "Negate",
        &[(&[0], Ok(0)), (&[i64::MIN], OVERFLOW)],
    );
    compare_function(&ku, &seed, &cwd, varied, "Limit", &[(&[], Ok(i64::MAX))]);

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
            (&[3, 4], Ok(6)),
            (&[-3, -4], Ok(2)),
            (&[i64::MAX, 0], OVERFLOW),
            (&[0, i64::MIN], OVERFLOW),
            (&[0, i64::MAX], OVERFLOW),
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
    assert!(!generated.contains("ku_scalar_check_divisor"));
    assert_checked_division(&ku, &seed, &cwd);
    assert_string_composition(&ku, &seed, &cwd);
    assert_owned_replacement(&ku, &seed, &cwd);
    assert_owned_composition(&ku, &seed, &cwd);
    assert_capacity_results(&ku, &seed, &cwd);
    assert_imported_calls(&ku, &seed, &cwd);
    assert_project_executable(&ku, &seed, &cwd);

    for (input, diagnostic) in [
        (
            "fn Example(): int! { return ok(1) }",
            "unsupported_int_type",
        ),
        (
            "fn Example(): null! { return ok(1) }",
            "unsupported_null_result",
        ),
        (
            "fn Example(ok: int): null! { return ok(null) }",
            "unsupported_null_result",
        ),
        (
            "fn Example(message: str): null! { fail { domain: \"x\", code: \"y\", message: message } }",
            "unsupported_static_error_field",
        ),
        (
            "fn Example(): null! { if (\"x\" >= 4096) { return ok(null) } return ok(null) }",
            "unsupported_int_comparison",
        ),
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
            "fn Example(a: int): int { return a / 2.0 }",
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
