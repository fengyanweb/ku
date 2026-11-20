//! Typed layout and struct projection checking run in the Ku compiler seed.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use ku::{span::Span, stdlib::json, value::Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ku-bootstrap-layout-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create private layout test root");
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stage_sources(source: &Path) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for stage in ["stage1", "stage2", "stage3", "semantics"] {
        let target = source.join(stage);
        fs::create_dir(&target).expect("create copied compiler stage");
        for entry in
            fs::read_dir(root.join("bootstrap").join(stage)).expect("read compiler sources")
        {
            let entry = entry.expect("stage source");
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "ku")
            {
                fs::copy(entry.path(), target.join(entry.file_name()))
                    .expect("copy hostable Ku compiler module");
            }
        }
    }
}

fn execute(command: &mut Command, cwd: &Path, limit: u64) -> BoundedOutput {
    command.current_dir(cwd);
    run_bounded(
        command,
        Duration::from_secs(limit),
        OutputLimits::new(256 * 1024, 512 * 1024),
    )
    .expect("bounded schema command")
}

fn assert_ok(result: &BoundedOutput) {
    assert!(
        result.status.success(),
        "schema program failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn coordinates(input: &str, position: usize) -> String {
    let preceding = &input[..position];
    let line = preceding.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = preceding
        .rsplit('\n')
        .next()
        .expect("last line")
        .chars()
        .count()
        + 1;
    format!("{line}:{column}@{position}")
}

fn stage0_detail(input: &str, output: &BoundedOutput) -> String {
    assert_eq!(output.status.code(), Some(1));
    let document = String::from_utf8(output.stderr.clone()).expect("UTF-8 from Stage0");
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(document)], Span::default())
            .expect("read Stage0 JSON")
    else {
        panic!("expected one Stage0 diagnostic, got {:?}", output.stderr);
    };
    let Value::Object(diagnostic) = *value else {
        panic!("expected a Stage0 diagnostic object")
    };
    let coord = |field: &str| {
        let Value::Int(value) = diagnostic[field] else {
            panic!("bad coordinate {field}")
        };
        usize::try_from(value).expect("nonnegative Stage0 coordinate")
    };
    let position = |line: usize, column: usize| {
        let mut lines = input.split_inclusive('\n');
        let base: usize = (&mut lines).take(line - 1).map(str::len).sum();
        let text = lines.next().expect("Stage0 line");
        let offset = text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .nth(column - 1)
            .expect("Stage0 column");
        format!("{line}:{column}@{}", base + offset)
    };
    let Value::String(message) = &diagnostic["message"] else {
        panic!("expected Stage0 message")
    };
    let start = position(coord("line"), coord("column"));
    let end = position(coord("endLine"), coord("endColumn"));
    format!("{message}|{start}..{end}")
}

const INSPECTOR: &str = r#"import "std.fs"
import { ParseProgram } from "../stage3/parser.ku"
import { BuildNameTable } from "./names.ku"
import { SuppliedModule } from "./imports.ku"
import { BindTypeNames } from "./type_names.ku"
import { BuildStructLayouts, BindStructReturn, StructLayout, StructReturnPlan } from "./struct_layouts.ku"

struct Facts {
    layout: StructLayout
    plan: StructReturnPlan
}

fn Inspect(): Facts! {
    named = BuildNameTable(ParseProgram(fs.read("input.ku")?)?)?
    token = BuildNameTable(ParseProgram(fs.read("supplier.ku")?)?)?
    supplied: [SuppliedModule] = [SuppliedModule { path: "../stage1/token.ku", named: token }]
    bindings = BindTypeNames(named, supplied)?
    layouts = BuildStructLayouts(named, bindings, supplied)?
    function = fs.read("function.txt")?
    plan = BindStructReturn(named, bindings, layouts, function)?
    index = 0
    while (index < layouts.len()) {
        if (layouts[index].node == plan.result_layout) {
            return ok(Facts { layout: layouts[index].clone(), plan: plan })
        }
        index = index + 1
    }
    panic("plan does not refer to the declared result layout")
    return ok(Facts { layout: layouts[0].clone(), plan: plan })
}

fn main(): null! {
    facts = Inspect()?
    plan = facts.plan
    layout = facts.layout
    println("FUNCTION|" + plan.function_name + "|" + layout.name)
    fields = layout.fields
    for field in fields {
        println("FIELD|" + str(field.ordinal) + "|" + field.name + "|" + field.resolved.name)
    }
    parameters = plan.parameters
    for parameter in parameters {
        println("PARAM|" + parameter.name + "|" + str(parameter.mode))
    }
    projections = plan.projections
    for projection in projections {
        println("PROJECT|" + str(projection.output_field) + "|" + str(projection.parameter) + "|" + str(projection.input_field))
    }
    return ok(null)
}
"#;

const SPAN_PLAN: &str = concat!(
    "FUNCTION|SpanMerge|Span\n",
    "FIELD|0|line|int\n",
    "FIELD|1|column|int\n",
    "FIELD|2|offset|int\n",
    "FIELD|3|end_line|int\n",
    "FIELD|4|end_column|int\n",
    "FIELD|5|end_offset|int\n",
    "PARAM|left|0\n",
    "PARAM|right|0\n",
    "PROJECT|0|0|0\n",
    "PROJECT|1|0|1\n",
    "PROJECT|2|0|2\n",
    "PROJECT|3|1|3\n",
    "PROJECT|4|1|4\n",
    "PROJECT|5|1|5\n",
);
const SPAN_TOKEN_PLAN: &str = concat!(
    "FUNCTION|SpanFromToken|Span\n",
    "FIELD|0|line|int\n",
    "FIELD|1|column|int\n",
    "FIELD|2|offset|int\n",
    "FIELD|3|end_line|int\n",
    "FIELD|4|end_column|int\n",
    "FIELD|5|end_offset|int\n",
    "PARAM|token|0\n",
    "PROJECT|0|0|3\n",
    "PROJECT|1|0|4\n",
    "PROJECT|2|0|5\n",
    "PROJECT|3|0|6\n",
    "PROJECT|4|0|7\n",
    "PROJECT|5|0|8\n",
);
const FLIPPED_PLAN: &str = concat!(
    "FUNCTION|Flip|Pair\n",
    "FIELD|0|x|int\n",
    "FIELD|1|flag|bool\n",
    "PARAM|right|1\n",
    "PARAM|left|0\n",
    "PROJECT|1|1|1\n",
    "PROJECT|0|0|0\n",
);

#[test]
fn bootstrap_checks_and_lowers_real_struct_projection_returns() {
    let work = Temp::new();
    let source = work.0.join("compiler");
    let cwd = work.0.join("invocation");
    let relocated = work.0.join("relocated");
    for directory in [&source, &cwd, &relocated] {
        fs::create_dir(directory).expect("create isolated consumer directory");
    }
    stage_sources(&source);
    let compiler = source.join("semantics");
    let entry = compiler.join("inspect_layout.ku");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    fs::write(&entry, INSPECTOR).expect("write isolated layout consumer");
    let span = include_str!("../bootstrap/stage2/span.ku").replace("\r\n", "\n");
    let supplier = include_str!("../bootstrap/stage1/token.ku");
    let position_check = |directory: &Path, source: &str, function: &str| {
        fs::write(directory.join("input.ku"), source).expect("write subject");
        fs::write(directory.join("function.txt"), function).expect("select function");
        fs::write(directory.join("supplier.ku"), supplier)
            .expect("write explicitly supplied module");
    };
    let flipped = "struct Pair { x: int flag: bool }\nfn Flip(&right: Pair, left: Pair): Pair { return Pair { flag: left.flag, x: right.x } }\nfn main() {}";
    let accepts = [
        ("SpanMerge", span.as_str(), SPAN_PLAN),
        ("SpanFromToken", span.as_str(), SPAN_TOKEN_PLAN),
        ("Flip", flipped, FLIPPED_PLAN),
    ];
    let rejects = [
        (
            "duplicate_layout_field",
            "struct Row { first: int first: int }\nfn main() {}",
        ),
        (
            "missing_literal_field",
            "struct Row { first: int second: int }\nfn Build(row: Row): Row { return Row { first: row.first } }\nfn main() {}",
        ),
        (
            "unknown_literal_field",
            "struct Row { first: int }\nfn Build(row: Row): Row { return Row { wrong: row.first } }\nfn main() {}",
        ),
        (
            "unknown_projection_field",
            "struct Row { first: int }\nfn Build(row: Row): Row { return Row { first: row.wrong } }\nfn main() {}",
        ),
        (
            "duplicate_literal_field",
            "struct Row { first: int second: int }\nfn Build(row: Row): Row { return Row { first: row.first, second: row.second, first: row.second } }\nfn main() {}",
        ),
        (
            "projection_type_mismatch",
            "struct Row { first: int second: bool }\nfn Build(row: Row): Row { return Row { first: row.second, second: row.second } }\nfn main() {}",
        ),
        (
            "duplicate_projection_parameter",
            "struct Row { first: int }\nfn Build(row: Row, row: Row): Row { return Row { first: row.first } }\nfn main() {}",
        ),
    ];

    let mut diagnostics = Vec::new();
    for (code, source) in rejects {
        position_check(&compiler, source, "Build");
        let stage0 = execute(
            Command::new(&ku)
                .args(["check", "--json"])
                .arg(compiler.join("input.ku")),
            &cwd,
            20,
        );
        let detail = stage0_detail(source, &stage0);
        diagnostics.push((
            source,
            "Build",
            format!("error|bootstrap.semantics.structs|{code}|<source>|{detail}"),
        ));
    }
    let transfer = "struct Row { text: str }\nfn Transfer(row: Row): Row { return Row { text: row.text } }\nfn main() {}";
    position_check(&compiler, transfer, "Transfer");
    assert_ok(&execute(
        Command::new(&ku)
            .arg("check")
            .arg(compiler.join("input.ku")),
        &cwd,
        20,
    ));
    let declaration = transfer.find("fn Transfer").expect("Transfer declaration");
    let declaration_end = transfer
        .find("\nfn main")
        .expect("end of Transfer declaration");
    let span_of_transfer = format!(
        "{}..{}",
        coordinates(transfer, declaration),
        coordinates(transfer, declaration_end)
    );
    diagnostics.push((transfer, "Transfer", format!(
        "error|bootstrap.semantics.structs|unsupported_aggregate_layout|<source>|projection returns require local structs containing Copy scalar fields|{span_of_transfer}"
    )));

    for (function, source, expected) in accepts {
        position_check(&compiler, source, function);
        let ran = execute(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_ok(&ran);
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout).replace("\r\n", "\n"),
            expected
        );
    }
    for (source, function, expected) in &diagnostics {
        position_check(&compiler, source, function);
        let ran = execute(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_eq!(ran.status.code(), Some(1));
        let actual = String::from_utf8_lossy(&ran.stderr);
        assert!(
            actual.contains(expected),
            "{expected} was absent from {actual}"
        );
    }

    let binary = compiler.join(format!("ku-layout{}", std::env::consts::EXE_SUFFIX));
    let built = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&binary),
        &cwd,
        120,
    );
    assert_ok(&built);
    let native = relocated.join(binary.file_name().expect("extract executable name"));
    fs::rename(binary, &native).expect("relocate native layout checker");
    fs::remove_dir_all(&source).expect("retire compiler source and staging");
    for (function, source, expected) in accepts {
        position_check(&relocated, source, function);
        let ran = execute(&mut Command::new(&native), &cwd, 20);
        assert_ok(&ran);
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout).replace("\r\n", "\n"),
            expected
        );
    }
    for (source, function, expected) in diagnostics {
        position_check(&relocated, source, function);
        let ran = execute(&mut Command::new(&native), &cwd, 20);
        assert_eq!(ran.status.code(), Some(1));
        assert!(ran.stdout.is_empty());
        assert_eq!(String::from_utf8_lossy(&ran.stderr).trim_end(), expected);
    }
}

const C_EMITTER: &str = r#"import "std.fs"
import { ParseProgram } from "../stage3/parser.ku"
import { BuildNameTable } from "./names.ku"
import { SuppliedModule } from "./imports.ku"
import { BindTypeNames } from "./type_names.ku"
import { BuildStructLayouts, BindStructReturn } from "./struct_layouts.ku"
import { EmitStructProjectionC } from "./struct_projection_c.ku"

fn main(): null! {
    named = BuildNameTable(ParseProgram(fs.read("input.ku")?)?)?
    token = BuildNameTable(ParseProgram(fs.read("supplier.ku")?)?)?
    supplied: [SuppliedModule] = [SuppliedModule { path: "../stage1/token.ku", named: token }]
    bindings = BindTypeNames(named, supplied)?
    layouts = BuildStructLayouts(named, bindings, supplied)?
    function = fs.read("function.txt")?
    plan = BindStructReturn(named, bindings, layouts, function)?
    println(str(plan.result_layout) + "|" + str(plan.function_node))
    println(EmitStructProjectionC(layouts, plan)?)
    return ok(null)
}
"#;

fn compile_projection(source: &Path, output: &Path, cwd: &Path) {
    let selected = std::env::var("KU_CC").unwrap_or_else(|_| {
        if cfg!(windows) {
            "clang".into()
        } else {
            "cc".into()
        }
    });
    let tokens: Vec<&str> = if Path::new(&selected).is_file() {
        vec![selected.as_str()]
    } else {
        selected.split_whitespace().collect()
    };
    let (program, arguments) = tokens
        .split_first()
        .expect("a C compiler for standalone projection");
    let mut compile = Command::new(program);
    compile.args(arguments);
    let stem = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    if stem.eq_ignore_ascii_case("cl") || stem.eq_ignore_ascii_case("clang-cl") {
        compile
            .args(["/nologo", "/std:c11", "/O2", "/WX"])
            .arg(source)
            .arg(format!("/Fe{}", output.display()))
            .arg(format!("/Fo{}", output.with_extension("obj").display()));
    } else {
        compile
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
    assert_ok(&execute(&mut compile, cwd, 30));
}

#[test]
fn ku_compiler_emits_standalone_c_for_real_scalar_projection() {
    let work = Temp::new();
    let source = work.0.join("compiler");
    let cwd = work.0.join("invocation");
    let relocated = work.0.join("relocated");
    for directory in [&source, &cwd, &relocated] {
        fs::create_dir(directory).expect("create isolated consumer directory");
    }
    stage_sources(&source);
    let compiler = source.join("semantics");
    let entry = compiler.join("project.ku");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    fs::write(&entry, C_EMITTER).expect("write actual Ku emitter entry");

    let token = include_str!("../bootstrap/stage1/token.ku");
    let span = include_str!("../bootstrap/stage2/span.ku");
    let flip = "struct Pair { x: int flag: bool }\nfn Flip(&right: Pair, left: Pair): Pair { return Pair { flag: left.flag, x: right.x } }\n";
    let oracle_entry = source.join("span_entry.ku");
    fs::write(
        &oracle_entry,
        r#"import { Span, SpanMerge } from "./stage2/span.ku"
fn main(): null {
    left = Span { line: 1, column: 2, offset: 3, end_line: 4, end_column: 5, end_offset: 6 }
    right = Span { line: 7, column: 8, offset: 9, end_line: 10, end_column: 11, end_offset: 12 }
    result = SpanMerge(left, right)
    println(result.line)
    println(result.column)
    println(result.offset)
    println(result.end_line)
    println(result.end_column)
    println(result.end_offset)
    return null
}
"#,
    )
    .expect("write ore source that imports real SpanMerge");
    let oracle = execute(Command::new(&ku).arg("run").arg(&oracle_entry), &cwd, 20);
    assert_ok(&oracle);
    assert_eq!(
        String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n"),
        "1\n2\n3\n10\n11\n12\n"
    );
    let flip_entry = format!(
        r#"{flip}
fn main(): null {{
    right = Pair {{ x: -5000000003, flag: false }}
    left = Pair {{ x: 42, flag: true }}
    result = Flip(right, left)
    println(result.x)
    println(result.flag)
    println(right.x)
    return null
}}
"#
    );
    fs::write(&oracle_entry, flip_entry).expect("write oracle for borrowed scalar aggregate");
    let oracle = execute(Command::new(&ku).arg("run").arg(&oracle_entry), &cwd, 20);
    assert_ok(&oracle);
    assert_eq!(
        String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n"),
        "-5000000003\ntrue\n-5000000003\n"
    );

    let cases = [
        (
            "SpanMerge",
            span,
            "1\n2\n3\n10\n11\n12\n",
            r#"    RESULT left = { 1, 2, 3, 4, 5, 6 };
    RESULT right = { 7, 8, 9, 10, 11, 12 };
    RESULT value = PROJECT(left, right);
    printf("%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n",
           value.f0, value.f1, value.f2, value.f3, value.f4, value.f5);
"#,
        ),
        (
            "Flip",
            flip,
            "-5000000003\ntrue\n-5000000003\n",
            r#"    RESULT right = { -INT64_C(5000000003), false };
    RESULT left = { INT64_C(42), true };
    RESULT value = PROJECT(&right, left);
    printf("%" PRId64 "\n%s\n%" PRId64 "\n",
           value.f0, value.f1 ? "true" : "false", right.f0);
"#,
        ),
    ];
    let materialize = |directory: &Path, function: &str, input: &str| {
        fs::write(directory.join("input.ku"), input).expect("write subject");
        fs::write(directory.join("function.txt"), function).expect("choose function");
        fs::write(directory.join("supplier.ku"), token).expect("write explicitly supplied module");
    };

    let mut expected_c = Vec::new();
    for (function, input, _, _) in cases {
        materialize(&compiler, function, input);
        let interpreted = execute(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_ok(&interpreted);
        expected_c.push(
            String::from_utf8(interpreted.stdout)
                .expect("C fragment")
                .replace("\r\n", "\n"),
        );
    }
    let seed = compiler.join(format!("ku-project-c{}", std::env::consts::EXE_SUFFIX));
    let build = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&seed),
        &cwd,
        120,
    );
    assert_ok(&build);
    let stage1 = relocated.join(seed.file_name().expect("seed executable name"));
    fs::rename(seed, &stage1).expect("relocate native Ku compiler seed");
    fs::remove_dir_all(&source).expect("remove seed source, oracle programs and staging");

    for ((function, input, expected, driver), reference) in cases.into_iter().zip(expected_c) {
        materialize(&relocated, function, input);
        let compiled = execute(&mut Command::new(&stage1), &cwd, 20);
        assert_ok(&compiled);
        let output = String::from_utf8(compiled.stdout)
            .expect("native seed output")
            .replace("\r\n", "\n");
        assert_eq!(output, reference, "emitter output differs: {function}");
        let (ids, c) = output
            .split_once('\n')
            .expect("metadata line followed by C");
        let (layout, function_id) = ids.split_once('|').expect("layout and function IDs");
        let layout: u32 = layout.parse().expect("numeric layout ID");
        let function_id: u32 = function_id.parse().expect("numeric function ID");
        assert!(layout > 0 && function_id > 0);
        assert!(!c.contains(function) && !c.contains("run_source"));
        assert!(!c.contains("KuString") && c.contains("int64_t f0;"));
        if function == "Flip" {
            assert!(c.contains("const KuStruct_"));
            assert!(c.find("result.f1 =").unwrap() < c.find("result.f0 =").unwrap());
        }
        let harness = driver
            .replace("RESULT", &format!("KuStruct_{layout}"))
            .replace("PROJECT", &format!("ku_project_{function_id}"));
        let app = work.0.join(format!("app_{function}"));
        fs::create_dir(&app).expect("create fresh C consumer directory");
        let c_source = app.join("projection.c");
        let app_binary = app.join(format!("projection{}", std::env::consts::EXE_SUFFIX));
        fs::write(&c_source, format!("{c}\n#include <inttypes.h>\n#include <stdio.h>\nint main(void) {{\n{harness}    return 0;\n}}\n"))
            .expect("write generated C plus scalar calling harness");
        compile_projection(&c_source, &app_binary, &app);
        fs::remove_file(c_source).expect("remove C source before run");
        let result = execute(&mut Command::new(app_binary), &app, 20);
        assert_ok(&result);
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).replace("\r\n", "\n"),
            expected
        );
    }
}

const TRACK_STRING_STORAGE: &str = r#"#include <assert.h>
#include <stddef.h>
#include <stdlib.h>

static void *owned[2];
static unsigned allocations;
static unsigned drops;
static void *probe_alloc(size_t size) {
    void *ptr = malloc(size);
    assert(ptr && allocations < 2);
    owned[allocations++] = ptr;
    return ptr;
}

static void probe_free(void *ptr) {
    unsigned i;
    for (i = 0; i < 2; ++i) {
        if (owned[i] && owned[i] == ptr) {
            owned[i] = NULL;
            ++drops;
            free(ptr);
            return;
        }
    }
    abort();
}

#define free probe_free
"#;

fn ownership_driver(owned_input: bool, result_type: &str, symbol: &str) -> String {
    let mut driver = r#"
#undef free
#include <inttypes.h>
#include <stdio.h>

static KuProjectionString owned_string(uint8_t byte) {
    uint8_t *ptr = probe_alloc(1);
    ptr[0] = byte;
    return (KuProjectionString){ ptr, 1, 1, KU_PROJECTION_STRING_OWNED };
}

int main(void) {
    static uint8_t permanent[] = { 's' };
    KuStruct_m1_19 token = {
        owned_string('k'), owned_string('l'), 0, 1, 2, 3, 4, 5, 6
    };
"#
    .to_string();
    if owned_input {
        driver.push_str(&format!(
            r#"    {result_type} value = {symbol}(token);
    assert(allocations == 2 && drops == 2);
    assert(owned[0] == NULL && owned[1] == NULL);

    KuProjectionString literal = {{ permanent, 1, 0, KU_PROJECTION_STRING_STATIC }};
    KuStruct_m1_19 static_token = {{ literal, literal, 0, 1, 2, 3, 4, 5, 6 }};
    {result_type} copy = {symbol}(static_token);
    assert(copy.f0 == 1 && copy.f5 == 6 && permanent[0] == 's');
    assert(drops == 2);
"#
        ));
    } else {
        // No generated drop helper is linked for View-only inputs. The
        // caller still owns its exact buffers and releases them itself.
        driver.push_str(&format!(
            r#"    uint8_t *kind = token.f0.ptr;
    uint8_t *lexeme = token.f1.ptr;
    {result_type} value = {symbol}(&token);
    assert(allocations == 2 && drops == 0 && permanent[0] == 's');
    assert(token.f0.ptr == kind && token.f1.ptr == lexeme);
    assert(kind[0] == 'k' && lexeme[0] == 'l');
    probe_free(kind);
    probe_free(lexeme);
    assert(drops == 2 && owned[0] == NULL && owned[1] == NULL);
"#
        ));
    }
    driver.push_str(
        r#"    printf("%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n%" PRId64 "\n",
        value.f0, value.f1, value.f2, value.f3, value.f4, value.f5);
    return 0;
}
"#,
    );
    driver
}

#[test]
fn bootstrap_projection_drops_owned_token_strings_and_preserves_views() {
    let work = Temp::new();
    let source = work.0.join("compiler");
    let cwd = work.0.join("invocation");
    let relocated = work.0.join("relocated");
    for directory in [&source, &cwd, &relocated] {
        fs::create_dir(directory).expect("create isolated consumer directory");
    }
    stage_sources(&source);
    let compiler = source.join("semantics");
    let entry = compiler.join("project.ku");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    fs::write(&entry, C_EMITTER).expect("write actual Ku compiler entry");

    let span = include_str!("../bootstrap/stage2/span.ku");
    let view = span.replace(
        "SpanFromToken(token: Token)",
        "SpanFromToken(&token: Token)",
    );
    assert_ne!(span, view);
    let token = include_str!("../bootstrap/stage1/token.ku");
    let materialize = |directory: &Path, input: &str| {
        fs::write(directory.join("input.ku"), input).expect("write SpanFromToken subject");
        fs::write(directory.join("function.txt"), "SpanFromToken")
            .expect("choose runtime function");
        fs::write(directory.join("supplier.ku"), token)
            .expect("write explicitly supplied Token module");
    };

    let oracle = source.join("token_entry.ku");
    fs::write(
        &oracle,
        r#"import { Token } from "./stage1/token.ku"
import { SpanFromToken } from "./stage2/span.ku"
fn main(): null! {
    token = Token { kind: "k".clone(), lexeme: "l".clone(), int_value: 0,
        line: 1, column: 2, offset: 3, end_line: 4, end_column: 5, end_offset: 6 }
    coordinates = SpanFromToken(token)
    println(coordinates.line)
    println(coordinates.column)
    println(coordinates.offset)
    println(coordinates.end_line)
    println(coordinates.end_column)
    println(coordinates.end_offset)
    return ok(null)
}
"#,
    )
    .expect("write Stage0 oracle using actual Token");
    let expected = "1\n2\n3\n4\n5\n6\n";
    let mut reference_fragments = Vec::new();
    for (owned, input) in [(true, span), (false, view.as_str())] {
        if !owned {
            fs::write(source.join("stage2/span.ku"), &view).expect("use View form in oracle");
        }
        let baseline = execute(Command::new(&ku).arg("run").arg(&oracle), &cwd, 20);
        assert_ok(&baseline);
        assert_eq!(
            String::from_utf8_lossy(&baseline.stdout).replace("\r\n", "\n"),
            expected
        );
        materialize(&compiler, input);
        let emitted = execute(Command::new(&ku).arg("run").arg(&entry), &cwd, 20);
        assert_ok(&emitted);
        reference_fragments.push(
            String::from_utf8(emitted.stdout)
                .expect("generated C")
                .replace("\r\n", "\n"),
        );
    }
    // Only the input subject uses the View variation. Restore the seed's own
    // compiler-source module before building it.
    fs::write(source.join("stage2/span.ku"), span).expect("restore seed's exact Span module");
    let seed = compiler.join(format!("ku-project-c{}", std::env::consts::EXE_SUFFIX));
    let build = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&seed),
        &cwd,
        120,
    );
    assert_ok(&build);
    let stage1 = relocated.join(seed.file_name().expect("seed executable name"));
    fs::rename(seed, &stage1).expect("relocate native Ku compiler seed");
    fs::remove_dir_all(&source).expect("remove seed source, oracle programs and staging");

    for ((owned, input), reference) in [(true, span), (false, view.as_str())]
        .into_iter()
        .zip(reference_fragments)
    {
        materialize(&relocated, input);
        let generated = execute(&mut Command::new(&stage1), &cwd, 20);
        assert_ok(&generated);
        let output = String::from_utf8(generated.stdout)
            .expect("C fragment")
            .replace("\r\n", "\n");
        assert_eq!(output, reference);
        let (ids, c) = output.split_once('\n').expect("layout metadata and C");
        let (layout, function_id) = ids.split_once('|').expect("numeric identity components");
        let layout: u32 = layout.parse().expect("numeric result layout");
        let function_id: u32 = function_id.parse().expect("numeric function ID");
        assert_eq!(
            c.matches("ku_projection_string_drop(&a0.").count(),
            if owned { 2 } else { 0 }
        );
        if owned {
            assert!(c.contains("ku_projection_string_drop(&a0.f0);"));
            assert!(c.contains("ku_projection_string_drop(&a0.f1);"));
            assert!(
                c.find("result.f5 =").unwrap()
                    < c.find("ku_projection_string_drop(&a0.f0);").unwrap()
            );
        } else {
            assert!(c.contains("const KuStruct_m1_19 *a0") && !c.contains("free("));
        }
        let mode = if owned { "owned" } else { "view" };
        let application = work.0.join(mode);
        fs::create_dir(&application).expect("fresh C consumer directory");
        let c_source = application.join("projection.c");
        let binary = application.join(format!("projection{}", std::env::consts::EXE_SUFFIX));
        let driver = ownership_driver(
            owned,
            &format!("KuStruct_{layout}"),
            &format!("ku_project_{function_id}"),
        );
        fs::write(&c_source, format!("{TRACK_STRING_STORAGE}{c}\n{driver}"))
            .expect("write generated C and owning bridge");
        compile_projection(&c_source, &binary, &application);
        fs::remove_file(c_source).expect("remove native consumer source before run");
        let result = execute(&mut Command::new(binary), &application, 20);
        assert_ok(&result);
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).replace("\r\n", "\n"),
            expected
        );
    }
}

#[test]
fn bootstrap_compiler_entry_reads_real_module_beyond_legacy_script_limit() {
    let work = Temp::new();
    let source = work.0.join("compiler");
    let relocated = work.0.join("relocated");
    for directory in [&source, &relocated] {
        fs::create_dir(directory).expect("create isolated compiler consumer");
    }
    stage_sources(&source);
    let compiler = source.join("semantics");
    let entry = compiler.join("module-window.ku");
    fs::write(
        &entry,
        r#"import "std.fs"
import { Scan } from "../stage1/lexer.ku"
import { AstCanonical } from "../stage2/ast.ku"
import { ParseModule, ParseProgram } from "../stage3/parser.ku"
import { ReadCompilerFunction, ReadFunction } from "../stage3/functions.ku"
import { ReadSignatureTypePrefix } from "../stage3/signature.ku"
import { ParseOutput } from "../stage2/ast.ku"

fn UnreachableExpression(start: int, end: int): ParseOutput! {
    fail { domain: "bootstrap.test", code: "invalid_callback", message: "a malformed function interval reached the expression reader" }
}

fn CheckFence(source: str, boundary: int, detail: str): null! {
    tokens = Scan(source)?
    next_function = tokens[boundary].clone()
    if (next_function.kind != "Fn") { panic("fixture must place the next fn at the boundary") }
    caught = false
    try { ReadCompilerFunction(tokens, 0, boundary, UnreachableExpression)? } catch(err) {
        position = str(next_function.line) + ":" + str(next_function.column) + "@" + str(next_function.offset)
        caught = err.code == "unexpected_token" && err.message.contains(detail + "|" + position + "..")
    }
    if (!caught) { panic("function interval did not see its EOF fence") }
    following = ReadCompilerFunction(tokens, boundary, tokens.len() - 1, UnreachableExpression)?
    if (following.consumed != 6 || following.output.root != 1) { panic("function interval changed its neighbor") }
    return ok(null)
}

fn main(): null! {
    module_source = fs.read("large.ku")?
    tokens = Scan(module_source.clone())?
    if (tokens.len() <= 512) { panic("fixture must exceed the script token limit") }
    caught = false
    try { ParseProgram(module_source.clone())? } catch(err) { caught = err.code == "invalid_token_stream" }
    if (!caught) { panic("script token limit changed") }
    parsed = ParseModule(module_source)?
    if (parsed.root != 454) { panic("context.ku AST changed") }
    small = fs.read("small.ku")?
    if (AstCanonical(ParseProgram(small.clone())?) != AstCanonical(ParseModule(small)?)) {
        panic("module mode changes the AST of a small source file")
    }
    long_function = fs.read("oversized.ku")?
    long_tokens = Scan(long_function.clone())?
    caught = false
    try { ReadFunction(long_tokens, 0, UnreachableExpression)? } catch(err) { caught = err.message.contains("function window accepts at most 512 tokens") }
    if (!caught) { panic("public function token limit changed") }
    long_parsed = ParseModule(long_function)?
    if (long_parsed.root != 446) { panic("long function and following function AST changed") }
    structs = ParseModule(fs.read("real-structs.ku")?)?
    if (structs.root != 830 || structs.arena.edges.len() != 829) { panic("structs.ku AST changed") }
    enums = ParseModule(fs.read("real-enums.ku")?)?
    if (enums.root != 1002 || enums.arena.edges.len() != 1001) { panic("enums.ku AST changed") }
    count = false
    try { ParseModule(fs.read("over-statements.ku")?)? } catch(err) { count = err.code == "statement_limit" }
    if (!count) { panic("compiler function statement limit changed") }
    type_tokens = Scan(fs.read("over-type.ku")?)?
    capped = false
    try { ReadSignatureTypePrefix(type_tokens, 0, type_tokens.len() - 1)? } catch(err) { capped = err.message.contains("type window accepts at most 512 tokens") }
    if (!capped) { panic("compiler type prefix limit changed") }
    CheckFence("fn Broken(op: fn(int)\nfn Following() {}", 9, "expected ':' before function type return")?
    CheckFence("fn Broken(){ if (true fn Following() {}", 8, "expected ')' after if condition")?
    println("module-window-ok")
    return ok(null)
}
"#,
    )
    .expect("write compiler module-window witness");

    for destination in [&compiler, &relocated] {
        fs::write(
            destination.join("large.ku"),
            include_str!("../bootstrap/stage2/context.ku"),
        )
        .expect("write actual bootstrap module");
        fs::write(
            destination.join("small.ku"),
            include_str!("../bootstrap/stage2/span.ku"),
        )
        .expect("write smaller bootstrap module");
        fs::write(
            destination.join("oversized.ku"),
            format!(
                "fn Test() {{\n{}\n}}\nfn Following(): int {{ return 7 }}",
                "value = 1 + 2\n".repeat(110)
            ),
        )
        .expect("write oversized function witness");
        fs::write(
            destination.join("real-structs.ku"),
            include_str!("../bootstrap/stage3/structs.ku"),
        )
        .expect("write actual struct reader");
        fs::write(
            destination.join("real-enums.ku"),
            include_str!("../bootstrap/stage3/enums.ku"),
        )
        .expect("write actual enum reader");
        fs::write(
            destination.join("over-statements.ku"),
            format!("fn TooMany() {{\n{}\n}}", "value = 1 + 2\n".repeat(129)),
        )
        .expect("write compiler statement limit witness");
        fs::write(
            destination.join("over-type.ku"),
            format!("A{}", ".A".repeat(256)),
        )
        .expect("write compiler type prefix limit witness");
    }

    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let interpreted = execute(Command::new(&ku).arg("run").arg(&entry), &work.0, 30);
    assert_ok(&interpreted);
    assert_eq!(
        String::from_utf8_lossy(&interpreted.stdout).trim(),
        "module-window-ok"
    );

    let executable = compiler.join(format!("module-window{}", std::env::consts::EXE_SUFFIX));
    let built = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&executable),
        &work.0,
        120,
    );
    assert_ok(&built);
    let native = relocated.join(executable.file_name().expect("native file name"));
    fs::rename(executable, &native).expect("relocate module-window seed");
    fs::remove_dir_all(source).expect("remove moved seed's source and build directory");
    let output = execute(&mut Command::new(native), &work.0, 30);
    assert_ok(&output);
    assert_eq!(output.stdout, interpreted.stdout);
}

const NESTED_LAYOUT_EMITTER: &str = r#"import "std.fs"
import { ParseModule } from "../stage3/parser.ku"
import { BuildNameTable } from "./names.ku"
import { SuppliedModule } from "./imports.ku"
import { BindTypeNames } from "./type_names.ku"
import { BuildStructLayouts } from "./struct_layouts.ku"
import { CTypeName } from "./struct_projection_c.ku"
import { OrderCStructLayouts, EmitNestedCLayouts } from "./nested_struct_c.ku"

fn main(): null! {
    named = BuildNameTable(ParseModule(fs.read("input.ku")?)?)?
    spans = BuildNameTable(ParseModule(fs.read("span.ku")?)?)?
    tokens = BuildNameTable(ParseModule(fs.read("token.ku")?)?)?
    supplied: [SuppliedModule] = [
        SuppliedModule { path: "./span.ku", named: spans },
        SuppliedModule { path: "../stage1/token.ku", named: tokens }
    ]
    bindings = BindTypeNames(named, supplied)?
    layouts = BuildStructLayouts(named, bindings, supplied)?
    root = fs.read("type.txt")?
    index = 0
    while (index < named.names.declarations.len()) {
        item = named.names.declarations[index].clone()
        if (item.kind == "Struct" && item.name == root) {
            order = OrderCStructLayouts(layouts, 0, item.node, item.span)?
            println(CTypeName(0, item.node))
            println(EmitNestedCLayouts(layouts, order)?)
            return ok(null)
        }
        index = index + 1
    }
    panic("no selected layout")
    return ok(null)
}
"#;

#[test]
fn native_seed_emits_nested_c_for_real_parse_context() {
    let tmp = Temp::new();
    let copied = tmp.0.join("source");
    let compiler = copied.join("semantics");
    let cwd = tmp.0.join("consumer");
    fs::create_dir(&copied).expect("create isolated stage source");
    fs::create_dir(&cwd).expect("create isolated layout invocation");
    stage_sources(&copied);
    let entry = compiler.join("nested_layout.ku");
    fs::write(&entry, NESTED_LAYOUT_EMITTER).expect("write C layout emitter fixture");
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let executable = format!("nested-layout{}", std::env::consts::EXE_SUFFIX);
    let built = compiler.join(&executable);
    let seed = cwd.join(executable);
    let build = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&built),
        &cwd,
        150,
    );
    assert_ok(&build);
    fs::rename(built, &seed).expect("relocate installed-build native layout compiler");
    let driver_name = format!("ku-project{}", std::env::consts::EXE_SUFFIX);
    let driver_built = compiler.join(&driver_name);
    let driver_binary = cwd.join(driver_name);
    let build = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(compiler.join("projection_main.ku"))
            .arg("-o")
            .arg(&driver_built),
        &cwd,
        150,
    );
    assert_ok(&build);
    fs::rename(driver_built, &driver_binary).expect("relocate the built project CLI");
    fs::remove_dir_all(copied).expect("retire compiler design sources before emission");

    let span_source = include_str!("../bootstrap/stage2/span.ku");
    let token_source = include_str!("../bootstrap/stage1/token.ku");
    let run = |input: &str, root: &str| {
        fs::write(cwd.join("input.ku"), input).expect("write layout subject");
        fs::write(cwd.join("span.ku"), span_source).expect("write real supplied Span");
        fs::write(cwd.join("token.ku"), token_source).expect("write real supplied Token");
        fs::write(cwd.join("type.txt"), root).expect("select required aggregate");
        execute(&mut Command::new(&seed), &cwd, 30)
    };
    let contexts = include_str!("../bootstrap/stage2/context.ku");
    let cases = [
        (
            contexts,
            "ParseContext",
            r#"
#include <stdio.h>
int main(void) {
    RESULT value = {0};
    value.f0 = (KuProjectionString){0};
    value.f1.f0 = INT64_C(1);
    value.f1.f1 = INT64_C(2);
    value.f2 = value.f1;
    value.f2.f1 = INT64_C(3);
    value.f3 = false;
    _Static_assert(sizeof(value.f1) == 6 * sizeof(int64_t), "Span size");
    _Static_assert(offsetof(RESULT, f2) == offsetof(RESULT, f1) + sizeof(value.f1), "adjacent spans");
    if (value.f0.storage != KU_PROJECTION_STRING_STATIC ||
        value.f1.f0 != 1 || value.f1.f1 != 2 ||
        value.f2.f0 != 1 || value.f2.f1 != 3 || value.f3) return 1;
    puts("context");
    return 0;
}
"#,
            "context",
        ),
        (
            "import { Span as External } from \"./span.ku\"\n\
             struct Root { pair: Pair copied: External flag: bool }\n\
             struct Pair { here: Span remote: External }\n\
             struct Span { marker: bool }\n",
            "Root",
            r#"
#include <stdio.h>
int main(void) {
    RESULT value = {0};
    value.f0.f0.f0 = true;
    value.f0.f1.f0 = INT64_C(42);
    value.f1 = value.f0.f1;
    value.f2 = false;
    if (!value.f0.f0.f0 || value.f1.f0 != 42 || value.f2) return 1;
    puts("aliases");
    return 0;
}
"#,
            "aliases",
        ),
    ];
    for (source, root, driver, expected) in cases {
        let generated = run(source, root);
        assert_ok(&generated);
        let generated = String::from_utf8(generated.stdout).expect("C output");
        let (symbol, c) = generated
            .split_once('\n')
            .expect("numeric C symbol then layouts");
        let symbol = symbol.trim();
        assert!(symbol
            .strip_prefix("KuStruct_")
            .is_some_and(|id| { !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) }));
        assert!(
            c.matches("} KuStruct_m1_").count() == 1,
            "one Span definition: {c}"
        );
        assert!(
            !c.contains("KuStruct_m2_"),
            "unreachable Token layout must not appear"
        );
        assert!(!c.contains("malloc(") && !c.contains("free("));
        let c_path = cwd.join("ctx.c");
        fs::write(&c_path, format!("{c}{}", driver.replace("RESULT", symbol)))
            .expect("independent C input");
        for file in ["input.ku", "span.ku", "token.ku", "type.txt"] {
            fs::remove_file(cwd.join(file)).expect("remove Ku subject before C build");
        }
        let app = cwd.join(format!("layout-app{}", std::env::consts::EXE_SUFFIX));
        compile_projection(&c_path, &app, &cwd);
        let output = execute(&mut Command::new(&app), &cwd, 10);
        assert_ok(&output);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    }

    let cycle = "struct Root { child: A }\nstruct A { next: B }\nstruct B { last: A }\n";
    let repeated = run(cycle, "Root");
    assert!(!repeated.status.success() && repeated.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&repeated.stderr);
    assert!(
        stderr.contains("recursive_nested_layout|<source>|recursive by-value struct field 'last'|")
    );
    let start = cycle.rfind("A }").expect("cycle edge type");
    assert!(stderr.contains(&format!(
        "{}..{}",
        coordinates(cycle, start),
        coordinates(cycle, start + 1)
    )));
    let unsupported = run("enum E { Item }\nstruct Root { field: E }", "Root");
    assert!(!unsupported.status.success() && unsupported.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unsupported.stderr).contains("unsupported_nested_layout"));
    for (steps, duplicate, code) in [
        (8, false, "nested_layout_depth"),
        (7, true, "nested_layout_fields"),
    ] {
        let mut source = "struct S0 { marker: int }\n".to_owned();
        for i in 1..=steps {
            let right = if duplicate {
                format!(" right: S{}", i - 1)
            } else {
                String::new()
            };
            source.push_str(&format!("struct S{i} {{ left: S{}{right} }}\n", i - 1));
        }
        let rejected = run(&source, &format!("S{steps}"));
        assert!(
            !rejected.status.success() && rejected.stdout.is_empty(),
            "{source}"
        );
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(code));
    }

    let oracle = cwd.join("oracle");
    fs::create_dir_all(oracle.join("stage1")).expect("create original token source directory");
    fs::create_dir_all(oracle.join("stage2")).expect("create original context source directory");
    fs::write(oracle.join("stage1/token.ku"), token_source).expect("write real oracle Token");
    fs::write(oracle.join("stage2/span.ku"), span_source).expect("write real oracle Span");
    let original = oracle.join("stage2/context.ku");
    fs::write(&original, format!("{contexts}{}", r#"
fn main() {
    value = DefaultParseContext()
    println(value.diagnostic_domain)
    println(str(value.origin.line) + " " + str(value.origin.column) + " " + str(value.origin.offset) + " " + str(value.origin.end_line) + " " + str(value.origin.end_column) + " " + str(value.origin.end_offset))
    println(str(value.boundary.line) + " " + str(value.boundary.column) + " " + str(value.boundary.offset) + " " + str(value.boundary.end_line) + " " + str(value.boundary.end_column) + " " + str(value.boundary.end_offset))
    println(value.has_boundary)
}
"#)).expect("write real context with observation entry");
    let expected = execute(Command::new(&ku).arg("run").arg(original), &cwd, 30);
    assert_ok(&expected);
    let expected = String::from_utf8(expected.stdout)
        .expect("Ku oracle output")
        .replace("\r\n", "\n");
    assert_eq!(
        expected.trim(),
        "bootstrap.parser\n1 1 0 1 1 0\n1 1 0 1 1 0\nfalse"
    );
    fs::remove_dir_all(oracle).expect("remove oracle sources before emitted C build");

    for directory in ["stage1", "stage2"] {
        fs::create_dir(cwd.join(directory)).expect("create manifest source directory");
    }
    let target = cwd.join("stage2/context.ku");
    fs::write(&target, contexts).expect("use original unmodified context module");
    fs::write(cwd.join("stage2/span.ku"), span_source).expect("use original span module");
    fs::write(cwd.join("stage1/token.ku"), token_source).expect("use original token module");
    fs::write(cwd.join("mode.txt"), "construct\n").expect("select constructor mode");
    fs::write(cwd.join("function.txt"), "DefaultParseContext").expect("select real constructor");
    let manifest = cwd.join("modules.txt");
    fs::write(
        &manifest,
        "stage2/context.ku\r\nstage2/span.ku\r\nstage1/token.ku\r\n",
    )
    .expect("use project-relative paths in manifest order");
    for flat_fixture in ["input.ku", "span.ku", "token.ku", "type.txt"] {
        fs::remove_file(cwd.join(flat_fixture)).expect("leave only manifest input for the CLI");
    }
    let emitted = execute(&mut Command::new(&driver_binary), &cwd, 30);
    assert_ok(&emitted);
    let c = String::from_utf8(emitted.stdout).expect("C text");
    let (result_type, declaration) = c
        .lines()
        .find_map(|line| {
            let parts = line.split_once(' ')?;
            parts.1.starts_with("ku_construct_").then_some(parts)
        })
        .expect("typed standalone constructor definition");
    let symbol = declaration
        .strip_suffix("(void) {")
        .expect("parameterless constructor");
    assert!(symbol
        .strip_prefix("ku_construct_")
        .is_some_and(|id| { !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) }));
    assert!(!c.contains("malloc(") && !c.contains("free(") && !c.contains("clone"));
    let driver = r#"
#include <stdio.h>
#include <inttypes.h>
int main(void) {
    RESULT value = CONSTRUCTOR();
    if (value.f0.storage != KU_PROJECTION_STRING_STATIC || value.f0.capacity != 0) return 1;
    printf("%.*s\n", (int)value.f0.len, (const char*)value.f0.ptr);
    printf("%" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 "\n",
           value.f1.f0, value.f1.f1, value.f1.f2, value.f1.f3, value.f1.f4, value.f1.f5);
    printf("%" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 "\n",
           value.f2.f0, value.f2.f1, value.f2.f2, value.f2.f3, value.f2.f4, value.f2.f5);
    puts(value.f3 ? "true" : "false");
    value.f1.f0 = 77;
    return value.f2.f0 != 1;
}
"#;
    let c_path = cwd.join("create.c");
    fs::write(
        &c_path,
        format!(
            "{c}{}",
            driver
                .replace("RESULT", result_type)
                .replace("CONSTRUCTOR", symbol)
        ),
    )
    .expect("write C constructor and independent harness");
    let app = cwd.join(format!("create-app{}", std::env::consts::EXE_SUFFIX));
    fs::rename(cwd.join("stage1"), cwd.join("private-stage1")).expect("hide supplied token source");
    fs::rename(cwd.join("stage2"), cwd.join("private-stage2"))
        .expect("hide supplied stage2 sources");
    compile_projection(&c_path, &app, &cwd);
    let observed = execute(&mut Command::new(app), &cwd, 10);
    assert_ok(&observed);
    assert_eq!(
        String::from_utf8(observed.stdout)
            .expect("C output")
            .replace("\r\n", "\n"),
        expected
    );

    fs::rename(cwd.join("private-stage1"), cwd.join("stage1"))
        .expect("restore token source for error cases");
    fs::rename(cwd.join("private-stage2"), cwd.join("stage2"))
        .expect("restore stage2 sources for error cases");
    for (paths, code) in [
        (
            "stage2/span.ku\nstage2/context.ku\nstage1/token.ku\n",
            "unknown_constructor_function",
        ),
        ("stage2/context.ku\nstage2/context.ku\n", "ambiguous_module"),
        ("stage2/context.ku\nstage1/token.ku\n", "unresolved_import"),
    ] {
        fs::write(&manifest, paths).expect("write manifest error case");
        let rejected = execute(&mut Command::new(&driver_binary), &cwd, 30);
        assert!(!rejected.status.success() && rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(code));
    }
    fs::write(cwd.join("function.txt"), "Make").expect("select rejection case");
    fs::write(&manifest, "stage2/context.ku").expect("select single module for rejection case");
    fs::write(
        target,
        r#"
struct Owned { text: str }
struct Outer { value: Owned }
fn Make(): Outer {
    item = Owned { text: "alive" }
    return Outer { value: item.clone() }
}
"#,
    )
    .expect("write owned local rejection case");
    let rejected = execute(&mut Command::new(&driver_binary), &cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("noncopy_constructor_local"));
    assert_guarded_context(&ku, &driver_binary, &cwd, contexts);
}

fn assert_guarded_context(ku: &Path, seed: &Path, cwd: &Path, context: &str) {
    let manifest = cwd.join("modules.txt");
    fs::write(
        &manifest,
        "stage2/context.ku\nstage2/span.ku\nstage1/token.ku\n",
    )
    .expect("read the real modules through the project entry");
    let source = cwd.join("stage2/context.ku");
    let reference = r#"
fn GuardEof(line: int, column: int, offset: int): Token {
    return Token { kind: "Eof", lexeme: "", int_value: 0, line: line, column: column, offset: offset,
                   end_line: line, end_column: column, end_offset: offset }
}
fn GuardContext(origin_line: int, boundary_line: int, boundary_column: int): ParseContext {
    return ParseContext { diagnostic_domain: "test", has_boundary: true,
        origin: Span { line: origin_line, column: 7, offset: 10, end_line: origin_line, end_column: 7, end_offset: 10 },
        boundary: Span { line: boundary_line, column: boundary_column, offset: 12,
                         end_line: boundary_line, end_column: boundary_column, end_offset: 12 } }
}
fn main() {
    no_boundary = GuardContext(2, 2, 9)
    no_boundary.has_boundary = false
    println("0|" + ParseContextError(3, 3, GuardEof(9223372036854775807, 3, 2), no_boundary))
    println("1|" + ParseContextError(3, 3, GuardEof(1, 3, 2), GuardContext(0, 2, 9)))
    println("2|" + ParseContextError(9223372036854775807, 3, GuardEof(1, 3, 2), GuardContext(2, 2, 9)))
    println("3|" + ParseContextError(3, 3, GuardEof(1, 3, 2), GuardContext(2, 2, 9)))
    println("4|" + ParseContextError(3, 3, GuardEof(1, 3, 2), GuardContext(2, 2, 8)))
    println("5|" + ParseContextError(3, 3, GuardEof(2, 3, 2), GuardContext(2, 3, 3)))
}
"#;
    fs::write(&source, format!("{context}{reference}"))
        .expect("trace the original guard with Stage0");
    let oracle = execute(Command::new(ku).arg("run").arg(&source), cwd, 30);
    assert_ok(&oracle);
    let expected = String::from_utf8(oracle.stdout)
        .expect("oracle output")
        .replace("\r\n", "\n");
    assert_eq!(expected, concat!(
        "0|\n",
        "1|parse origin must use positive ordered line/column coordinates and a non-negative byte offset\n",
        "2|parse origin cannot be relocated without integer overflow\n",
        "3|\n",
        "4|parse boundary must be an ordered absolute span at or after the source window\n",
        "5|\n",
    ));

    fs::write(&source, context).expect("restore the unmodified project source");
    fs::write(cwd.join("mode.txt"), "guarded").expect("select guarded code generation");
    fs::write(cwd.join("function.txt"), "ParseContextError").expect("select the original guard");
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).expect("generated C");
    let declaration = code
        .lines()
        .find(|line| line.starts_with("KuProjectionString ku_guarded_"))
        .expect("guard ABI definition");
    let (symbol, params) = declaration
        .trim_start_matches("KuProjectionString ")
        .split_once('(')
        .expect("emitted signature");
    let params: Vec<_> = params.trim_end_matches(") {").split(", ").collect();
    let token = params[2]
        .strip_suffix(" a2")
        .expect("Token is the third parameter");
    let context = params[3]
        .strip_suffix(" a3")
        .expect("ParseContext is the fourth parameter");
    let probes = r#"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
static void *owned[3];
static int remaining;
static void counted_drop(void *pointer) {
    for (int i = 0; i < 3; ++i) {
        if (owned[i] == pointer && pointer) {
            owned[i] = NULL;
            --remaining;
            free(pointer);
            return;
        }
    }
    abort();
}
static void guard_finished(void) {
    if (remaining != 0) _Exit(97);
}
#define free counted_drop
"#;
    let main = r#"
#undef free
static KuProjectionString own(int index) {
    assert(owned[index] == NULL);
    uint8_t *ptr = malloc(1);
    assert(ptr);
    *ptr = 'x';
    owned[index] = ptr;
    ++remaining;
    return (KuProjectionString){ ptr, 1, 1, KU_PROJECTION_STRING_OWNED };
}
static KuProjectionString invoke(int variant, int64_t eof_line, int64_t characters) {
    TOKEN token = { own(0), own(1), 0, eof_line, 3, 2, eof_line, 3, 2 };
    CTX context = {0};
    context.f0 = own(2);
    context.f1.f0 = variant == 1 ? 0 : 2;
    context.f1.f1 = 7;
    context.f1.f2 = 10;
    context.f1.f3 = context.f1.f0;
    context.f1.f4 = 7;
    context.f1.f5 = 10;
    context.f2.f0 = variant == 5 ? 3 : 2;
    context.f2.f1 = variant == 5 ? 3 : variant == 4 ? 8 : 9;
    context.f2.f2 = 12;
    context.f2.f3 = context.f2.f0;
    context.f2.f4 = context.f2.f1;
    context.f2.f5 = 12;
    context.f3 = variant != 0;
    return FUNCTION(characters, 3, token, context);
}
int main(int argc, char **argv) {
    (void)argv;
    assert(atexit(guard_finished) == 0);
    if (argc > 1) {
        (void)invoke(3, INT64_MAX, 3);
        return 98;
    }
    for (int i = 0; i < 6; ++i) {
        int64_t eof_line = i == 0 ? INT64_MAX : i == 5 ? 2 : 1;
        KuProjectionString result = invoke(i, eof_line, i == 2 ? INT64_MAX : 3);
        assert(remaining == 0 && result.storage == KU_PROJECTION_STRING_STATIC && result.capacity == 0);
        printf("%d|%.*s\n", i, (int)result.len, (const char*)result.ptr);
    }
    return 0;
}
"#;
    let consumer = cwd.join("guard.c");
    fs::write(
        &consumer,
        format!(
            "{probes}{code}{}",
            main.replace("TOKEN", token)
                .replace("CTX", context)
                .replace("FUNCTION", symbol)
        ),
    )
    .expect("write independent consumer and ownership probes");
    let borrowed = emit_guarded_view(seed, cwd, &source, &manifest);
    for family in ["stage1", "stage2"] {
        fs::rename(cwd.join(family), cwd.join(format!("guarded-{family}")))
            .expect("hide Ku input from C consumer");
    }
    let binary = cwd.join(format!("guarded-context{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&consumer, &binary, cwd);
    let result = execute(&mut Command::new(&binary), cwd, 10);
    assert_ok(&result);
    assert_eq!(
        String::from_utf8(result.stdout)
            .expect("C output")
            .replace("\r\n", "\n"),
        expected
    );
    let trap = execute(Command::new(&binary).arg("overflow"), cwd, 10);
    assert_eq!(trap.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&trap.stderr).contains("integer overflow"));
    let view_binary = cwd.join(format!("guarded-view{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&borrowed, &view_binary, cwd);
    assert_ok(&execute(&mut Command::new(&view_binary), cwd, 10));
    let trap = execute(Command::new(&view_binary).arg("evaluate"), cwd, 10);
    assert_eq!(trap.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&trap.stderr).contains("integer overflow"));
}

fn emit_guarded_view(seed: &Path, cwd: &Path, source: &Path, manifest: &Path) -> PathBuf {
    fs::write(manifest, "stage2/context.ku").expect("select a single module");
    fs::write(
        source,
        r#"
struct Point { label: str high: int }
struct Outer { point: Point }
fn Read(&value: Outer, skip: bool): str {
    ceiling: int = 9223372036854775807
    if (skip || value.point.high + 1 > ceiling) { return "kept" }
    return "unexpected"
}
"#,
    )
    .expect("write typed local and borrowed short circuit");
    fs::write(cwd.join("function.txt"), "Read").expect("select View function");
    let output = execute(&mut Command::new(seed), cwd, 15);
    assert_ok(&output);
    let code = String::from_utf8(output.stdout).expect("generated View C");
    assert!(!code.contains("ku_projection_string_drop"));
    let declaration = code
        .lines()
        .find(|line| line.starts_with("KuProjectionString ku_guarded_"))
        .expect("View signature");
    let (symbol, params) = declaration
        .trim_start_matches("KuProjectionString ")
        .split_once('(')
        .expect("View signature");
    let view_type = params
        .strip_prefix("const ")
        .and_then(|tail| tail.split_once('*'))
        .map(|(ty, _)| ty)
        .expect("const View pointer");
    let main = r#"
#include <assert.h>
#include <string.h>
int main(int argc, char **argv) {
    (void)argv;
    VIEW value = {0};
    static uint8_t stable = 'x';
    uint8_t *ptr = argc == 1 ? malloc(1) : &stable;
    assert(ptr);
    *ptr = 'x';
    value.f0.f0 = argc == 1 ? (KuProjectionString){ ptr, 1, 1, KU_PROJECTION_STRING_OWNED }
                           : (KuProjectionString){ ptr, 1, 0, KU_PROJECTION_STRING_STATIC };
    value.f0.f1 = INT64_MAX;
    KuProjectionString result = FUNCTION(&value, argc == 1);
    assert(result.len == 4 && !memcmp(result.ptr, "kept", 4));
    assert(value.f0.f0.ptr == ptr && *ptr == 'x');
    free(ptr);
    return 0;
}
"#;
    let path = cwd.join("guarded-view.c");
    fs::write(
        &path,
        format!(
            "{code}{}",
            main.replace("VIEW", view_type).replace("FUNCTION", symbol)
        ),
    )
    .expect("write standalone View consumer");
    path
}

const ARRAY_OWNER_EMITTER: &str = r#"import "std.fs"
import { BindProjectImports } from "./imports.ku"
import { BindImportedTypeNames } from "./type_names.ku"
import { BuildStructLayouts } from "./struct_layouts.ku"
import { ReadProjectModules } from "./project_manifest.ku"
import { EmitReturnedStructArrayC } from "./struct_array_c.ku"

fn main(): null! {
    supplied = ReadProjectModules(["stage2/context.ku", "stage2/span.ku", "stage1/token.ku"])?
    imports = BindProjectImports(supplied[0], supplied)?
    bound = BindImportedTypeNames(supplied[0].named, imports, supplied)?
    layouts = BuildStructLayouts(supplied[0].named, bound, supplied)?
    c = EmitReturnedStructArrayC(supplied[0].named, bound, layouts, fs.read("target.txt")?)?
    println(c.element_type + "|" + c.array_type + "|" + c.prefix)
    print(c.source)
    return ok(null)
}
"#;

const CHECKED_ARRAY_ALLOCATOR: &str = r#"
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static void *live[16];
static int attempts;
static int fail_after = -1;
static int slot_for(void *pointer) {
    for (int i = 0; i < 16; ++i) if (live[i] == pointer) return i;
    abort();
}
static int inject_failure(void) {
    ++attempts;
    if (fail_after == 0) return 1;
    if (fail_after > 0) --fail_after;
    return 0;
}
static void *test_malloc(size_t size) {
    if (inject_failure()) return NULL;
    size_t slot = (size_t)slot_for(NULL);
    void *result = malloc(size);
    assert(result);
    live[slot] = result;
    return result;
}
static void *test_realloc(void *pointer, size_t size) {
    if (inject_failure()) return NULL;
    size_t slot = (size_t)slot_for(pointer);
    void *result = realloc(pointer, size);
    assert(result);
    live[slot] = result;
    return result;
}
static void test_free(void *pointer) {
    if (!pointer) return;
    live[slot_for(pointer)] = NULL;
    free(pointer);
}
static int outstanding(void) {
    int count = 0;
    for (int i = 0; i < 16; ++i) count += live[i] != NULL;
    return count;
}
#define malloc test_malloc
#define realloc test_realloc
#define free test_free
"#;

const TOKEN_OWNER_DRIVER: &str = r#"
typedef struct { size_t len; __ELEMENT__ *data; size_t capacity; } Stage0ArrayHeader;
_Static_assert(sizeof(__ARRAY__) == sizeof(Stage0ArrayHeader), "Stage0 array ABI size");
_Static_assert(offsetof(__ARRAY__, data) == offsetof(Stage0ArrayHeader, data), "Stage0 array data");
_Static_assert(offsetof(__ARRAY__, capacity) == offsetof(Stage0ArrayHeader, capacity), "Stage0 array capacity");

static KuProjectionString string_owner(const uint8_t *text, size_t len) {
    uint8_t *buf = malloc(len ? len : 1);
    if (len) memcpy(buf, text, len);
    return (KuProjectionString){ buf, len, len ? len : 1, KU_PROJECTION_STRING_OWNED };
}
static void print_token(const __ELEMENT__ *value) {
    printf("%.*s|%.*s|%lld|%lld|%lld|%lld|%lld|%lld|%lld\n",
        (int)value->f0.len, (char*)value->f0.ptr, (int)value->f1.len, (char*)value->f1.ptr,
        (long long)value->f2, (long long)value->f3, (long long)value->f4, (long long)value->f5,
        (long long)value->f6, (long long)value->f7, (long long)value->f8);
}
int main(void) {
    static uint8_t spelling[] = {0xe4,0xbb,0xbb,0xe5,0x8a,0xa1,0xf0,0x9f,0x99,0x82};
    __ARRAY__ tokens = {0};
    __ELEMENT__ owner = {
        string_owner((const uint8_t*)"String", 6), string_owner(spelling, sizeof(spelling)),
        0, 1, 8, 7, 1, 13, 19
    };
    // Receive the legacy representation whose storage has exactly len slots.
    tokens.data = malloc(sizeof(*tokens.data));
    tokens.len = 1;
    tokens.data[0] = owner;
    owner = (__ELEMENT__){0};
    assert(outstanding() == 3 && !tokens.capacity);

    __ELEMENT__ clone = {0};
    fail_after = 1;
    assert(__PREFIX___clone_at(&tokens, 0, &clone) == KU_STRUCT_ARRAY_OOM);
    fail_after = -1;
    assert(outstanding() == 3 && !clone.f0.ptr && !clone.f1.ptr);
    assert(__PREFIX___clone_at(&tokens, -1, &clone) == KU_STRUCT_ARRAY_BOUNDS);
    assert(__PREFIX___clone_at(&tokens, INT64_MAX, &clone) == KU_STRUCT_ARRAY_BOUNDS);
    assert(__PREFIX___clone_at(&tokens, 0, &tokens.data[0]) == KU_STRUCT_ARRAY_INVALID);
    assert(__PREFIX___append_move(&tokens, &tokens.data[0]) == KU_STRUCT_ARRAY_INVALID);

    assert(__PREFIX___clone_at(&tokens, 0, &clone) == KU_STRUCT_ARRAY_OK);
    uint8_t *cloned_body = clone.f1.ptr;
    assert(outstanding() == 5 && cloned_body != tokens.data[0].f1.ptr);
    __ELEMENT__ *old_storage = tokens.data;
    fail_after = 0;
    assert(__PREFIX___append_move(&tokens, &clone) == KU_STRUCT_ARRAY_OOM);
    fail_after = -1;
    assert(tokens.data == old_storage && tokens.len == 1 && !tokens.capacity);
    assert(clone.f1.ptr == cloned_body && outstanding() == 5);
    assert(__PREFIX___append_move(&tokens, &clone) == KU_STRUCT_ARRAY_OK);
    assert(tokens.len == 2 && tokens.capacity == 2 && tokens.data[1].f1.ptr == cloned_body);
    assert(!clone.f0.ptr && !clone.f1.ptr && outstanding() == 5);

    // The parser's existing 4096-entry boundary fits without narrowing the
    // language's array type or cloning any earlier owner during growth.
    KuProjectionString state = { spelling, sizeof(spelling), 0, KU_PROJECTION_STRING_STATIC };
    int before = attempts;
    for (size_t i = tokens.len; i < 4096; ++i) {
        __ELEMENT__ item = {0};
        item.f0 = state;
        assert(__PREFIX___append_move(&tokens, &item) == KU_STRUCT_ARRAY_OK && !item.f0.ptr);
    }
    assert(attempts - before == 11 && outstanding() == 5);
    assert(tokens.len == 4096 && tokens.capacity == 4096 && tokens.data[1].f1.ptr == cloned_body);
    before = attempts;
    assert(__PREFIX___clone_at(&tokens, 4095, &clone) == KU_STRUCT_ARRAY_OK);
    assert(clone.f0.ptr == spelling && attempts == before);
    __PREFIX___element_drop(&clone);

    old_storage = tokens.data;
    __ARRAY__ moved = __PREFIX___move(&tokens);
    assert(!tokens.data && !tokens.len && !tokens.capacity && moved.data == old_storage);
    assert(attempts == before);
    print_token(&moved.data[1]);
    __PREFIX___drop(&tokens);
    __PREFIX___drop(&moved);
    __PREFIX___drop(&moved);
    assert(outstanding() == 0);
    return 0;
}
"#;

const NESTED_OWNER_DRIVER: &str = r#"
int main(void) {
    __ARRAY__ values = { 1, malloc(sizeof(__ELEMENT__)), 1 };
    uint8_t *body = malloc(3);
    memcpy(body, "yes", 3);
    values.data[0] = (__ELEMENT__){0};
    values.data[0].f0.f0 = (KuProjectionString){ body, 3, 3, KU_PROJECTION_STRING_OWNED };
    values.data[0].f0.f1 = 11;
    __ELEMENT__ *initial = values.data;
    fail_after = 1;
    assert(__PREFIX___append_clone(&values, &values.data[0]) == KU_STRUCT_ARRAY_OOM);
    fail_after = -1;
    assert(values.data == initial && values.len == 1 && values.capacity == 1);
    assert(values.data[0].f0.f0.ptr == body && outstanding() == 2);
    assert(__PREFIX___append_clone(&values, &values.data[0]) == KU_STRUCT_ARRAY_OK);
    assert(values.len == 2 && values.capacity == 2 && outstanding() == 3);
    assert(values.data[0].f0.f0.ptr == body && values.data[1].f0.f0.ptr != body);
    assert(memcmp(values.data[1].f0.f0.ptr, "yes", 3) == 0 && values.data[1].f0.f1 == 11);
    __PREFIX___drop(&values);
    assert(outstanding() == 0);
    return 0;
}
"#;

fn write_array_owner_source(output: BoundedOutput, path: &Path, driver: &str) {
    assert_ok(&output);
    let generated = String::from_utf8(output.stdout).unwrap();
    let (names, body) = generated
        .split_once('\n')
        .expect("type names then generated C");
    let names: Vec<_> = names.trim_end().split('|').collect();
    let [element, array, prefix] = names.as_slice() else {
        panic!("generated array names")
    };
    let driver = driver
        .replace("__ELEMENT__", element)
        .replace("__ARRAY__", array)
        .replace("__PREFIX__", prefix);
    fs::write(path, format!("{CHECKED_ARRAY_ALLOCATOR}{body}{driver}")).unwrap();
}

#[test]
fn bootstrap_emits_owned_array_operations_for_relocate_tokens_type() {
    let work = Temp::new();
    let source = work.0.join("compiler");
    let project = work.0.join("project");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&project).unwrap();
    stage_sources(&source);
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let exe = format!("array-emitter{}", std::env::consts::EXE_SUFFIX);
    let entry = source.join("semantics/array-emitter.ku");
    let built = source.join("semantics").join(&exe);
    let installed = project.join(exe);
    fs::write(&entry, ARRAY_OWNER_EMITTER).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(entry)
            .arg("-o")
            .arg(&built),
        &project,
        150,
    ));
    fs::rename(built, &installed).unwrap();
    fs::remove_dir_all(&source).unwrap();

    for (path, contents) in [
        (
            "stage1/token.ku",
            include_str!("../bootstrap/stage1/token.ku"),
        ),
        (
            "stage1/lexer.ku",
            include_str!("../bootstrap/stage1/lexer.ku"),
        ),
        (
            "stage2/context.ku",
            include_str!("../bootstrap/stage2/context.ku"),
        ),
        (
            "stage2/span.ku",
            include_str!("../bootstrap/stage2/span.ku"),
        ),
    ] {
        let path = project.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let stage0 = project.join("reference.ku");
    fs::write(
        &stage0,
        r#"import { Scan } from "./stage1/lexer.ku"
import { Token, TokenCanonical } from "./stage1/token.ku"
fn main(): null! {
    tokens = Scan("plan = \"任务🙂\"")?
    token = tokens[2].clone()
    result: [Token] = []
    result = result.push(token)
    println(TokenCanonical(result[0].clone()))
    return ok(null)
}
"#,
    )
    .unwrap();
    let reference = execute(Command::new(&ku).arg("run").arg(stage0), &project, 20);
    assert_ok(&reference);
    let expected = String::from_utf8(reference.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(expected, "String|任务🙂|0|1|8|7|1|13|19\n");

    let select = project.join("target.txt");
    fs::write(&select, "RelocateTokens").unwrap();
    let path = project.join("array_owner.c");
    write_array_owner_source(
        execute(&mut Command::new(&installed), &project, 30),
        &path,
        TOKEN_OWNER_DRIVER,
    );
    fs::write(
        project.join("stage2/context.ku"),
        r#"
struct Item { label: str count: int }
struct Outer { item: Item }
fn Select(values: [Outer]): [Outer] { return values }
fn Primitive(values: [int]): [int] { return values }
"#,
    )
    .unwrap();
    fs::write(&select, "Select").unwrap();
    let nested = project.join("nested_owner.c");
    write_array_owner_source(
        execute(&mut Command::new(&installed), &project, 30),
        &nested,
        NESTED_OWNER_DRIVER,
    );
    fs::write(&select, "Primitive").unwrap();
    let unsupported = execute(&mut Command::new(&installed), &project, 15);
    assert!(!unsupported.status.success() && unsupported.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unsupported.stderr).contains("unsupported_struct_array_type"));
    for family in ["stage1", "stage2"] {
        fs::remove_dir_all(project.join(family)).unwrap();
    }
    fs::remove_file(project.join("reference.ku")).unwrap();
    let binary = project.join(format!("array-owner{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&path, &binary, &project);
    let run = execute(&mut Command::new(binary), &project, 15);
    assert_ok(&run);
    assert_eq!(
        String::from_utf8(run.stdout).unwrap().replace("\r\n", "\n"),
        expected
    );
    let binary = project.join(format!("nested-owner{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&nested, &binary, &project);
    assert_ok(&execute(&mut Command::new(binary), &project, 15));
}
