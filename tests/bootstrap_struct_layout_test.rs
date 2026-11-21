//! Typed layout and struct projection checking run in the Ku compiler seed.

#[path = "support/bounded_process.rs"]
pub mod bounded_process;

#[path = "support/bootstrap_binary_operator.rs"]
mod binary_operator;

#[path = "support/bootstrap_diagnostic_calls.rs"]
mod diagnostic_calls;

#[path = "support/bootstrap_linked_guard.rs"]
mod linked_guard;

#[path = "support/bootstrap_linked_chars.rs"]
mod linked_chars;

#[path = "support/bootstrap_linked_null.rs"]
mod linked_null;

#[path = "support/bootstrap_member_move.rs"]
mod member_move;

#[path = "support/bootstrap_struct_view.rs"]
mod struct_view;

#[path = "support/bootstrap_token_window.rs"]
mod token_window;

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
    compile_projection_product(source, output, cwd, true, false);
}

#[test]
fn bootstrap_emits_owning_layout_for_real_supplied_module() {
    let tmp = Temp::new();
    let copied = tmp.0.join("compiler");
    let cwd = tmp.0.join("application");
    fs::create_dir(&copied).unwrap();
    fs::create_dir(&cwd).unwrap();
    stage_sources(&copied);
    for file in [
        "semantics/call_contracts.ku",
        "semantics/imports.ku",
        "semantics/names.ku",
        "semantics/project_paths.ku",
        "stage2/ast.ku",
        "stage2/diagnostic.ku",
        "stage2/span.ku",
        "stage1/token.ku",
    ] {
        let output = cwd.join(file);
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::copy(copied.join(file), output).unwrap();
    }
    let entry = copied.join("semantics/emit_owning.ku");
    fs::write(
        &entry,
        r#"import "std.fs"
import { ReadProjectModules } from "./project_manifest.ku"
import { BindProjectModules } from "./project_bindings.ku"
import { BuildProjectOwningLayouts, EmitProjectOwningLayoutsC, OwningLayoutDropName } from "./nested_struct_c.ku"
import { CTypeName } from "./struct_projection_c.ku"
fn main(): null! {
    subject = fs.read("subject.txt")?
    needs_imports = subject == "semantics/call_contracts.ku"
    paths = [subject, "semantics/names.ku", "semantics/project_paths.ku",
        "stage2/ast.ku", "stage2/diagnostic.ku", "stage2/span.ku", "stage1/token.ku"]
    if (needs_imports) { paths = paths.push("semantics/imports.ku") }
    supplied = ReadProjectModules(paths)?
    project = BindProjectModules(supplied)?
    for item in supplied[0].named.names.declarations {
        if (item.kind == "Struct") {
            layouts = BuildProjectOwningLayouts(supplied, project, 1, item.node, item.span.clone())?
            emitted = EmitProjectOwningLayoutsC(layouts, 1, item.node, item.span)?
            println(CTypeName(1, item.node) + "|" + OwningLayoutDropName(1, item.node))
            print(emitted)
            return ok(null)
        }
    }
    fail { domain: "test", code: "empty", message: "no owning layout" }
}
"#,
    )
    .unwrap();
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let name = format!("emit-owning{}", std::env::consts::EXE_SUFFIX);
    let built = copied.join("semantics").join(&name);
    assert_ok(&execute(
        Command::new(ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&built),
        &cwd,
        180,
    ));
    let seed = cwd.join(name);
    fs::rename(built, &seed).unwrap();
    fs::remove_dir_all(copied).unwrap();

    fs::write(cwd.join("subject.txt"), "semantics/imports.ku").unwrap();
    let output = execute(&mut Command::new(&seed), &cwd, 30);
    assert_ok(&output);
    let emitted = String::from_utf8(output.stdout).unwrap();
    let (symbols, text) = emitted.split_once('\n').unwrap();
    let (ty, drop) = symbols.trim().split_once('|').unwrap();
    assert_eq!(text.matches("} KuArrayInt;").count(), 1);
    assert_eq!(text.matches(".len; i != 0; --i").count(), 2);
    let caller = r#"
static KuProjectionString string_owner(void) {
    uint8_t *bytes = malloc(1);
    if (!bytes) return (KuProjectionString){0};
    *bytes = UINT8_C(42);
    return (KuProjectionString){bytes, 1, 1, KU_PROJECTION_STRING_OWNED};
}
static void partial_project(RESULT *project) {
    project->f0 = string_owner();
    if (!project->f0.ptr) return;
    project->f1.f0.f0.f0.data = malloc(4 * sizeof *project->f1.f0.f0.f0.data);
    if (!project->f1.f0.f0.f0.data) return;
    memset(project->f1.f0.f0.f0.data, 0xff, 4 * sizeof *project->f1.f0.f0.f0.data);
    project->f1.f0.f0.f0.capacity = 4;
    memset(&project->f1.f0.f0.f0.data[0], 0, sizeof *project->f1.f0.f0.f0.data);
    project->f1.f0.f0.f0.len = 1;
    project->f1.f0.f0.f0.data[0].f0 = string_owner();
    if (!project->f1.f0.f0.f0.data[0].f0.ptr) return;
    project->f1.f0.f0.f1.data = malloc(sizeof *project->f1.f0.f0.f1.data);
    if (!project->f1.f0.f0.f1.data) return;
    project->f1.f0.f0.f1.data[0] = INT64_C(42);
    project->f1.f0.f0.f1.len = project->f1.f0.f0.f1.capacity = 1;
    project->f1.f1.f0.data = malloc(sizeof *project->f1.f1.f0.data);
    if (!project->f1.f1.f0.data) return;
    memset(project->f1.f1.f0.data, 0, sizeof *project->f1.f1.f0.data);
    project->f1.f1.f0.len = project->f1.f1.f0.capacity = 1;
    project->f1.f1.f0.data[0].f1 = string_owner();
}
int main(void) {
    (void)test_realloc;
    for (int limit = 0; limit <= 6; ++limit) {
        RESULT owner = {0};
        fail_after = limit;
        partial_project(&owner);
        assert(outstanding() == limit);
        DROP(&owner);
        assert(outstanding() == 0);
        assert(!owner.f0.ptr && !owner.f1.f0.f0.f0.data && !owner.f1.f1.f0.data);
        DROP(&owner);
    }
    puts("partial and complete Project owners released");
    return 0;
}
"#;
    let path = cwd.join("owning.c");
    fs::write(
        &path,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{text}{}",
            caller.replace("RESULT", ty).replace("DROP", drop)
        ),
    )
    .unwrap();
    let binary = cwd.join(format!("owning{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&path, &binary, &cwd);
    let result = execute(&mut Command::new(binary), &cwd, 10);
    assert_ok(&result);
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        "partial and complete Project owners released"
    );

    fs::write(cwd.join("subject.txt"), "semantics/call_contracts.ku").unwrap();
    let output = execute(&mut Command::new(&seed), &cwd, 30);
    assert_ok(&output);
    let emitted = String::from_utf8(output.stdout).unwrap();
    let (symbols, text) = emitted.split_once('\n').unwrap();
    let (ty, drop) = symbols.trim().split_once('|').unwrap();
    assert_eq!(text.matches("} KuGuardStringArray;").count(), 1);
    let caller = r#"
int main(void) {
    (void)test_realloc;
    RESULT signature = {0};
    signature.f1.data = malloc(sizeof *signature.f1.data);
    assert(signature.f1.data);
    memset(signature.f1.data, 0, sizeof *signature.f1.data);
    signature.f1.len = signature.f1.capacity = 1;
    KuGuardStringArray *types = &signature.f1.data[0].f0;
    types->data = malloc(3 * sizeof *types->data);
    assert(types->data);
    memset(types->data, 0xff, 3 * sizeof *types->data);
    types->capacity = 3;
    types->data[0] = (KuProjectionString){(uint8_t*)"int", 3, 0, KU_PROJECTION_STRING_STATIC};
    types->data[1] = (KuProjectionString){malloc(3), 3, 3, KU_PROJECTION_STRING_OWNED};
    assert(types->data[1].ptr);
    memcpy(types->data[1].ptr, "str", 3);
    types->len = 2;
    assert(outstanding() == 3);
    DROP(&signature);
    DROP(&signature);
    assert(!signature.f1.data && outstanding() == 0);
    puts("signature string array released");
    return 0;
}
"#;
    fs::write(
        &path,
        format!(
            "{CHECKED_ARRAY_ALLOCATOR}{text}{}",
            caller.replace("RESULT", ty).replace("DROP", drop)
        ),
    )
    .unwrap();
    let binary = cwd.join(format!("signature{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&path, &binary, &cwd);
    let result = execute(&mut Command::new(binary), &cwd, 10);
    assert_ok(&result);
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        "signature string array released"
    );

    for (source, diagnostic) in [
        ("struct Loop { children: [Loop] }", "recursive_owned_layout"),
        (
            "struct Shallow { flags: [bool] }",
            "unsupported_layout_type",
        ),
    ] {
        fs::write(cwd.join("subject.txt"), "rejected.ku").unwrap();
        fs::write(cwd.join("rejected.ku"), source).unwrap();
        let result = execute(&mut Command::new(&seed), &cwd, 30);
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains(diagnostic));
    }
}

fn compile_projection_product(
    source: &Path,
    output: &Path,
    cwd: &Path,
    strict: bool,
    object: bool,
) {
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
        compile.args(["/nologo", "/std:c11", "/O2"]);
        if strict {
            compile.arg("/WX");
        }
        if object {
            compile.arg("/c").arg(format!("/Fo{}", output.display()));
        } else {
            compile
                .arg(format!("/Fe{}", output.display()))
                .arg(format!("/Fo{}", output.with_extension("obj").display()));
        }
        compile.arg(source);
    } else {
        compile.args(["-std=c11", "-O2"]);
        if strict {
            compile.args(["-pedantic-errors", "-Wall", "-Wextra", "-Werror"]);
        }
        if object {
            compile.arg("-c");
        }
        compile.arg(source).arg("-o").arg(output);
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
    assert_projected_span(&ku, &driver_binary, &cwd, span_source);
    assert_constructor_parameter_ownership(&driver_binary, &cwd);
    assert_constructor_view(&driver_binary, &cwd);
    assert_guarded_predicate(&driver_binary, &cwd);
    assert_guarded_string_array(&driver_binary, &cwd);
    assert_integer_prefix_projection(&driver_binary, &cwd);
    assert_project_link_plans(&driver_binary, &cwd);
    linked_null::assert_checked_owned_call(&driver_binary, &cwd);
    linked_null::assert_owned_binary_join(&driver_binary, &cwd);
    linked_null::assert_path_byte_guard(&driver_binary, &cwd);
    linked_chars::assert_owned_characters(&driver_binary, &cwd);
    token_window::assert_readonly_preflight(&driver_binary, &cwd);
    assert_managed_source_characters(&driver_binary, &cwd);
}

fn assert_managed_source_characters(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("source_probe.ku"),
        r#"
struct SourceLength { value: int }
fn PrepareSource(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    if (source.byte_len() > 131072) { return ok(entries) }
    if (source.len() > 32768) { return ok(entries) }
    characters: [str] = source.chars()
    item = SourceLength { value: characters.len() }
    entries = entries.push(item)
    return ok(entries)
}
fn ReadLocal(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    combined = ""
    index = 0
    while (index < characters.len()) {
        ch = characters[index].clone()
        next = ""
        if (index + 1 < characters.len()) { next = characters[index + 1].clone() }
        size = ch.byte_len() + next.byte_len()
        if (index + 1 < characters.len() && characters[index + 1].clone() == "中") { size = size + 1 }
        combined += ch
        total = size + combined.byte_len()
        item = SourceLength { value: total }
        entries = entries.push(item)
        index = index + 1
    }
    return ok(entries)
}
fn Describe(message: str, count: int): str {
    return message + str(count)
}
fn DescribeLocal(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    count = characters.len()
    fail { domain: "test", code: "count", message: Describe("size=", count) }
}
fn Allows(value: str): bool {
    return value != "" && "az_".contains(value)
}
fn TestLocal(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    if (characters.len() > 0) {
        ch = characters[0].clone()
        if (Allows(ch.clone())) {
            width = ch.byte_len()
            item = SourceLength { value: width }
            entries = entries.push(item)
        }
    }
    return ok(entries)
}
fn BadAppend(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    right = characters[0].clone()
    left = ""
    left += right
    width = right.byte_len()
    item = SourceLength { value: width }
    entries = entries.push(item)
    return ok(entries)
}
fn RepeatedAppend(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    right = characters[0].clone()
    left = ""
    index = 0
    while (index < 2) {
        left += right
        index = index + 1
    }
    return ok(entries)
}
fn ResolveKind(word: str): str {
    if (word == "fn") { return "Fn" }
    return "Ident"
}
fn ChooseKind(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    tag = "previous"
    tag = ResolveKind(source.clone())
    width = tag.byte_len()
    item = SourceLength { value: width }
    entries = entries.push(item)
    return ok(entries)
}
fn AllocatingMessage(word: str): str {
    return word + "!"
}
fn RejectAllocatingKind(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    if (source.byte_len() == 0) {
        fail { domain: "test", code: "empty", message: AllocatingMessage("empty") }
    }
    tag = AllocatingMessage(source.clone())
    return ok(entries)
}
struct Scanned { text: str, count: int }
fn ReadScanned(&characters: [str], count: int): Scanned! {
    return ok(Scanned { text: "found", count: count })
}
fn TakeScanned(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    text = "previous"
    count = 0
    if (characters.len() > 0) {
        item = ReadScanned(characters, count)?
        text = item.text
        count = item.count
    }
    width = text.byte_len()
    entry = SourceLength { value: width }
    entries = entries.push(entry)
    return ok(entries)
}
fn ReadQuote(&characters: [str], quote: str, count: int): Scanned! {
    return ok(Scanned { text: "found", count: count })
}
fn TakeQuote(source: str, first: bool, index: int): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    quote = characters[0].clone()
    count = 0
    if (first) {
        item = ReadQuote(characters, quote, index + 1)?
        count = item.count
    } else {
        item = ReadQuote(characters, quote, index)?
        count = item.count
    }
    entry = SourceLength { value: count }
    entries = entries.push(entry)
    return ok(entries)
}
fn ReuseQuote(source: str, first: bool): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    quote = characters[0].clone()
    count = 0
    if (first) {
        item = ReadQuote(characters, quote, count)?
        count = item.count
    }
    count = quote.byte_len()
    return ok(entries)
}
fn ReplaceDecoded(&characters: [str], quote: str): Scanned! {
    decoded = characters[0].clone()
    decoded = quote.clone()
    count = quote.byte_len()
    return ok(Scanned { text: decoded, count: count })
}
fn ReplaceEscaped(&characters: [str], template: bool): Scanned! {
    escaped = characters[0].clone()
    decoded = characters[1].clone()
    if (template) {
        decoded = "\\" + escaped
    } else {
        decoded = escaped.clone()
    }
    count = decoded.byte_len()
    return ok(Scanned { text: decoded, count: count })
}
fn DescribeEscaped(&characters: [str], valid: bool): Scanned! {
    escaped = characters[0].clone()
    count = 0
    if (!valid) {
        fail { domain: "test", code: "escape", message: Describe("[" + escaped + "]", count) }
    }
    return ok(Scanned { text: escaped, count: count })
}
fn CheckLength(count: int): null! {
    if (count > 2) { fail { domain: "test", code: "size", message: "too long" } }
    return ok(null)
}
fn CheckOwner(value: str): null! {
    return ok(null)
}
fn CheckArray(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    characters: [str] = source.chars()
    if (characters.len() != 0) { CheckLength(characters.len())? }
    CheckLength(characters.len())?
    return ok(entries)
}
fn CheckRecord(&characters: [str], checked: bool): Scanned! {
    saved = characters[0].clone()
    if (checked) { CheckLength(saved.len())? }
    CheckLength(saved.len())?
    return ok(Scanned { text: saved, count: 1 })
}
fn RejectCheckedOwner(source: str): [SourceLength]! {
    entries: [SourceLength] = []
    CheckOwner(source)?
    return ok(entries)
}
fn JoinClones(&characters: [str], left: str, right: str): Scanned! {
    saved = characters[0].clone()
    pair = left.clone() + right.clone()
    saved = pair
    width = saved.byte_len()
    return ok(Scanned { text: saved, count: width })
}
fn RejectMoveReuse(&characters: [str], left: str, right: str): Scanned! {
    saved = characters[0].clone()
    pair = left.clone() + right.clone()
    saved = pair
    width = pair.byte_len()
    return ok(Scanned { text: saved, count: width })
}
"#,
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "source_probe.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "PrepareSource").unwrap();
    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let body = text.split_once("(KuProjectionString a0) {").unwrap().1;
    let bytes = body.find("a0.len").unwrap();
    let scalars = body.find("for (size_t char_index").unwrap();
    let characters = body
        .find("source_status = ku_managed_source_chars(a0")
        .unwrap();
    assert!(bytes < scalars && scalars < characters);
    let cleanup = body.split_once("cleanup:\n").unwrap().1;
    assert!(cleanup.contains("ku_managed_source_drop(&l"));
    assert!(cleanup.contains("ku_projection_string_drop(&a0)"));
    assert!(cleanup.contains("source_status != KU_SOURCE_OK"));

    fs::write(cwd.join("mode.txt"), "managed-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(
        r#""kind":"Builtin","source_name":"str","source_path":null,"node":0,"c_type":"KuProjectionString","passing":"value""#
    ));
    assert!(plan.contains(r#""dependencies":[]"#));

    fs::write(cwd.join("function.txt"), "ReadLocal").unwrap();
    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let body = text.split_once("(KuProjectionString a0) {").unwrap().1;
    let (iteration, cleanup) = body.split_once("cleanup:\n").unwrap();
    assert_eq!(
        iteration
            .matches("if (!ku_guard_string_array_clone_at(&l")
            .count(),
        2
    );
    let (lookup, replacement) = iteration.split_once("&replaced").unwrap();
    assert!(lookup.contains("while (true)") && lookup.contains("KuProjectionString replaced"));
    let drop = replacement.find("ku_projection_string_drop(&l").unwrap();
    assert!(drop < replacement.find(" = replaced").unwrap());
    assert!(replacement.matches("ku_projection_string_drop(&l").count() >= 3);
    let compare = replacement
        .find("if (!ku_guard_string_array_equal(&l")
        .unwrap();
    let branch = replacement[..compare].rfind("if (t").unwrap();
    assert!(replacement[branch..compare].contains("array_match"));
    let append = replacement
        .find("if (!ku_managed_string_append(&l")
        .unwrap();
    let drop = replacement[append..].lines().nth(1).unwrap();
    assert!(drop.contains("ku_projection_string_drop(&l"));
    assert!(cleanup.contains("ku_managed_source_drop(&l"));
    assert!(cleanup.contains("array_failure.code == 2"));
    assert!(cleanup.contains("if (string_too_large)"));

    fs::write(cwd.join("function.txt"), "DescribeLocal").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    assert!(text.contains("extern KuProjectionString ku_str_"));
    let cleanup = text.split_once("cleanup:\n").unwrap().1;
    let message = cleanup.find("result.error.message = ku_str_").unwrap();
    assert!(cleanup[..message].contains("ku_managed_source_drop(&l"));
    assert!(cleanup[..message].contains("ku_projection_string_drop(&a0)"));
    fs::write(cwd.join("mode.txt"), "managed-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    let dependencies = plan.split_once(r#""dependencies":["#).unwrap().1;
    assert!(dependencies.contains(r#""source_name":"Describe","symbol":"ku_str_"#));
    assert_eq!(
        dependencies.matches(r#""source_name":"Describe"#).count(),
        1
    );

    fs::write(cwd.join("function.txt"), "TestLocal").unwrap();
    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let body = text.split_once("(KuProjectionString a0) {").unwrap().1;
    let clone = body.find("if (!ku_result_clone_string(l").unwrap();
    let predicate = body.find(" = ku_bool_").unwrap();
    assert!(clone < predicate && body[predicate..].contains(".len"));
    let cleanup = body.split_once("cleanup:\n").unwrap().1;
    assert!(cleanup.contains("ku_projection_string_drop(&l"));
    assert!(cleanup.contains("if (out_of_memory)"));
    fs::write(cwd.join("mode.txt"), "managed-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    let dependency = plan.split_once(r#""dependencies":["#).unwrap().1;
    assert!(dependency.contains(r#""source_name":"Allows","symbol":"ku_bool_"#));
    assert!(
        dependency.contains(r#""source_name":"bool","source_path":null,"node":0,"c_type":"bool"#)
    );

    fs::write(cwd.join("function.txt"), "BadAppend").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("managed_append_move"));
    fs::write(cwd.join("function.txt"), "RepeatedAppend").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("outside its iteration"));

    fs::write(cwd.join("function.txt"), "ChooseKind").unwrap();
    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let body = text.split_once("(KuProjectionString a0) {").unwrap().1;
    let clone = body.find("if (!ku_result_clone_string(a0").unwrap();
    let result = body.find(" = ku_str_").unwrap();
    let drop = body.find("ku_projection_string_drop(&l").unwrap();
    assert!(clone < result && result < drop);
    assert!(body[drop..].contains(" = replaced"));
    fs::write(cwd.join("mode.txt"), "managed-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(r#""source_name":"ResolveKind","symbol":"ku_str_"#));

    fs::write(cwd.join("function.txt"), "RejectAllocatingKind").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unsupported_guarded_operand"));

    fs::write(cwd.join("function.txt"), "TakeScanned").unwrap();
    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    assert!(text.contains("const KuGuardStringArray* record_arg"));
    assert!(text.contains(" = &l"));
    assert!(text.contains(".value.f0 = (KuProjectionString){0};"));
    assert!(text.contains("result.error = record_result"));
    assert!(text.contains(".error = (KuBootstrapError){0};"));
    fs::write(cwd.join("mode.txt"), "managed-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(r#""source_name":"ReadScanned","symbol":"ku_struct_result_"#));
    assert!(plan.contains(r#""c_type":"const KuGuardStringArray*","passing":"view""#));

    fs::write(cwd.join("function.txt"), "TakeQuote").unwrap();
    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let overflow = text.find("{ overflow = true; goto cleanup; }").unwrap();
    let cleared = text[overflow..]
        .find(" = (KuProjectionString){0};")
        .unwrap();
    let call = text[overflow..].find(" = ku_struct_result_").unwrap();
    assert!(cleared < call);
    assert_eq!(text.matches(" = ku_struct_result_").count(), 2);

    fs::write(cwd.join("function.txt"), "ReuseQuote").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("managed_call_move"));

    fs::write(cwd.join("function.txt"), "ReplaceDecoded").unwrap();
    fs::write(cwd.join("mode.txt"), "struct-result").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let clone = text
        .find("if (!ku_result_clone_string(a1, &replaced")
        .unwrap();
    let drop = text[clone..].find("ku_projection_string_drop(&l").unwrap();
    let replace = text[clone..].find(" = replaced").unwrap();
    assert!(drop < replace);
    assert!(text[clone + replace..].contains("a1.len"));

    fs::write(cwd.join("function.txt"), "ReplaceEscaped").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let concat = text
        .find("if (!ku_managed_string_append(&replaced")
        .unwrap();
    let next = text[concat..].lines().skip(1).take(3).collect::<Vec<_>>();
    assert!(next[0].contains("ku_projection_string_drop(&l"));
    assert!(next[1].contains("ku_projection_string_drop(&l"));
    assert!(next[2].contains(" = replaced"));

    fs::write(cwd.join("function.txt"), "DescribeEscaped").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let (body, cleanup) = text.split_once("cleanup:\n").unwrap();
    assert!(body.contains("saved_error") && body.contains(" = (KuProjectionString){0};"));
    let compose = cleanup
        .find("if (!ku_managed_string_append(&joined_error")
        .unwrap();
    let invoke = cleanup.find("result.error.message = ku_str_").unwrap();
    assert!(compose < invoke);
    assert!(cleanup[compose..invoke].contains("ku_projection_string_drop(&saved_error"));

    for (function, mode) in [("CheckArray", "managed"), ("CheckRecord", "struct-result")] {
        fs::write(cwd.join("function.txt"), function).unwrap();
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        let emitted = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&emitted);
        let text = String::from_utf8(emitted.stdout).unwrap();
        assert_eq!(text.matches("extern KuBootstrapResultNull ").count(), 1);
        assert_eq!(text.matches(" = ku_result_dependency_").count(), 2);
        assert_eq!(text.matches("result.error = dependency").count(), 2);
        let cleanup = text.split_once("cleanup:\n").unwrap().1;
        assert!(cleanup.contains("ku_projection_string_drop(&"));
        fs::write(cwd.join("mode.txt"), format!("{mode}-plan")).unwrap();
        let plan = execute(&mut Command::new(seed), cwd, 30);
        assert_ok(&plan);
        assert_eq!(
            String::from_utf8(plan.stdout)
                .unwrap()
                .matches(r#""source_name":"CheckLength""#)
                .count(),
            1
        );
    }

    fs::write(cwd.join("mode.txt"), "managed").unwrap();
    fs::write(cwd.join("function.txt"), "RejectCheckedOwner").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("managed_checked_argument"));

    fs::write(cwd.join("mode.txt"), "struct-result").unwrap();
    fs::write(cwd.join("function.txt"), "JoinClones").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let left = text
        .find("if (!ku_result_clone_string(a1, &concat_left")
        .unwrap();
    let right = text
        .find("!ku_result_clone_string(a2, &concat_right")
        .unwrap();
    let join = text.find("!ku_managed_string_append(&concat_left").unwrap();
    assert!(left < right && right < join);
    let failure = text[join..].split_once("goto cleanup;").unwrap().0;
    assert!(failure.contains("drop(&concat_left") && failure.contains("drop(&concat_right"));
    let (target, cleanup) = text[join..].split_once("cleanup:\n").unwrap();
    let moved = target.split_once("KuProjectionString replaced").unwrap().1;
    let source = moved.lines().nth(1).unwrap().split_once(" = ").unwrap().1;
    assert!(moved.lines().nth(2).unwrap().contains(&format!(
        "{} = (KuProjectionString){{0}};",
        source.trim_end_matches(';')
    )));
    assert!(cleanup.contains("ku_projection_string_drop(&a1)"));
    assert!(cleanup.contains("ku_projection_string_drop(&a2)"));

    fs::write(cwd.join("function.txt"), "RejectMoveReuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("managed_call_move"));
}

fn assert_guarded_predicate(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("mode.txt"), "guarded").unwrap();
    fs::write(cwd.join("function.txt"), "IsNamedStructStart").unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "stage2/expression_support.ku\nstage2/span.ku\nstage2/diagnostic.ku\nstage1/token.ku\nstage2/ast.ku\n",
    )
    .unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    assert!(!text.contains("malloc(") && !text.contains("free("));
    let signature = text
        .lines()
        .find_map(|line| line.strip_prefix("bool ku_guarded_"))
        .unwrap();
    let (id, arguments) = signature.split_once('(').unwrap();
    let token = arguments
        .strip_prefix("const ")
        .unwrap()
        .split_once('*')
        .unwrap()
        .0;
    let driver = r#"
#define TEXT(s) ((KuProjectionString){(uint8_t*)(s),sizeof(s)-1,0,KU_PROJECTION_STRING_STATIC})
int main(void) {
    TOKEN name={.f0=TEXT("Ident"),.f3=7,.f6=7}, opener={.f0=TEXT("LBrace"),.f3=7,.f6=7};
    TOKEN field={.f0=TEXT("Ident"),.f3=7,.f6=7}, sep={.f0=TEXT("Colon"),.f3=7,.f6=7};
    if (!PREDICATE(&name,&opener,&field,&sep)) return 1;
    opener.f3=8;
    if (PREDICATE(&name,&opener,&field,&sep)) return 2;
    opener.f3=7;
    sep.f0=TEXT("Comma");
    return PREDICATE(&name,&opener,&field,&sep);
}
"#
    .replace("TOKEN", token)
    .replace("PREDICATE", &format!("ku_guarded_{id}"));
    let c = cwd.join("guarded-predicate.c");
    fs::write(&c, format!("{text}{driver}")).unwrap();
    let exe = cwd.join(format!("guarded-predicate{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(exe), cwd, 15));
}

fn assert_guarded_string_array(seed: &Path, cwd: &Path) {
    fs::write(
        cwd.join("stage1/lexer.ku"),
        include_str!("../bootstrap/stage1/lexer.ku"),
    )
    .unwrap();
    fs::write(cwd.join("mode.txt"), "guarded").unwrap();
    fs::write(cwd.join("function.txt"), "HasFractionPoint").unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "stage1/lexer.ku\nstage1/token.ku\n",
    )
    .unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let symbol = text
        .lines()
        .find_map(|line| line.strip_prefix("bool ku_guarded_"))
        .unwrap()
        .split_once('(')
        .unwrap()
        .0;
    let driver = r#"
int main(int argc, char **argv) {
    (void)argv;
    uint8_t dot[] = {'.'};
    KuProjectionString item = {dot, 1u, 0u, KU_PROJECTION_STRING_STATIC};
    const KuGuardStringArray value = {1u, &item, 1u};
    if (argc == 2) return PREDICATE(&value, -1, 1);
    if (argc == 3) return PREDICATE(&value, INT64_MAX, INT64_MAX);
    uint8_t *owner = malloc(1);
    if (!owner) return 1;
    owner[0] = '.';
    item = (KuProjectionString){owner, 1u, 1u, KU_PROJECTION_STRING_OWNED};
    bool success = PREDICATE(&value, 0, 2) && !PREDICATE(&value, 1, 2);
    success = success && value.data == &item && item.ptr == owner && owner[0] == '.';
    free(owner);
    return success ? 0 : 1;
}
"#
    .replace("PREDICATE", &format!("ku_guarded_{symbol}"));
    let c = cwd.join("guarded-string-array.c");
    fs::write(&c, format!("{text}{driver}")).unwrap();
    let exe = cwd.join(format!(
        "guarded-string-array{}",
        std::env::consts::EXE_SUFFIX
    ));
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 15));
    for (args, wanted) in [
        (
            &["bound"][..],
            "array/index_out_of_bounds: index -1 out of bounds for length 1",
        ),
        (&["overflow", "case"][..], "integer overflow"),
    ] {
        let run = execute(Command::new(&exe).args(args), cwd, 15);
        assert_eq!(run.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&run.stderr).contains(wanted));
    }
    fs::write(cwd.join("mode.txt"), "guarded-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let document = String::from_utf8(plan.stdout).unwrap();
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(document)], Span::default()).unwrap()
    else {
        panic!("expected a typed array link plan");
    };
    let Value::Object(plan) = *value else {
        panic!("expected a plan object");
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected the emitted export");
    };
    let Value::Object(signature) = &export["signature"] else {
        panic!("expected a checked signature");
    };
    let Value::Array(parameters) = &signature["parameters"] else {
        panic!("expected three planned parameters");
    };
    assert_eq!(parameters.len(), 3);
    let Value::Object(carrier) = &parameters[0] else {
        panic!("expected a typed array carrier");
    };
    assert_eq!(carrier["kind"], Value::String("BuiltinArray".into()));
    assert_eq!(carrier["source_name"], Value::String("str".into()));
    assert_eq!(carrier["source_path"], Value::Null);
    assert_eq!(carrier["passing"], Value::String("view".into()));
    assert_eq!(
        carrier["c_type"],
        Value::String("const KuGuardStringArray*".into())
    );
}

fn assert_integer_prefix_projection(seed: &Path, cwd: &Path) {
    fs::write(cwd.join("mode.txt"), "struct-result").unwrap();
    fs::write(cwd.join("function.txt"), "ScanIntegerPrefix").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let (assignment, clear) = text
        .lines()
        .zip(text.lines().skip(1))
        .find(|(assignment, _)| assignment.starts_with("    result.value.f0 = l"))
        .expect("the actual prefix moves a live lexeme into its result");
    let local = assignment
        .strip_prefix("    result.value.f0 = ")
        .and_then(|value| value.strip_suffix(';'))
        .unwrap();
    assert_eq!(clear, format!("    {local} = (KuProjectionString){{0}};"));
    assert!(text.contains("while (true)") && text.contains("ku_guard_string_array_clone_at(a0"));

    fs::write(cwd.join("mode.txt"), "struct-result-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(r#""kind":"BuiltinArray","source_name":"str""#));
    assert!(plan.contains(r#""source_name":"IsDigit","symbol":"ku_bool_"#));
    assert!(plan.contains(r#""source_name":"DigitValue","symbol":"ku_int_"#));

    let lexer = cwd.join("stage1/lexer.ku");
    let source = fs::read_to_string(&lexer).unwrap();
    let caller = r#"
fn ObserveNumberPrefix(&characters: [str], length: int, start_index: int, start_line: int,
                       start_column: int, start_offset: int): ScannedLiteral! {
    prefix = ScanIntegerPrefix(characters, length, start_index, start_column, start_offset)?
    {lexeme, int_value, next_index: index, column, offset, overflow: _} = prefix
    return ok(ScannedLiteral { kind: "Int", lexeme: lexeme, int_value: int_value,
        next_index: index, line: start_line, column: column, offset: offset })
}
"#;
    fs::write(&lexer, format!("{source}{caller}")).unwrap();
    fs::write(cwd.join("function.txt"), "ObserveNumberPrefix").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(r#""source_name":"ScanIntegerPrefix","symbol":"ku_struct_result_"#));
    fs::write(cwd.join("mode.txt"), "struct-result").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let (transfer, clear) = text
        .lines()
        .zip(text.lines().skip(1))
        .find(|(line, _)| line.starts_with("        result.error = record_result"))
        .expect("the caller moves the exact child Error carrier");
    let error = transfer
        .strip_prefix("        result.error = ")
        .and_then(|value| value.strip_suffix(';'))
        .unwrap();
    assert_eq!(clear, format!("        {error} = (KuBootstrapError){{0}};"));
    assert!(text.contains(".value.f0 = (KuProjectionString){0};"));

    let source = fs::read_to_string(&lexer).unwrap();
    let predicate = r#"
struct FractionProbe { begins: bool }
fn ObserveFraction(&characters: [str], index: int, length: int): FractionProbe! {
    is_float = HasFractionPoint(characters, index, length) && IsDigit(characters[index + 1].clone())
    return ok(FractionProbe { begins: is_float })
}
"#;
    fs::write(&lexer, format!("{source}{predicate}")).unwrap();
    fs::write(cwd.join("function.txt"), "ObserveFraction").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let body = text.split_once("    bool t").unwrap().1;
    let guard = body.find(" = ku_guarded_").unwrap();
    let selected = body.find("    if (t").unwrap();
    let indexed = body
        .find("        if (!ku_guard_string_array_clone_at(a0")
        .unwrap();
    let digit = body.find(" = ku_bool_").unwrap();
    assert!(guard < selected && selected < indexed && indexed < digit);
    fs::write(cwd.join("mode.txt"), "struct-result-plan").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(r#""source_name":"HasFractionPoint","symbol":"ku_guarded_"#));
    assert!(plan.contains(r#""source_name":"IsDigit","symbol":"ku_bool_"#));

    fs::write(&lexer, include_str!("../bootstrap/stage1/lexer.ku")).unwrap();
    fs::write(cwd.join("function.txt"), "ScanNumber").unwrap();
    let plan = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&plan);
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains(r#""source_name":"ScanIntegerPrefix","symbol":"ku_struct_result_"#));
    assert!(plan.contains(r#""source_name":"At","symbol":"ku_str_"#));

    fs::write(cwd.join("mode.txt"), "struct-result").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let text = String::from_utf8(emitted.stdout).unwrap();
    let (body, cleanup) = text.split_once("\ncleanup:\n").unwrap();
    assert!(body.contains("record_error = ") && body.contains("error_arg"));
    assert!(!body.contains("result.error.message = ku_str_"));
    let call = cleanup.find("result.error.message = ku_str_").unwrap();
    let last_drop = cleanup.rfind("ku_projection_string_drop(&").unwrap();
    assert!(last_drop < call);
}

fn assert_constructor_view(seed: &Path, cwd: &Path) {
    for (file, source) in [
        (
            "stage2/expression_support.ku",
            include_str!("../bootstrap/stage2/expression_support.ku"),
        ),
        ("stage2/ast.ku", include_str!("../bootstrap/stage2/ast.ku")),
    ] {
        fs::write(cwd.join(file), source).unwrap();
    }
    fs::write(
        cwd.join("modules.txt"),
        "stage2/expression_support.ku\nstage2/span.ku\nstage2/diagnostic.ku\nstage1/token.ku\nstage2/ast.ku\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "OperatorSpan").unwrap();
    let output = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("free(") && !text.contains("malloc("));
    let definition = text
        .lines()
        .find(|line| line.contains(" ku_construct_"))
        .unwrap();
    let (result, callable) = definition.split_once(' ').unwrap();
    let (constructor, arguments) = callable.split_once('(').unwrap();
    let entry = arguments
        .strip_prefix("const ")
        .unwrap()
        .split_once('*')
        .unwrap()
        .0;
    let driver = r#"
#include <stdlib.h>
int main(void) {
    uint8_t *owner=malloc(1);
    if(!owner) return 1;
    *owner=73;
    ENTRY input={.f0={owner,1,1,KU_PROJECTION_STRING_OWNED},.f4=2,.f5=3,.f6=7,.f7=4,.f8=5,.f9=9};
    RESULT projected=CTOR(&input);
    if (projected.f0!=2 || projected.f1!=3 || projected.f2!=7 ||
        projected.f3!=4 || projected.f4!=5 || projected.f5!=9 ||
        input.f0.ptr!=owner || *input.f0.ptr!=73) return 2;
    free(input.f0.ptr);
    return 0;
}
"#
    .replace("ENTRY", entry)
    .replace("RESULT", result)
    .replace("CTOR", constructor);
    let source = cwd.join("constructor-view.c");
    fs::write(&source, format!("{text}{driver}")).unwrap();
    let binary = cwd.join(format!("constructor-view{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&source, &binary, cwd);
    assert_ok(&execute(&mut Command::new(binary), cwd, 15));

    for (program, expected) in [
        (
            "struct Data { label: str line: int }\nstruct ResultValue { data: Data line: int }\n\
             fn Try(value: Data): ResultValue { return ResultValue { data: value, line: value.line } }\n",
            "constructor_parameter_moved",
        ),
        (
            "struct Data { label: str }\nstruct ResultValue { data: Data }\n\
             fn Try(&value: Data): ResultValue { return ResultValue { data: value } }\n",
            "constructor_view_escape",
        ),
    ] {
        fs::write(cwd.join("bad-copy.ku"), program).unwrap();
        fs::write(cwd.join("modules.txt"), "bad-copy.ku\n").unwrap();
        fs::write(cwd.join("function.txt"), "Try").unwrap();
        let output = execute(&mut Command::new(seed), cwd, 30);
        assert!(!output.status.success() && output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}

fn assert_project_link_plans(driver: &Path, cwd: &Path) {
    for (path, source) in [
        (
            "stage2/expression_support.ku",
            include_str!("../bootstrap/stage2/expression_support.ku"),
        ),
        ("stage2/ast.ku", include_str!("../bootstrap/stage2/ast.ku")),
    ] {
        fs::write(cwd.join(path), source).unwrap();
    }
    let emitter = |mode: &str, name: &str| {
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        fs::write(cwd.join("function.txt"), name).unwrap();
        let produced = execute(&mut Command::new(driver), cwd, 30);
        assert_ok(&produced);
        assert!(produced.stderr.is_empty());
        String::from_utf8(produced.stdout).unwrap()
    };
    let decode = |document: String| {
        let Some(Value::Result { ok: true, value }) =
            json::eval("decode", &[Value::String(document)], Span::default()).unwrap()
        else {
            panic!("expected one project link plan JSON document")
        };
        let Value::Object(record) = *value else {
            panic!("expected a plan object")
        };
        record
    };
    for (mode, name, manifest, wanted) in [
        (
            "struct-result",
            "ReadPrimaryAtom",
            "stage2/expression_support.ku\nstage2/span.ku\nstage2/diagnostic.ku\nstage1/token.ku\nstage2/ast.ku\n",
            &["RejectAt"][..],
        ),
        (
            "linked-null",
            "RejectAt",
            "stage2/expression_support.ku\nstage2/span.ku\nstage2/diagnostic.ku\nstage1/token.ku\nstage2/ast.ku\n",
            &["MakeDiagnostic", "SpanFromToken", "DiagnosticCanonical"][..],
        ),
        (
            "linked-null",
            "RejectAtPoint",
            "stage2/expression_support.ku\nstage2/span.ku\nstage2/diagnostic.ku\nstage1/token.ku\nstage2/ast.ku\n",
            &["RejectAt"][..],
        ),
        (
            "linked-str",
            "DiagnosticCanonical",
            "stage2/diagnostic.ku\nstage2/span.ku\nstage1/token.ku\n",
            &["EscapeCanonical", "SpanCanonical"][..],
        ),
        (
            "linked-str",
            "TokenCanonical",
            "stage1/token.ku\n",
            &["EscapeCanonical"][..],
        ),
        (
            "linked-str",
            "Canonical",
            "stage1/token.ku\n",
            &["TokenCanonical"][..],
        ),
    ] {
        fs::write(cwd.join("modules.txt"), manifest).unwrap();
        let c = emitter(mode, name);
        let plan_mode = format!("{mode}-plan");
        let raw = emitter(&plan_mode, name);
        assert_eq!(raw, emitter(&plan_mode, name));
        let plan = decode(raw);
        assert_eq!(plan["schema"], Value::String("ku-bootstrap-link-plan-v1".into()));
        let Value::Object(export) = &plan["export"] else { panic!("expected one export") };
        assert_eq!(export["source_name"], Value::String(name.into()));
        if name == "Canonical" {
            let Value::Object(signature) = &export["signature"] else { panic!("expected signature") };
            let Value::Array(params) = &signature["parameters"] else { panic!("expected parameters") };
            let [Value::Object(array)] = params.as_slice() else { panic!("expected one array") };
            assert_eq!(array["kind"], Value::String("StructArray".into()));
            assert_eq!(array["source_name"], Value::String("Token".into()));
            assert_eq!(array["source_path"], export["source_path"]);
            assert_eq!(array["passing"], Value::String("value".into()));
            assert!(c.contains("element = a0.data[cursor++];"));
            assert!(c.contains("for (; cursor < a0.len; ++cursor)"));
        }
        let Value::Array(dependencies) = &plan["dependencies"] else { panic!("expected dependencies") };
        assert_eq!(dependencies.len(), wanted.len());
        for (dependency, expected) in dependencies.iter().zip(wanted) {
            let Value::Object(dependency) = dependency else { panic!("expected a dependency") };
            assert_eq!(dependency["source_name"], Value::String((*expected).into()));
            if name == "TokenCanonical" {
                assert_eq!(dependency["source_path"], export["source_path"]);
                assert_ne!(dependency["node"], export["node"]);
                assert!(c.contains("= a0.f0;\n    a0.f0 = (KuProjectionString){0};"));
                assert!(c.contains("ku_scalar_string_int(a0.f2, digits)"));
            }
            assert!(matches!(dependency["signature"], Value::Object(_)));
            if name == "RejectAtPoint" {
                let Value::Object(signature) = &dependency["signature"] else {
                    unreachable!("checked dependency signature")
                };
                assert_eq!(signature["returns_result"], Value::Bool(true));
            }
            let Value::String(symbol) = &dependency["symbol"] else { panic!("expected a symbol") };
            assert!(c.contains(&format!(" {symbol}(")));
        }
        assert_eq!(c, emitter(mode, name));
    }

    fs::write(
        cwd.join("provider.ku"),
        "fn Size(&text: str): int { return 4 }\nfn Complete(): null! { return ok(null) }\n",
    )
    .unwrap();
    for (mode, name, manifest, expected_params, result_name, result_path, wrapper) in [
        (
            "construct",
            "MakeDiagnostic",
            "stage2/diagnostic.ku\nstage2/span.ku\nstage1/token.ku\n",
            &[
                ("str", "", "value"),
                ("str", "", "value"),
                ("str", "", "value"),
                ("str", "", "value"),
                ("str", "", "value"),
                ("Span", "stage2/span.ku", "value"),
            ][..],
            "Diagnostic",
            "stage2/diagnostic.ku",
            false,
        ),
        (
            "project-fields",
            "SpanFromToken",
            "stage2/span.ku\nstage1/token.ku\n",
            &[("Token", "stage1/token.ku", "value")][..],
            "Span",
            "stage2/span.ku",
            false,
        ),
        (
            "guarded",
            "SpanCanonical",
            "stage2/span.ku\nstage1/token.ku\n",
            &[("Span", "stage2/span.ku", "value")][..],
            "str",
            "",
            false,
        ),
        (
            "project-int",
            "EscapeCanonical",
            "stage1/token.ku\n",
            &[("str", "", "value")][..],
            "str",
            "",
            false,
        ),
        (
            "project-int",
            "Size",
            "provider.ku\n",
            &[("str", "", "view")][..],
            "int",
            "",
            false,
        ),
        (
            "project-int",
            "Complete",
            "provider.ku\n",
            &[][..],
            "null",
            "",
            true,
        ),
    ] {
        fs::write(cwd.join("modules.txt"), manifest).unwrap();
        let plan = decode(emitter(&format!("{mode}-plan"), name));
        let c = emitter(mode, name);
        assert_eq!(plan["mode"], Value::String(mode.into()));
        assert_eq!(plan["dependencies"], Value::Array(vec![]));
        let Value::Object(export) = &plan["export"] else {
            panic!("expected an export")
        };
        assert_eq!(export["source_name"], Value::String(name.into()));
        assert_eq!(
            export["source_path"],
            Value::String(manifest.lines().next().unwrap().into())
        );
        let Value::String(symbol) = &export["symbol"] else {
            panic!("expected a symbol")
        };
        let Value::Object(signature) = &export["signature"] else {
            panic!("expected a signature")
        };
        assert_eq!(signature["returns_result"], Value::Bool(wrapper));
        let Value::Array(parameters) = &signature["parameters"] else {
            panic!("expected parameters")
        };
        assert_eq!(parameters.len(), expected_params.len());
        let c_type = |record: &Value, name: &str, path: &str, passing: &str| {
            let Value::Object(ty) = record else {
                panic!("expected a type")
            };
            assert_eq!(ty["source_name"], Value::String(name.into()));
            assert_eq!(ty["passing"], Value::String(passing.into()));
            if path.is_empty() {
                assert_eq!(ty["kind"], Value::String("Builtin".into()));
                assert_eq!(ty["source_path"], Value::Null);
            } else {
                assert_eq!(ty["kind"], Value::String("Struct".into()));
                assert_eq!(ty["source_path"], Value::String(path.into()));
            }
            let Value::String(c_type) = &ty["c_type"] else {
                panic!("expected a C type")
            };
            c_type.clone()
        };
        let result = c_type(&signature["result"], result_name, result_path, "value");
        let mut params = parameters
            .iter()
            .zip(expected_params)
            .enumerate()
            .map(|(index, (entry, (name, path, passing)))| {
                format!("{} a{index}", c_type(entry, name, path, passing))
            })
            .collect::<Vec<_>>()
            .join(", ");
        if params.is_empty() {
            params = "void".into();
        }
        assert!(
            c.replace(" *a", "* a")
                .contains(&format!("{result} {symbol}({params}) {{")),
            "incorrect C signature for {name}"
        );
    }

    for directory in ["left", "right"] {
        fs::create_dir(cwd.join(directory)).unwrap();
        fs::write(
            cwd.join(directory).join("same.ku"),
            "fn Convert(text: str): str { return text }\n",
        )
        .unwrap();
    }
    fs::write(
        cwd.join("duplicate.ku"),
        r#"import { Convert as Left } from "./left/same.ku"
import { Convert as Right } from "./right/same.ku"
struct Pair { left: str, right: str }
fn Display(pair: Pair): str { return Left(pair.left) + Right(pair.right) }
"#,
    )
    .unwrap();
    for manifest in [
        "duplicate.ku\nleft/same.ku\nright/same.ku\n",
        "duplicate.ku\nright/same.ku\nleft/same.ku\n",
    ] {
        fs::write(cwd.join("modules.txt"), manifest).unwrap();
        let plan = decode(emitter("linked-str-plan", "Display"));
        let Value::Array(deps) = &plan["dependencies"] else {
            panic!("expected dependencies")
        };
        let [Value::Object(left), Value::Object(right)] = deps.as_slice() else {
            panic!("supplier identities must stay distinct");
        };
        assert_eq!(left["source_path"], Value::String("left/same.ku".into()));
        assert_eq!(right["source_path"], Value::String("right/same.ku".into()));
        assert_eq!(left["source_name"], right["source_name"]);
        assert_eq!(left["node"], right["node"]);
        assert_ne!(left["symbol"], right["symbol"]);
    }
}

fn assert_constructor_parameter_ownership(seed: &Path, cwd: &Path) {
    let diagnostic = cwd.join("stage2/diagnostic.ku");
    fs::write(
        &diagnostic,
        include_str!("../bootstrap/stage2/diagnostic.ku"),
    )
    .unwrap();
    fs::write(cwd.join("mode.txt"), "construct").unwrap();
    fs::write(cwd.join("function.txt"), "MakeDiagnostic").unwrap();
    fs::write(
        cwd.join("modules.txt"),
        "stage2/diagnostic.ku\nstage2/span.ku\nstage1/token.ku\n",
    )
    .unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    assert!(!code.contains("malloc(") && !code.contains("free("));
    let signature = code
        .lines()
        .find(|line| line.contains(" ku_construct_") && line.ends_with(" {"))
        .unwrap();
    let (result, call) = signature.split_once(' ').unwrap();
    let (symbol, formals) = call.split_once('(').unwrap();
    let position = formals
        .rsplit(", ")
        .next()
        .unwrap()
        .strip_suffix(" a5) {")
        .unwrap();
    let driver = r#"
#include <stdlib.h>
int main(void) {
    KuProjectionString args[5];
    for (size_t i = 0; i < 5; ++i) {
        uint8_t *memory = malloc(1);
        if (!memory) return 1;
        *memory = (uint8_t)i;
        args[i] = (KuProjectionString){memory, 1, 1, KU_PROJECTION_STRING_OWNED};
    }
    DIAGNOSTIC value = FUNCTION(args[0], args[1], args[2], args[3], args[4], (POSITION){.f0=7});
    if (value.f0.ptr != args[0].ptr || value.f1.ptr != args[1].ptr ||
        value.f2.ptr != args[2].ptr || value.f3.ptr != args[3].ptr ||
        value.f4.ptr != args[4].ptr || value.f5.f0 != 7) return 2;
    free(value.f0.ptr); free(value.f1.ptr); free(value.f2.ptr);
    free(value.f3.ptr); free(value.f4.ptr);
    return 0;
}
"#
    .replace("DIAGNOSTIC", result)
    .replace("FUNCTION", symbol)
    .replace("POSITION", position);
    let source = cwd.join("construct-owners.c");
    fs::write(&source, format!("{code}{driver}")).unwrap();
    let binary = cwd.join(format!("construct-owners{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&source, &binary, cwd);
    assert_ok(&execute(&mut Command::new(binary), cwd, 15));

    fs::write(
        cwd.join("bad-owner.ku"),
        "struct Pair { a: str b: str }\nfn Reuse(value: str): Pair { return Pair { a: value, b: value } }\n",
    )
    .unwrap();
    fs::write(cwd.join("modules.txt"), "bad-owner.ku\n").unwrap();
    fs::write(cwd.join("function.txt"), "Reuse").unwrap();
    let rejected = execute(&mut Command::new(seed), cwd, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("constructor_parameter_moved"));

    fs::write(
        cwd.join("bad-owner.ku"),
        "struct Spare { label: str }\nstruct Output { item: str }\n\
         fn Select(spare: Spare, keep: str): Output { return Output { item: keep } }\n",
    )
    .unwrap();
    fs::write(cwd.join("function.txt"), "Select").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let signature = code
        .lines()
        .find(|line| line.contains(" ku_construct_"))
        .unwrap();
    let (result, declaration) = signature.split_once(' ').unwrap();
    let (symbol, arguments) = declaration.split_once('(').unwrap();
    let (spare, _) = arguments.split_once(' ').unwrap();
    let shim = r#"
#include <stdlib.h>
#include <assert.h>
static void *unused;
static unsigned retired;
static void observe_free(void *memory) {
    assert(memory == unused);
    ++retired;
    free(memory);
}
#define free observe_free
"#;
    let driver = r#"
#undef free
int main(void) {
    unused=malloc(1);
    uint8_t *kept=malloc(1);
    assert(unused && kept);
    SPARE spare={.f0=(KuProjectionString){unused, 1, 1, KU_PROJECTION_STRING_OWNED}};
    OUTPUT value=FUNCTION(spare, (KuProjectionString){kept, 1, 1, KU_PROJECTION_STRING_OWNED});
    assert(retired==1 && value.f0.ptr==kept);
    free(value.f0.ptr);
    return 0;
}
"#
    .replace("SPARE", spare)
    .replace("OUTPUT", result)
    .replace("FUNCTION", symbol);
    fs::write(&source, format!("{shim}{code}{driver}")).unwrap();
    let binary = cwd.join(format!("unused-owner{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&source, &binary, cwd);
    assert_ok(&execute(&mut Command::new(binary), cwd, 15));
}

fn assert_projected_span(ku: &Path, seed: &Path, cwd: &Path, source: &str) {
    fs::create_dir_all(cwd.join("stage1")).unwrap();
    fs::create_dir_all(cwd.join("stage2")).unwrap();
    fs::write(
        cwd.join("stage1/token.ku"),
        include_str!("../bootstrap/stage1/token.ku"),
    )
    .unwrap();
    let span = cwd.join("stage2/span.ku");
    fs::write(
        &span,
        format!(
            "{source}{}",
            r#"
fn main() {
    span = Span { line: 7, column: 4, offset: -9223372036854775807 - 1,
                  end_line: 8, end_column: 1, end_offset: 9223372036854775807 }
    println(SpanCanonical(span))
}
"#
        ),
    )
    .unwrap();
    let oracle = execute(Command::new(ku).arg("run").arg(&span), cwd, 30);
    assert_ok(&oracle);
    assert_eq!(
        String::from_utf8_lossy(&oracle.stdout).trim(),
        "7:4@-9223372036854775808..8:1@9223372036854775807"
    );
    fs::write(&span, source).unwrap();
    fs::write(cwd.join("mode.txt"), "guarded").unwrap();
    fs::write(cwd.join("function.txt"), "SpanCanonical").unwrap();
    fs::write(cwd.join("modules.txt"), "stage2/span.ku\nstage1/token.ku\n").unwrap();
    let emitted = execute(&mut Command::new(seed), cwd, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let declaration = code
        .lines()
        .find_map(|line| line.strip_prefix("KuProjectionString ku_guarded_"))
        .expect("standalone C signature");
    let (id, args) = declaration.split_once('(').unwrap();
    let (span_type, _) = args.split_once(' ').unwrap();
    assert_eq!(code.matches("data = malloc(").count(), 1);
    let driver = r#"
int main(void) {
    SPAN input = { .f0 = 7, .f1 = 4, .f2 = INT64_MIN,
                   .f3 = 8, .f4 = 1, .f5 = INT64_MAX };
    KuProjectionString output = FUNCTION(input);
    if (output.storage != KU_PROJECTION_STRING_OWNED) return 2;
    printf("%.*s\n", (int)output.len, (const char*)output.ptr);
    free(output.ptr);
    return 0;
}
"#
    .replace("SPAN", span_type)
    .replace("FUNCTION", &format!("ku_guarded_{id}"));
    let c = cwd.join("projected-span.c");
    fs::write(&c, format!("{code}{driver}")).unwrap();
    let exe = cwd.join(format!("projected-span{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&c, &exe, cwd);
    let observed = execute(&mut Command::new(exe), cwd, 15);
    assert_ok(&observed);
    assert_eq!(observed.stdout, oracle.stdout);
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
static void *live[96];
static int attempts;
static int fail_after = -1;
static int slot_for(void *pointer) {
    for (int i = 0; i < 96; ++i) if (live[i] == pointer) return i;
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
    for (int i = 0; i < 96; ++i) count += live[i] != NULL;
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

const RELOCATE_EMITTER: &str = r#"import "std.fs"
import { BindProjectImports } from "./imports.ku"
import { BindImportedTypeNames } from "./type_names.ku"
import { BuildStructLayouts } from "./struct_layouts.ku"
import { ReadProjectModules } from "./project_manifest.ku"
import { EmitManagedArrayC } from "./managed_array_c.ku"
fn main(): null! {
    supplied = ReadProjectModules(["stage2/context.ku", "stage2/span.ku", "stage1/token.ku"])?
    imports = BindProjectImports(supplied[0], supplied)?
    bound = BindImportedTypeNames(supplied[0].named, imports, supplied)?
    layouts = BuildStructLayouts(supplied[0].named, bound, supplied)?
    c = EmitManagedArrayC(supplied[0].named, bound, imports, supplied, layouts, fs.read("target.txt")?)?
    print(c.source)
    return ok(null)
}
"#;

const RELOCATE_ORACLE: &str = r#"import { Scan } from "./stage1/lexer.ku"
import { Canonical, Token } from "./stage1/token.ku"
import { DefaultParseContext, RelocateTokens } from "./stage2/context.ku"
fn main(): null! {
    tokens = Scan("plan = \"任务🙂\"\ntrue")?
    println("INPUT\n" + Canonical(tokens.clone()))
    println("UNCHANGED\n" + Canonical(RelocateTokens(tokens.clone(), DefaultParseContext())))
    context = DefaultParseContext()
    context.origin.line = 4
    context.origin.column = 9
    context.origin.offset = 80
    println("SHIFT\n" + Canonical(RelocateTokens(tokens.clone(), context.clone())))
    context.has_boundary = true
    context.boundary.line = 9
    context.boundary.column = 12
    context.boundary.offset = 500
    context.boundary.end_line = 10
    context.boundary.end_column = 4
    context.boundary.end_offset = 510
    println("BOUNDARY\n" + Canonical(RelocateTokens(tokens, context.clone())))
    empty: [Token] = []
    println("EMPTY\n" + Canonical(RelocateTokens(empty, context)))
    return ok(null)
}
"#;

const RELOCATE_CONSUMER: &str = r#"
static KuProjectionString make_owned(const char *text) {
    size_t len = strlen(text);
    uint8_t *ptr = malloc(len ? len : 1);
    if (len) memcpy(ptr, text, len);
    return (KuProjectionString){ ptr, len, len ? len : 1, KU_PROJECTION_STRING_OWNED };
}
static __ARRAY__ make_input(void) {
    __ARRAY__ array = { __COUNT__, malloc(__COUNT__ * sizeof(__TOKEN__)), __COUNT__ };
__INPUT__
    return array;
}
static __CONTEXT__ context_for(int variant) {
    __CONTEXT__ context = {0};
    context.f0 = make_owned("bootstrap.parser");
    context.f1.f0 = variant ? 4 : 1;
    context.f1.f1 = variant ? 9 : 1;
    context.f1.f2 = variant ? 80 : 0;
    context.f1.f3 = 1;
    context.f1.f4 = 1;
    context.f2.f0 = 1;
    context.f2.f1 = 1;
    context.f2.f3 = 1;
    context.f2.f4 = 1;
    if (variant >= 2) {
        context.f3 = true;
        context.f2.f0 = 9;
        context.f2.f1 = 12;
        context.f2.f2 = 500;
        context.f2.f3 = 10;
        context.f2.f4 = 4;
        context.f2.f5 = 510;
    }
    return context;
}
static void canonical(const char *label, __ARRAY__ *result) {
    puts(label);
    for (size_t i = 0; i < result->len; ++i) {
        __TOKEN__ *t = &result->data[i];
        printf("%.*s|%.*s|%lld|%lld|%lld|%lld|%lld|%lld|%lld\n",
            (int)t->f0.len, (char*)t->f0.ptr, (int)t->f1.len, (char*)t->f1.ptr,
            (long long)t->f2, (long long)t->f3, (long long)t->f4, (long long)t->f5,
            (long long)t->f6, (long long)t->f7, (long long)t->f8);
    }
    if (!result->len) puts("");
}
static void check_clean(void) {
    if (outstanding() != 0) _Exit(97);
}
int main(int argc, char **argv) {
    assert(atexit(check_clean) == 0);
    if (argc == 2) {
        __ARRAY__ input = make_input();
        __CONTEXT__ context = context_for(2);
        if (strcmp(argv[1], "overflow") == 0) context.f1.f2 = INT64_MAX;
        else fail_after = atoi(argv[1]);
        (void)__ENTRY__(input, context);
        return 98;
    }
    const char *labels[] = {"UNCHANGED", "SHIFT", "BOUNDARY", "EMPTY"};
    for (int mode = 0; mode < 4; ++mode) {
        __ARRAY__ input = {0};
        if (mode != 3) input = make_input();
        __TOKEN__ *origin = input.data;
        __CONTEXT__ context = context_for(mode);
        int initial_attempts = attempts;
        __ARRAY__ result = __ENTRY__(input, context);
        if (mode == 0) assert(origin == result.data && attempts == initial_attempts);
        if (mode == 2) fprintf(stderr, "%d\n", attempts - initial_attempts);
        canonical(labels[mode], &result);
        __PREFIX___drop(&result);
        assert(!outstanding());
    }
    return 0;
}
"#;

fn c_token_bytes(value: &str) -> String {
    let escaped: String = value.bytes().map(|byte| format!("\\{byte:03o}")).collect();
    format!("make_owned(\"{escaped}\")")
}

#[test]
fn bootstrap_compiles_unmodified_relocate_tokens_with_owned_cleanup() {
    let work = Temp::new();
    let compiler = work.0.join("compiler");
    let project = work.0.join("project");
    fs::create_dir(&compiler).unwrap();
    fs::create_dir(&project).unwrap();
    stage_sources(&compiler);
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let exe = format!("relocate-emitter{}", std::env::consts::EXE_SUFFIX);
    let entry = compiler.join("semantics/relocate-emitter.ku");
    let built = compiler.join("semantics").join(&exe);
    let emitter = project.join(exe);
    fs::write(&entry, RELOCATE_EMITTER).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&built),
        &project,
        150,
    ));
    fs::rename(built, &emitter).unwrap();
    for stage in ["stage1", "stage2"] {
        fs::rename(compiler.join(stage), project.join(stage)).unwrap();
    }
    fs::remove_dir_all(&compiler).unwrap();

    let oracle = project.join("oracle.ku");
    let expected_exe = project.join(format!("reference{}", std::env::consts::EXE_SUFFIX));
    fs::write(&oracle, RELOCATE_ORACLE).unwrap();
    let result = execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(oracle)
            .arg("-o")
            .arg(&expected_exe),
        &project,
        90,
    );
    assert_ok(&result);
    let observed = execute(&mut Command::new(expected_exe), &project, 20);
    assert_ok(&observed);
    let expected = String::from_utf8(observed.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    let (input, other) = expected
        .strip_prefix("INPUT\n")
        .unwrap()
        .split_once("UNCHANGED\n")
        .unwrap();
    let expected = format!("UNCHANGED\n{other}");
    let mut initializer = String::new();
    let mut count = 0;
    for line in input.lines() {
        let fields: Vec<_> = line.split('|').collect();
        let [kind, lexeme, rest @ ..] = fields.as_slice() else {
            panic!("canonical input token")
        };
        assert_eq!(rest.len(), 7);
        let values: Vec<_> = rest
            .iter()
            .map(|item| item.parse::<i64>().expect("token integer").to_string())
            .collect();
        initializer.push_str(&format!(
            "    array.data[{count}] = (__TOKEN__){{ {}, {}, {} }};\n",
            c_token_bytes(kind),
            c_token_bytes(lexeme),
            values.join(", ")
        ));
        count += 1;
    }
    assert_eq!(count, 5);
    assert!(expected.contains("String|任务🙂") && expected.contains("Eof||0|9|12|500|10|4|510"));

    fs::write(project.join("target.txt"), "RelocateTokens").unwrap();
    let emitted = execute(&mut Command::new(&emitter), &project, 30);
    assert_ok(&emitted);
    let code = String::from_utf8(emitted.stdout).unwrap();
    let signature = code
        .lines()
        .find(|line| line.contains(" ku_managed_"))
        .unwrap();
    let (array, function) = signature.split_once(' ').unwrap();
    let (symbol, signature) = function.split_once('(').unwrap();
    let (_, context) = signature.split_once(", ").unwrap();
    let context = context.strip_suffix(" a1) {").unwrap();
    let token = array.strip_prefix("KuArray_").unwrap();
    let prefix = format!("ku_owned_array_{token}");
    let driver = RELOCATE_CONSUMER
        .replace("__INPUT__", &initializer)
        .replace("__COUNT__", &count.to_string())
        .replace("__ARRAY__", array)
        .replace("__TOKEN__", token)
        .replace("__CONTEXT__", context)
        .replace("__ENTRY__", symbol)
        .replace("__PREFIX__", &prefix);
    let executable = project.join(format!("relocated{}", std::env::consts::EXE_SUFFIX));
    let source = project.join("relocated.c");
    fs::write(&source, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{driver}")).unwrap();
    let bounds = write_managed_bounds_case(&project, &emitter);
    for stage in ["stage1", "stage2"] {
        fs::remove_dir_all(project.join(stage)).unwrap();
    }
    fs::remove_file(project.join("oracle.ku")).unwrap();
    compile_projection(&source, &executable, &project);
    let result = execute(&mut Command::new(&executable), &project, 20);
    assert_ok(&result);
    assert_eq!(
        String::from_utf8(result.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        expected
    );
    let allocation_count: usize = String::from_utf8(result.stderr)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(allocation_count > 10 && allocation_count <= 64);
    for fail_after in 0..allocation_count {
        let trap = execute(
            Command::new(&executable).arg(fail_after.to_string()),
            &project,
            5,
        );
        assert_eq!(
            trap.status.code(),
            Some(1),
            "injected allocation failure {fail_after}: {}",
            String::from_utf8_lossy(&trap.stderr)
        );
        assert!(String::from_utf8_lossy(&trap.stderr).contains("array allocation failed"));
    }
    let overflow = execute(Command::new(executable).arg("overflow"), &project, 5);
    assert_eq!(overflow.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&overflow.stderr).contains("integer overflow"));
    let bounds_exe = project.join(format!("bounds{}", std::env::consts::EXE_SUFFIX));
    compile_projection(&bounds, &bounds_exe, &project);
    assert_ok(&execute(&mut Command::new(&bounds_exe), &project, 10));
    let trap = execute(Command::new(&bounds_exe).arg("-1"), &project, 5);
    assert_eq!(trap.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&trap.stderr).trim_end(),
        "array/index_out_of_bounds: index -1 out of bounds for length 1"
    );
}

fn write_managed_bounds_case(project: &Path, emitter: &Path) -> PathBuf {
    fs::write(
        project.join("stage2/context.ku"),
        r#"
struct Entry { text: str score: int }
fn Take(source: [Entry], idx: int): [Entry] {
    item = source[idx].clone()
    result: [Entry] = []
    result = result.push(item)
    return result
}
"#,
    )
    .unwrap();
    fs::write(project.join("target.txt"), "Take").unwrap();
    let result = execute(&mut Command::new(emitter), project, 25);
    assert_ok(&result);
    let code = String::from_utf8(result.stdout).unwrap();
    let signature = code
        .lines()
        .find(|line| line.contains(" ku_managed_"))
        .unwrap();
    let (array, function) = signature.split_once(' ').unwrap();
    let (symbol, _) = function.split_once('(').unwrap();
    let item = array.strip_prefix("KuArray_").unwrap();
    let driver = r#"
static void check_clean(void) {
    if (outstanding()) _Exit(97);
}
int main(int argc, char **argv) {
    assert(atexit(check_clean) == 0);
    __ARRAY__ input = { 1, malloc(sizeof(__ITEM__)), 1 };
    uint8_t *bytes = malloc(1);
    bytes[0] = 'x';
    input.data[0] = (__ITEM__){ { bytes, 1, 1, KU_PROJECTION_STRING_OWNED }, 42 };
    __ARRAY__ output = __ENTRY__(input, argc == 1 ? 0 : atoi(argv[1]));
    assert(output.len == 1 && output.data[0].f1 == 42 && output.data[0].f0.ptr[0] == 'x');
    __PREFIX___drop(&output);
    assert(!outstanding());
    return 0;
}
"#
    .replace("__ARRAY__", array)
    .replace("__ITEM__", item)
    .replace("__ENTRY__", symbol)
    .replace("__PREFIX__", &format!("ku_owned_array_{item}"));
    let path = project.join("managed_bounds.c");
    fs::write(&path, format!("{CHECKED_ARRAY_ALLOCATOR}{code}{driver}")).unwrap();
    path
}

#[test]
fn bootstrap_emits_typed_dependencies_for_real_parser_reject() {
    let tmp = Temp::new();
    let staged = tmp.0.join("compiler");
    let runtime = tmp.0.join("consumer");
    fs::create_dir(&staged).unwrap();
    fs::create_dir(&runtime).unwrap();
    stage_sources(&staged);
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let script = staged.join("semantics/linked_reject.ku");
    fs::write(
        &script,
        r#"import { ReadProjectModules } from "./project_manifest.ku"
import { EmitLinkedNullC } from "./linked_null_c.ku"
fn main(): null! {
    supplied = ReadProjectModules(["stage2/expression_support.ku", "stage2/span.ku", "stage2/diagnostic.ku", "stage1/token.ku", "stage2/ast.ku"])?
    generated = EmitLinkedNullC(supplied, "RejectAt")?
    print(generated.source)
    return ok(null)
}
"#,
    )
    .unwrap();
    let name = format!("emit-reject{}", std::env::consts::EXE_SUFFIX);
    let built = staged.join("semantics").join(&name);
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&script)
            .arg("-o")
            .arg(&built),
        &runtime,
        150,
    ));
    let emitter = runtime.join(name);
    fs::rename(built, &emitter).unwrap();
    for stage in ["stage1", "stage2"] {
        fs::rename(staged.join(stage), runtime.join(stage)).unwrap();
    }
    fs::remove_dir_all(&staged).unwrap();
    let emitted = execute(&mut Command::new(emitter), &runtime, 30);
    assert_ok(&emitted);
    let source = String::from_utf8(emitted.stdout).unwrap();
    for stage in ["stage1", "stage2"] {
        fs::remove_dir_all(runtime.join(stage)).unwrap();
    }
    let dependencies: Vec<_> = source
        .lines()
        .filter(|line| line.starts_with("extern "))
        .collect();
    assert_eq!(dependencies.len(), 3);
    assert!(dependencies
        .iter()
        .all(|line| line.contains("ku_value_dependency_m")));
    assert!(dependencies
        .iter()
        .any(|line| line.matches("KuProjectionString").count() == 5));
    assert!(
        source.contains("result.error.domain = ")
            && source.contains("result.error.code = ")
            && source.contains("result.error.message = ")
            && !source.contains("__ku_import")
    );
    let path = runtime.join("reject.c");
    fs::write(&path, source).unwrap();
    compile_projection_product(&path, &runtime.join("reject.obj"), &runtime, true, true);
}

const PRIMARY_EMITTER: &str = r#"import { BindProjectImports } from "./imports.ku"
import { BindImportedTypeNames } from "./type_names.ku"
import { BuildStructLayouts } from "./struct_layouts.ku"
import { ReadProjectModules } from "./project_manifest.ku"
import { EmitStructResultC } from "./struct_result_c.ku"
fn main(): null! {
    supplied = ReadProjectModules(["stage2/expression_support.ku", "stage2/span.ku", "stage1/token.ku", "stage2/ast.ku", "stage2/diagnostic.ku"])?
    imports = BindProjectImports(supplied[0], supplied)?
    bound = BindImportedTypeNames(supplied[0].named, imports, supplied)?
    layouts = BuildStructLayouts(supplied[0].named, bound, supplied)?
    c = EmitStructResultC(supplied[0].named, bound, imports, supplied, layouts, "ReadPrimaryAtom")?
    println(c.symbol + "|" + c.dependency)
    print(c.source)
    return ok(null)
}
"#;

const PRIMARY_ORACLE: &str = r#"import { Token } from "./stage1/token.ku"
import { ReadPrimaryAtom } from "./stage2/expression_support.ku"
fn observe(kind: str, stage3_domain: bool): null {
    token = Token { kind: kind, lexeme: "任务🙂", int_value: 703, line: 4, column: 9, offset: 51,
                     end_line: 4, end_column: 12, end_offset: 59 }
    try {
        value = ReadPrimaryAtom(token, stage3_domain)?
        println("OK|" + value.kind + "|" + value.text + "|" + str(value.int_value))
    } catch(err) {
        println("ERR|" + err.domain + "|" + err.code + "|" + err.message)
    }
    return null
}
fn main(): null! {
    for kind in ["Int", "Float", "String", "TemplateString", "True", "False", "Null", "Ident", "Plus"] {
        observe(kind, false)
    }
    observe("Plus", true)
    return ok(null)
}
"#;

const PRIMARY_ABI_BRIDGE: &str = r#"
_Static_assert(sizeof(KuProjectionString) == sizeof(KuString), "string ABI size");
_Static_assert(offsetof(KuProjectionString, storage) == offsetof(KuString, storage), "string ABI tag");
_Static_assert(sizeof(KuBootstrapError) == sizeof(KuError), "error ABI size");
_Static_assert(sizeof(KuBootstrapResultNull) == sizeof(KuResult_null), "null Result ABI size");
_Static_assert(sizeof(__RESULT__) == sizeof(__STAGE0_RESULT__), "struct Result ABI size");
_Static_assert(offsetof(__RESULT__, error) == offsetof(__STAGE0_RESULT__, error), "struct Result error offset");

static unsigned calls_to_stage0;
static KuString into_stage0(KuProjectionString value) {
    return (KuString){value.ptr, value.len, value.capacity, value.storage};
}
static KuProjectionString from_stage0(KuString value) {
    return (KuProjectionString){value.ptr, value.len, value.capacity, value.storage};
}

// The only linked dependency is the real Stage0-generated RejectAt.
// Typed aggregate construction transfers each field to its actual owner.
KuBootstrapResultNull __DEPENDENCY__(__TOKEN__ token, KuProjectionString code,
                                    KuProjectionString message, bool stage3_domain) {
    ++calls_to_stage0;
    __STAGE0_TOKEN__ input = {
        .kind = into_stage0(token.f0), .lexeme = into_stage0(token.f1), .int_value = token.f2,
        .line = token.f3, .column = token.f4, .offset = token.f5,
        .end_line = token.f6, .end_column = token.f7, .end_offset = token.f8
    };
    KuResult_null result = __REJECT__(input, into_stage0(code), into_stage0(message), stage3_domain);
    KuBootstrapError error = { from_stage0(result.error.domain), from_stage0(result.error.code), from_stage0(result.error.message) };
    return (KuBootstrapResultNull){ result.ok, result.value, error };
}

static KuProjectionString make_owned(const uint8_t *text, size_t len) {
    uint8_t *ptr = malloc(len ? len : 1);
    if (len) memcpy(ptr, text, len);
    return (KuProjectionString){ ptr, len, len ? len : 1, KU_PROJECTION_STRING_OWNED };
}
static __TOKEN__ token_for(const char *kind) {
    static const uint8_t content[] = {0xe4,0xbb,0xbb,0xe5,0x8a,0xa1,0xf0,0x9f,0x99,0x82};
    return (__TOKEN__){ make_owned((const uint8_t*)kind, strlen(kind)), make_owned(content, sizeof(content)),
        703, 4, 9, 51, 4, 12, 59 };
}
static void check_clean(void) {
    if (outstanding()) _Exit(97);
}
static void print_carrier(KuProjectionString value) {
    if (value.len) printf("%.*s", (int)value.len, (char*)value.ptr);
}
int main(int argc, char **argv) {
    assert(atexit(check_clean) == 0);
    if (argc == 2) {
        __TOKEN__ token = token_for("Float");
        fail_after = atoi(argv[1]);
        (void)__ENTRY__(token, false);
        return 98;
    }
    const char *kinds[] = { "Int", "Float", "String", "TemplateString", "True", "False", "Null", "Ident", "Plus", "Plus" };
    for (unsigned i = 0; i < sizeof(kinds)/sizeof(kinds[0]); ++i) {
        __RESULT__ returned = __ENTRY__(token_for(kinds[i]), i == 9);
        __RESULT__ result = __ENTRY___move(&returned);
        __ENTRY___drop(&returned);
        if (i < 8) {
            assert(result.ok && !result.error.message.ptr && calls_to_stage0 == 0);
            int owners = i >= 1 && i <= 3 ? 2 : i == 7 ? 1 : 0;
            assert(outstanding() == owners);
            fputs("OK|", stdout); print_carrier(result.value.f0);
            putchar('|'); print_carrier(result.value.f1);
            printf("|%lld\n", (long long)result.value.f2);
        } else {
            assert(!result.ok && !result.value.f0.ptr && !result.value.f1.ptr && calls_to_stage0 == i - 7);
            assert(result.error.message.storage == KU_PROJECTION_STRING_OWNED && outstanding() == 1);
            fputs("ERR|", stdout); print_carrier(result.error.domain);
            putchar('|'); print_carrier(result.error.code);
            putchar('|'); print_carrier(result.error.message); putchar('\n');
        }
        __ENTRY___drop(&result);
        __ENTRY___drop(&result);
        assert(outstanding() == 0);
    }
    return 0;
}
"#;

#[test]
fn bootstrap_compiles_primary_atom_with_actual_stage0_error_dependency() {
    let tmp = Temp::new();
    let staged = tmp.0.join("compiler");
    let cwd = tmp.0.join("consumer");
    fs::create_dir(&staged).unwrap();
    fs::create_dir(&cwd).unwrap();
    stage_sources(&staged);
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let exe = format!("emit-primary{}", std::env::consts::EXE_SUFFIX);
    let built = staged.join("semantics").join(&exe);
    let emitter = cwd.join(exe);
    let entry = staged.join("semantics/emit_primary.ku");
    fs::write(&entry, PRIMARY_EMITTER).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&entry)
            .arg("-o")
            .arg(&built),
        &cwd,
        150,
    ));
    fs::rename(built, &emitter).unwrap();
    for stage in ["stage1", "stage2"] {
        fs::rename(staged.join(stage), cwd.join(stage)).unwrap();
    }
    fs::remove_dir_all(&staged).unwrap();

    let origin = cwd.join("oracle.ku");
    let oracle = cwd.join(format!("stage0-oracle{}", std::env::consts::EXE_SUFFIX));
    fs::write(&origin, PRIMARY_ORACLE).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&origin)
            .arg("-o")
            .arg(&oracle),
        &cwd,
        90,
    ));
    let result = execute(&mut Command::new(oracle), &cwd, 20);
    assert_ok(&result);
    let expected = String::from_utf8(result.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        expected.contains("OK|String|任务🙂|0")
            && expected.contains("ERR|bootstrap.parser.stage3|unsupported_expression|")
    );
    assert!(expected.contains("4:9@51..4:12@59"));
    assert_ok(&execute(
        Command::new(&ku).args(["build", "--native"]).arg(&origin),
        &cwd,
        30,
    ));
    let stage0_code = fs::read_to_string(cwd.join("oracle.c")).unwrap();
    let reject = stage0_code
        .lines()
        .find(|line| {
            line.starts_with("KuResult_null ")
                && line.contains("_RejectAt(")
                && line.ends_with(" {")
        })
        .unwrap();
    let (reject, declared) = reject
        .trim_start_matches("KuResult_null ")
        .split_once('(')
        .unwrap();
    let (stage0_token, _) = declared.split_once(" token, ").unwrap();
    let stage0_result = stage0_code
        .lines()
        .find(|line| line.contains("_ReadPrimaryAtom(") && line.ends_with(" {"))
        .unwrap()
        .split_once(' ')
        .unwrap()
        .0;

    let result = execute(&mut Command::new(emitter), &cwd, 30);
    assert_ok(&result);
    let emitted = String::from_utf8(result.stdout).unwrap();
    let (symbols, code) = emitted.split_once('\n').unwrap();
    let (entry, dependency) = symbols.trim_end().split_once('|').unwrap();
    let signature = code
        .lines()
        .find(|line| line.contains(&format!(" {entry}(")))
        .unwrap();
    let (result_type, declaration) = signature.split_once(' ').unwrap();
    let (_, arguments) = declaration.split_once('(').unwrap();
    let (token, _) = arguments.split_once(" a0,").unwrap();
    assert!(code.contains(&format!("extern KuBootstrapResultNull {dependency}(")));

    let strict_file = cwd.join("primary.c");
    fs::write(&strict_file, code).unwrap();
    let consumer = PRIMARY_ABI_BRIDGE
        .replace("__RESULT__", result_type)
        .replace("__STAGE0_RESULT__", stage0_result)
        .replace("__STAGE0_TOKEN__", stage0_token)
        .replace("__TOKEN__", token)
        .replace("__DEPENDENCY__", dependency)
        .replace("__REJECT__", reject)
        .replace("__ENTRY__", entry);
    let linked = cwd.join("primary_linked.c");
    let stage0_adapter =
        format!("\n#define main ku_stage0_original_main\n{stage0_code}\n#undef main\n");
    fs::write(
        &linked,
        format!("{CHECKED_ARRAY_ALLOCATOR}{stage0_adapter}\n{code}{consumer}"),
    )
    .unwrap();
    for stage in ["stage1", "stage2"] {
        fs::remove_dir_all(cwd.join(stage)).unwrap();
    }
    fs::remove_file(cwd.join("oracle.c")).unwrap();
    fs::remove_file(origin).unwrap();
    compile_projection_product(&strict_file, &cwd.join("primary.o"), &cwd, true, true);
    let binary = cwd.join(format!("primary-linked{}", std::env::consts::EXE_SUFFIX));
    // Stage0 is compiled by its normal C11 O2 options. The strict compiler
    // gate above already checked the new artifact independently.
    compile_projection_product(&linked, &binary, &cwd, false, false);
    let observed = execute(&mut Command::new(&binary), &cwd, 20);
    assert_ok(&observed);
    assert_eq!(
        String::from_utf8(observed.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        expected
    );
    for fail_at in ["0", "1"] {
        let trap = execute(Command::new(&binary).arg(fail_at), &cwd, 10);
        assert_eq!(
            trap.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&trap.stderr)
        );
        assert!(String::from_utf8_lossy(&trap.stderr).contains("out of memory"));
    }
}

#[test]
fn bootstrap_links_owned_array_results_and_cleans_up_checked_locals() {
    let tmp = Temp::new();
    let copied = tmp.0.join("source");
    let cwd = tmp.0.join("consumer");
    fs::create_dir(&copied).unwrap();
    fs::create_dir(&cwd).unwrap();
    stage_sources(&copied);
    let suffix = std::env::consts::EXE_SUFFIX;
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let semantics = copied.join("semantics");
    let built = semantics.join(format!("ku-project{suffix}"));
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release", "--offline"])
            .arg(semantics.join("projection_main.ku"))
            .arg("-o")
            .arg(&built),
        &cwd,
        180,
    ));
    let compiler = cwd.join(format!("ku-project{suffix}"));
    fs::rename(built, &compiler).unwrap();
    fs::remove_dir_all(&copied).unwrap();
    fs::write(
        cwd.join("types.ku"),
        "struct Entry { value: int label: str }\n",
    )
    .unwrap();
    fs::write(
        cwd.join("producer.ku"),
        r#"import { Entry } from "./types.ku"
fn Read(flag: int): [Entry]! {
    values: [Entry] = []
    element = Entry { value: flag, label: "owned🙂" }
    values = values.push(element)
    if (flag < 0) { fail { domain: "read", code: "negative", message: "denied🙂" } }
    return ok(values)
}
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("consumer.ku"),
        r#"import { Read as Load } from "./producer.ku"
fn Inspect(flag: int): null! {
    first = Load(0)?
    second = Load(flag)?
    final = Load(first[0].value)?
    return ok(null)
}
"#,
    )
    .unwrap();

    let emit = |manifest: &str, function: &str, mode: &str| {
        fs::write(cwd.join("modules.txt"), manifest).unwrap();
        fs::write(cwd.join("function.txt"), function).unwrap();
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        let emitted = execute(&mut Command::new(&compiler), &cwd, 30);
        assert_ok(&emitted);
        let c = cwd.join(format!("{function}.c"));
        fs::write(&c, emitted.stdout).unwrap();
        let plan = cwd.join(format!("{function}.json"));
        fs::write(cwd.join("mode.txt"), format!("{mode}-plan")).unwrap();
        let emitted = execute(&mut Command::new(&compiler), &cwd, 30);
        assert_ok(&emitted);
        fs::write(&plan, &emitted.stdout).unwrap();
        (plan, c, String::from_utf8(emitted.stdout).unwrap())
    };
    let producer = emit("producer.ku\ntypes.ku\nconsumer.ku\n", "Read", "managed");
    let consumer = emit(
        "consumer.ku\nproducer.ku\ntypes.ku\n",
        "Inspect",
        "linked-null",
    );
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(consumer.2)], Span::default()).unwrap()
    else {
        panic!("expected checked array link metadata");
    };
    let Value::Object(plan) = *value else {
        panic!("expected linked plan")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected consumer export")
    };
    let Value::Int(node) = export["node"] else {
        panic!("expected export node")
    };
    let Value::String(symbol) = &export["symbol"] else {
        panic!("expected export symbol")
    };
    assert!(producer.2.contains("\"kind\":\"StructArray\""));
    assert!(producer.2.contains("\"source_path\":\"types.ku\""));
    assert!(producer.2.contains("\"returns_result\":true"));

    let allocator = CHECKED_ARRAY_ALLOCATOR
        .replace("static void *test_", "void *test_")
        .replace("static void test_free", "void test_free")
        .replace("static int outstanding", "int outstanding");
    let allocator_source = cwd.join("ownership.c");
    fs::write(
        &allocator_source,
        allocator + "\nint completed_attempts(void) { return attempts; }\n",
    )
    .unwrap();
    let caller = cwd.join("caller.c");
    fs::write(
        &caller,
        r#"#include <assert.h>
extern int outstanding(void), completed_attempts(void);
int main(void) {
    KuBootstrapResultNull pass = __ENTRY__(1);
    assert(pass.ok && !outstanding() && completed_attempts() == 3);
    puts("ok");
    KuBootstrapResultNull refused = __ENTRY__(-1);
    assert(!refused.ok && !outstanding() && completed_attempts() == 5);
    printf("%.*s:%.*s:%.*s\n",
        (int)refused.error.domain.len, (char*)refused.error.domain.ptr,
        (int)refused.error.code.len, (char*)refused.error.code.ptr,
        (int)refused.error.message.len, (char*)refused.error.message.ptr);
    return 0;
}
"#
        .replace("__ENTRY__", symbol),
    )
    .unwrap();

    let binary = cwd.join(format!("array-result{suffix}"));
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap/tools/link_c.py");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let cc = std::env::var("CC").unwrap_or_else(|_| {
        if cfg!(windows) {
            "clang".into()
        } else {
            "cc".into()
        }
    });
    let linked = execute(
        Command::new(python)
            .arg("-B")
            .arg(script)
            .arg("--cc")
            .arg(cc)
            .arg("--unit")
            .args([&producer.0, &producer.1])
            .arg("--unit")
            .args([&consumer.0, &consumer.1])
            .args(["--entry", "consumer.ku", &node.to_string(), "--caller"])
            .arg(caller)
            .args([
                "--cflag=-Dmalloc=test_malloc",
                "--cflag=-Drealloc=test_realloc",
                "--cflag=-Dfree=test_free",
            ])
            .arg(format!("--ldflag={}", allocator_source.display()))
            .arg("-o")
            .arg(&binary),
        &cwd,
        150,
    );
    assert_ok(&linked);
    let source = cwd.join("main.ku");
    fs::write(
        &source,
        r#"import { Inspect } from "./consumer.ku"
fn main(): null! {
    Inspect(1)?
    println("ok")
    try { Inspect(-1)? } catch (err) { println(err.domain + ":" + err.code + ":" + err.message) }
    return ok(null)
}
"#,
    )
    .unwrap();
    let oracle = cwd.join(format!("oracle{suffix}"));
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release", "--offline"])
            .arg(&source)
            .arg("-o")
            .arg(&oracle),
        &cwd,
        100,
    ));
    for name in [
        "main.ku",
        "producer.ku",
        "consumer.ku",
        "types.ku",
        "modules.txt",
    ] {
        fs::remove_file(cwd.join(name)).unwrap();
    }
    let expected = execute(&mut Command::new(oracle), &cwd, 15);
    let actual = execute(&mut Command::new(binary), &cwd, 15);
    assert_ok(&expected);
    assert_ok(&actual);
    assert_eq!(actual.stdout, expected.stdout);
    assert!(actual.stderr.is_empty());
    assert_eq!(
        String::from_utf8(actual.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        "ok\nread:negative:denied🙂\n"
    );

    fs::write(
        cwd.join("reader.ku"),
        r#"fn Read(&paths: [str], &prefix: str): null! {
    copied = paths[0].clone()
    local = prefix.clone()
    return ok(null)
}
fn MoveAfter(&loan: str, owner: str): null! { return ok(null) }
"#,
    )
    .unwrap();
    fs::write(
        cwd.join("view.ku"),
        r#"import { Read as Load, MoveAfter } from "./reader.ku"
fn Inspect(&paths: [str], prefix: str): null! {
    Load(paths, prefix)?
    Load(paths, prefix)?
    full = prefix.clone()
    fail { domain: "loan", code: "checked", message: full }
}
fn Invalid(owner: str): null! {
    MoveAfter(owner, owner)?
    return ok(null)
}
"#,
    )
    .unwrap();
    let reader = emit("reader.ku\nview.ku\n", "Read", "linked-null");
    let view = emit("view.ku\nreader.ku\n", "Inspect", "linked-null");
    assert!(view.2.contains("const KuGuardStringArray*"));
    assert!(view.2.contains("const KuProjectionString*"));
    let Some(Value::Result { ok: true, value }) =
        json::eval("decode", &[Value::String(view.2)], Span::default()).unwrap()
    else {
        panic!("expected checked View metadata")
    };
    let Value::Object(plan) = *value else {
        panic!("expected View plan")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected View export")
    };
    let Value::Int(node) = export["node"] else {
        panic!("expected View node")
    };
    let Value::String(symbol) = &export["symbol"] else {
        panic!("expected View symbol")
    };
    let host = cwd.join("loan.c");
    fs::write(
        &host,
        r#"#include <assert.h>
int main(int argc, char **argv) {
    (void)argv;
    uint8_t original[] = "name";
    KuProjectionString item = { original, 4, 0, KU_PROJECTION_STRING_STATIC };
    KuGuardStringArray array = {1, &item, 1};
    if (argc > 1) array = (KuGuardStringArray){0};
    uint8_t *prefix = malloc(6);
    assert(prefix);
    memcpy(prefix, "prefix", 6);
    KuBootstrapResultNull result = __ENTRY__(&array, (KuProjectionString){ prefix, 6, 6, KU_PROJECTION_STRING_OWNED });
    assert(!result.ok && result.error.message.len == 6 && memcmp(result.error.message.ptr, "prefix", 6) == 0);
    assert(array.data == &item && item.ptr == original && memcmp(item.ptr, "name", 4) == 0);
    ku_projection_string_drop(&result.error.message);
    return 0;
}
"#
        .replace("__ENTRY__", symbol),
    )
    .unwrap();
    let executable = cwd.join(format!("array-view{suffix}"));
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap/tools/link_c.py");
    let cc = std::env::var("CC").unwrap_or_else(|_| {
        if cfg!(windows) {
            "clang".into()
        } else {
            "cc".into()
        }
    });
    let link = execute(
        Command::new(python)
            .arg("-B")
            .arg(script)
            .arg("--cc")
            .arg(cc)
            .arg("--unit")
            .args([reader.0, reader.1])
            .arg("--unit")
            .args([view.0, view.1])
            .args(["--entry", "view.ku", &node.to_string(), "--caller"])
            .arg(host)
            .arg("-o")
            .arg(&executable),
        &cwd,
        150,
    );
    assert_ok(&link);

    fs::write(cwd.join("function.txt"), "Invalid").unwrap();
    fs::write(cwd.join("mode.txt"), "linked-null").unwrap();
    let mixed_modes = execute(&mut Command::new(&compiler), &cwd, 30);
    assert_eq!(mixed_modes.status.code(), Some(1));
    assert!(mixed_modes.stdout.is_empty());
    assert!(String::from_utf8_lossy(&mixed_modes.stderr).contains("linked_view_move"));
    for name in ["reader.ku", "view.ku", "modules.txt"] {
        fs::remove_file(cwd.join(name)).unwrap();
    }
    assert_ok(&execute(&mut Command::new(&executable), &cwd, 15));
    let bounds = execute(Command::new(executable).arg("empty"), &cwd, 15);
    assert_eq!(bounds.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bounds.stderr).contains("index 0 out of bounds for length 0"));
    linked_null::assert_nested_array_result_cleanup(&compiler, &cwd);
    linked_null::assert_checked_string_array_borrow(&compiler, &cwd);
    linked_null::assert_short_circuit_reads(&compiler, &cwd);
    linked_null::assert_linked_fs_read(&compiler, &cwd);
    linked_guard::assert_terminal_branch_ownership(&compiler, &cwd);
    linked_guard::assert_checked_output_ownership(&compiler, &cwd);
    linked_guard::assert_terminal_try_unwind(&compiler, &cwd);
    linked_guard::assert_ignored_result_cleanup(&compiler, &cwd);
    linked_guard::assert_checked_string_rebind(&compiler, &cwd);
    linked_guard::assert_checked_str_predicate(&compiler, &cwd);
    struct_view::assert_named_struct_view(&compiler, &cwd);
    member_move::assert_owned_member_transfer(&compiler, &cwd);
    linked_guard::assert_linked_call_addition(&compiler, &cwd);
    linked_guard::assert_decorated_caught_error(&compiler, &cwd);
    struct_view::assert_checked_int_view(&compiler, &cwd);
    struct_view::assert_checked_field_clone(&compiler, &cwd);
    struct_view::assert_owned_int_locals(&compiler, &cwd);
    struct_view::assert_counted_branch_owners(&compiler, &cwd);
    struct_view::assert_lazy_readonly_bool_call(&compiler, &cwd);
}
