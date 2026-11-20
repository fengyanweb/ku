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
    layouts = BuildStructLayouts(named, bindings)?
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
    // Use the declared parameter rather than the last lexical occurrence.
    let signature = span.find("token: Token").expect("real Token parameter");
    let span_of_token = format!(
        "{}..{}",
        coordinates(&span, signature),
        coordinates(&span, signature + "token".len())
    );
    diagnostics.push((span.as_str(), "SpanFromToken", format!(
        "error|bootstrap.semantics.structs|unsupported_aggregate_layout|<source>|projection returns require local structs containing Copy scalar fields|{span_of_token}"
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
