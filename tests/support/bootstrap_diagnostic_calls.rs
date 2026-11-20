use super::{
    assert_ok, compile_projection_product, execute, stage_sources, Temp, CHECKED_ARRAY_ALLOCATOR,
};
use std::{fs, path::PathBuf, process::Command};

const EMITTER: &str = r#"import "std.fs"
import { ReadProjectModules } from "./project_manifest.ku"
import { EmitProjectStructStringC } from "./project_struct_string_c.ku"
fn main(): null! {
    supplied = ReadProjectModules(["stage2/diagnostic.ku", "stage1/token.ku", "stage2/span.ku"])?
    generated = EmitProjectStructStringC(supplied, fs.read("target.txt")?)?
    println(generated.symbol)
    for dependency in generated.dependencies {
        println(dependency.source_name + "|" + dependency.symbol + "|" + dependency.parameter)
    }
    println("--")
    print(generated.source)
    return ok(null)
}
"#;

const ORACLE: &str = r#"import { Span } from "./stage2/span.ku"
import { MakeDiagnostic, DiagnosticCanonical } from "./stage2/diagnostic.ku"
fn main(): null! {
    minimum = -9223372036854775807 - 1
    span = Span { line: 4, column: 9, offset: minimum, end_line: 6, end_column: 13, end_offset: 9223372036854775807 }
    diagnostic = MakeDiagnostic("error|错误🙂", "bootstrap\\parser", "bad\tinput", "two\r\nlines", "<stage|source>", span)
    println(DiagnosticCanonical(diagnostic))
    return ok(null)
}
"#;

const BRIDGE: &str = r#"
static unsigned escape_calls, span_calls;
static KuProjectionString from_stage0(KuString s) {
    return (KuProjectionString){s.ptr, s.len, s.capacity, s.storage};
}
KuProjectionString __ESCAPE__(KuProjectionString value) {
    ++escape_calls;
    return from_stage0(__ESCAPE0__((KuString){value.ptr, value.len, value.capacity, value.storage}));
}
KuProjectionString __SPAN_TEXT__(__SPAN__ value) {
    ++span_calls;
    __SPAN0__ span = {
        .line = value.f0, .column = value.f1, .offset = value.f2,
        .end_line = value.f3, .end_column = value.f4, .end_offset = value.f5,
    };
    return from_stage0(__SPAN_TEXT0__(span));
}
static KuProjectionString owned(const char *text) {
    size_t length = strlen(text);
    uint8_t *buffer = malloc(length ? length : 1);
    if (length) memcpy(buffer, text, length);
    return (KuProjectionString){buffer, length, length ? length : 1, KU_PROJECTION_STRING_OWNED};
}
static void verify_cleanup(void) {
    if (outstanding()) _Exit(97);
    if (fail_join_after == 0) fprintf(stderr, "calls:%u,%u\n", escape_calls, span_calls);
}
int main(int argc, char **argv) {
    assert(atexit(verify_cleanup) == 0);
    __DIAGNOSTIC__ input = {
        owned("error|\351\224\231\350\257\257\360\237\231\202"), owned("bootstrap\\parser"),
        owned("bad\tinput"), owned("two\r\nlines"), owned("<stage|source>"),
        {4, 9, INT64_MIN, 6, 13, INT64_MAX},
    };
    if (argc == 2) fail_join_after = atoi(argv[1]);
    KuProjectionString result = __ENTRY__(input);
    assert(escape_calls == 5 && span_calls == 1 && join_attempts == 10);
    assert(outstanding() == 1 && result.storage == KU_PROJECTION_STRING_OWNED);
    assert(fwrite(result.ptr, 1, result.len, stdout) == result.len);
    putchar('\n');
    ku_projection_string_drop(&result);
    ku_projection_string_drop(&result);
    return 0;
}
"#;

const JOIN_FAILURES: &str = r#"
static int fail_join_after = -1;
static int join_attempts;
static void *join_malloc(size_t size) {
    ++join_attempts;
    if (fail_join_after == 0) return NULL;
    if (fail_join_after > 0) --fail_join_after;
    return test_malloc(size);
}
#undef malloc
#define malloc join_malloc
"#;

fn stage_function<'a>(source: &'a str, name: &str) -> (&'a str, &'a str) {
    let marker = format!("_{name}(");
    let line = source
        .lines()
        .find(|line| {
            line.starts_with("KuString ") && line.contains(&marker) && line.ends_with(" {")
        })
        .unwrap();
    line.strip_prefix("KuString ")
        .unwrap()
        .split_once('(')
        .unwrap()
}

#[test]
fn bootstrap_compiles_original_diagnostic_canonical_with_typed_imports() {
    let work = Temp::new();
    let compiler = work.0.join("compiler");
    let project = work.0.join("project");
    fs::create_dir(&compiler).unwrap();
    fs::create_dir(&project).unwrap();
    stage_sources(&compiler);
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let file = format!("emit-diagnostic{}", std::env::consts::EXE_SUFFIX);
    let entry = compiler.join("semantics/emit_diagnostic.ku");
    let built = compiler.join("semantics").join(&file);
    let emitter = project.join(file);
    fs::write(&entry, EMITTER).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(entry)
            .arg("-o")
            .arg(&built),
        &project,
        150,
    ));
    fs::rename(built, &emitter).unwrap();
    for stage in ["stage1", "stage2"] {
        fs::rename(compiler.join(stage), project.join(stage)).unwrap();
    }
    fs::remove_dir_all(compiler).unwrap();

    let oracle = project.join("oracle.ku");
    let reference = project.join(format!("oracle{}", std::env::consts::EXE_SUFFIX));
    fs::write(&oracle, ORACLE).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&oracle)
            .arg("-o")
            .arg(&reference),
        &project,
        100,
    ));
    let expected = execute(&mut Command::new(reference), &project, 15);
    assert_ok(&expected);
    let expected = String::from_utf8(expected.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(expected, "error\\p错误🙂|bootstrap\\\\parser|bad\\tinput|<stage\\psource>|two\\r\\nlines|4:9@-9223372036854775808..6:13@9223372036854775807\n");
    assert_ok(&execute(
        Command::new(&ku).args(["build", "--native"]).arg(&oracle),
        &project,
        30,
    ));
    let stage0 = fs::read_to_string(project.join("oracle.c")).unwrap();
    let (escape0, _) = stage_function(&stage0, "EscapeCanonical");
    let (span_text0, arguments) = stage_function(&stage0, "SpanCanonical");
    let (span0, _) = arguments.split_once(" span) {").unwrap();

    let select = project.join("target.txt");
    fs::write(&select, "DiagnosticCanonical").unwrap();
    let result = execute(&mut Command::new(&emitter), &project, 30);
    assert_ok(&result);
    let result = String::from_utf8(result.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    let (manifest, code) = result.split_once("--\n").unwrap();
    let lines: Vec<_> = manifest.lines().collect();
    let [entry, escape, span] = lines.as_slice() else {
        panic!("two typed dependencies")
    };
    let ["EscapeCanonical", escape_link, "KuProjectionString"] =
        escape.split('|').collect::<Vec<_>>().as_slice()[..]
    else {
        panic!("owned str dependency")
    };
    let ["SpanCanonical", span_text_link, span_layout] =
        span.split('|').collect::<Vec<_>>().as_slice()[..]
    else {
        panic!("Copy Span dependency")
    };
    let signature = code
        .lines()
        .find(|line| line.contains(&format!(" {entry}(")))
        .unwrap();
    let (_, arguments) = signature.split_once('(').unwrap();
    let (layout, _) = arguments.split_once(" a0) {").unwrap();
    assert!(code.contains(&format!(
        "extern KuProjectionString {escape_link}(KuProjectionString);"
    )));
    assert!(code.contains(&format!(
        "extern KuProjectionString {span_text_link}({span_layout});"
    )));

    let provider = BRIDGE
        .replace("__ESCAPE__", escape_link)
        .replace("__ESCAPE0__", escape0)
        .replace("__SPAN_TEXT__", span_text_link)
        .replace("__SPAN_TEXT0__", span_text0)
        .replace("__SPAN__", span_layout)
        .replace("__SPAN0__", span0)
        .replace("__DIAGNOSTIC__", layout)
        .replace("__ENTRY__", entry);
    let linked = project.join("diagnostic_linked.c");
    fs::write(&linked, format!("{CHECKED_ARRAY_ALLOCATOR}\n#define main origin_main\n{stage0}\n#undef main\n{JOIN_FAILURES}{code}\n#undef malloc\n#define malloc test_malloc\n{provider}")).unwrap();
    let strict = project.join("diagnostic_strict.c");
    fs::write(&strict, code).unwrap();

    let subject = project.join("stage2/diagnostic.ku");
    for (source, error) in [
        (
            r#"import { EscapeCanonical } from "../stage1/token.ku"
struct E { text: str }
fn Probe(e: E): str { return EscapeCanonical(e.text) + EscapeCanonical(e.text) }
"#,
            "reused_struct_string_owner",
        ),
        (
            r#"struct E { text: str }
fn Local(input: str): str { return input }
fn Probe(e: E): str { return e.text + Local(e.text) }
"#,
            "reused_struct_string_owner",
        ),
        (
            r#"struct E { text: str }
fn Probe(e: E): str { return str(e.text) }
"#,
            "struct_string_int_argument",
        ),
        (
            r#"import { SpanCanonical } from "./span.ku"
struct Span { line: int }
struct E { span: Span }
fn Probe(e: E): str { return SpanCanonical(e.span) }
"#,
            "struct_string_link_argument",
        ),
        (
            r#"import { EscapeCanonical } from "../stage1/token.ku"
struct E { text: str }
fn Probe(EscapeCanonical: E): str { return EscapeCanonical(EscapeCanonical.text) }
"#,
            "shadowed_struct_string_call",
        ),
    ] {
        fs::write(&subject, source).unwrap();
        fs::write(&select, "Probe").unwrap();
        let rejected = execute(&mut Command::new(&emitter), &project, 15);
        assert!(!rejected.status.success() && rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(error));
    }
    for stage in ["stage1", "stage2"] {
        fs::remove_dir_all(project.join(stage)).unwrap();
    }
    fs::remove_file(oracle).unwrap();
    fs::remove_file(project.join("oracle.c")).unwrap();
    compile_projection_product(&strict, &project.join("diagnostic.o"), &project, true, true);
    let app = project.join(format!("diagnostic-linked{}", std::env::consts::EXE_SUFFIX));
    compile_projection_product(&linked, &app, &project, false, false);
    let output = execute(&mut Command::new(&app), &project, 15);
    assert_ok(&output);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        expected
    );
    for (index, (escape_calls, span_calls)) in [
        (1, 0),
        (2, 0),
        (2, 0),
        (3, 0),
        (3, 0),
        (4, 0),
        (4, 0),
        (5, 0),
        (5, 0),
        (5, 1),
    ]
    .into_iter()
    .enumerate()
    {
        let failed = execute(Command::new(&app).arg(index.to_string()), &project, 5);
        assert_eq!(
            failed.status.code(),
            Some(1),
            "join {index}: {}",
            String::from_utf8_lossy(&failed.stderr)
        );
        assert!(failed.stdout.is_empty());
        assert_eq!(
            String::from_utf8(failed.stderr)
                .unwrap()
                .replace("\r\n", "\n"),
            format!("out of memory\ncalls:{escape_calls},{span_calls}\n")
        );
    }
}
