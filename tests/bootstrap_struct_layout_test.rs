//! Typed layout and struct projection checking run in the Ku compiler seed.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use ku::{span::Span, stdlib::json, value::Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ku-bootstrap-layout-{}-{nonce}",
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
    caught = false
    try { ParseModule(long_function)? } catch(err) { caught = err.message.contains("function window accepts at most 512 tokens") }
    if (!caught) { panic("function token limit changed") }
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
            format!("fn Test() {{\n{}\n}}", "value = 1 + 2\n".repeat(110)),
        )
        .expect("write oversized function witness");
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
