use super::{
    assert_ok, compile_projection_product, execute, stage_sources, Temp, CHECKED_ARRAY_ALLOCATOR,
    PRIMARY_EMITTER,
};
use std::{fs, path::PathBuf, process::Command};

const ORACLE: &str = r#"import { Token } from "./stage1/token.ku"
import { ReadBinaryOperator } from "./stage2/expression_support.ku"
fn observe(kind: str, stage3_domain: bool): null {
    token = Token { kind: kind.clone(), lexeme: "原始🙂", int_value: 42, line: 4, column: 9, offset: 51,
                     end_line: 4, end_column: 12, end_offset: 59 }
    try {
        value = ReadBinaryOperator(token, stage3_domain)?
        println("OK|" + kind + "|" + str(value.precedence) + "|" + value.text)
    } catch(err) {
        println("ERR|" + err.domain + "|" + err.code + "|" + err.message)
    }
    return null
}
fn main(): null! {
    for kind in ["OrOr", "AndAnd", "BangEqual", "EqualEqual", "Less", "LessEqual", "Greater", "GreaterEqual", "Minus", "Plus", "Slash", "Star", "Percent", "Ident"] {
        observe(kind, false)
    }
    observe("Ident", true)
    return ok(null)
}
"#;

const BRIDGE: &str = r#"
static __TOKEN__ borrowed;
static uint8_t *caller_kind, *caller_lexeme;
static const char *caller_label;
static unsigned calls;
static bool expect_fatal;

static KuString to_stage0(KuProjectionString v) {
    return (KuString){v.ptr,v.len,v.capacity,v.storage};
}
static KuProjectionString from_stage0(KuString v) {
    return (KuProjectionString){v.ptr,v.len,v.capacity,v.storage};
}
static void verify_borrow(void) {
    static const uint8_t original[] = {0xe5,0x8e,0x9f,0xe5,0xa7,0x8b,0xf0,0x9f,0x99,0x82};
    assert(borrowed.f0.ptr == caller_kind && borrowed.f1.ptr == caller_lexeme);
    assert(borrowed.f0.len == strlen(caller_label) && memcmp(caller_kind, caller_label, borrowed.f0.len) == 0);
    assert(borrowed.f1.len == sizeof(original) && memcmp(caller_lexeme, original, sizeof(original)) == 0);
    assert(borrowed.f0.storage == KU_PROJECTION_STRING_OWNED && borrowed.f1.storage == KU_PROJECTION_STRING_OWNED);
    assert(borrowed.f2 == 42 && borrowed.f3 == 4 && borrowed.f4 == 9 && borrowed.f5 == 51);
    assert(borrowed.f6 == 4 && borrowed.f7 == 12 && borrowed.f8 == 59);
}
static void drop_borrow(void) {
    ku_projection_string_drop(&borrowed.f0);
    ku_projection_string_drop(&borrowed.f1);
}
static void check_clean(void) {
    if (expect_fatal) {
        if (calls || outstanding() != 2) _Exit(97);
        verify_borrow();
        drop_borrow();
    }
    if (outstanding()) _Exit(97);
}
// Only the dependency uses the original Stage0-generated implementation.
KuBootstrapResultNull __DEPENDENCY__(__TOKEN__ input, KuProjectionString code, KuProjectionString message, bool stage3_domain) {
    assert(input.f0.ptr != caller_kind && input.f1.ptr != caller_lexeme);
    assert(input.f0.len == borrowed.f0.len && memcmp(input.f0.ptr, caller_kind, input.f0.len) == 0);
    assert(input.f1.len == borrowed.f1.len && memcmp(input.f1.ptr, caller_lexeme, input.f1.len) == 0);
    assert(outstanding() == 4);
    ++calls;
    __STAGE0_TOKEN__ token = {
        .kind = to_stage0(input.f0), .lexeme = to_stage0(input.f1), .int_value = input.f2,
        .line = input.f3, .column = input.f4, .offset = input.f5,
        .end_line = input.f6, .end_column = input.f7, .end_offset = input.f8
    };
    KuResult_null result = __REJECT__(token, to_stage0(code), to_stage0(message), stage3_domain);
    return (KuBootstrapResultNull){result.ok, result.value,
        {from_stage0(result.error.domain), from_stage0(result.error.code), from_stage0(result.error.message)}};
}
static KuProjectionString owned(const uint8_t *text, size_t len) {
    uint8_t *bytes = malloc(len);
    memcpy(bytes, text, len);
    return (KuProjectionString){ bytes, len, len, KU_PROJECTION_STRING_OWNED };
}
static void set_token(const char *kind) {
    static const uint8_t original[] = {0xe5,0x8e,0x9f,0xe5,0xa7,0x8b,0xf0,0x9f,0x99,0x82};
    borrowed = (__TOKEN__){ owned((const uint8_t*)kind, strlen(kind)), owned(original, sizeof(original)),
        42, 4, 9, 51, 4, 12, 59 };
    caller_kind = borrowed.f0.ptr;
    caller_lexeme = borrowed.f1.ptr;
    caller_label = kind;
}
static void put_carrier(KuProjectionString value) {
    if (value.len) printf("%.*s", (int)value.len, (char*)value.ptr);
}
int main(int argc, char **argv) {
    assert(atexit(check_clean) == 0);
    if (argc == 2) {
        set_token("Ident");
        expect_fatal = true;
        if (strcmp(argv[1], "overflow") != 0) fail_after = atoi(argv[1]);
        (void)__ENTRY__(&borrowed, true);
        return 98;
    }
    const char *kinds[] = {"OrOr","AndAnd","BangEqual","EqualEqual","Less","LessEqual","Greater","GreaterEqual","Minus","Plus","Slash","Star","Percent","Ident","Ident"};
    for (unsigned i = 0; i < sizeof(kinds)/sizeof(kinds[0]); ++i) {
        set_token(kinds[i]);
        int before = attempts;
        __RESULT__ result = __ENTRY__(&borrowed, i == 14);
        verify_borrow();
        if (i < 13) {
            assert(result.ok && !calls && attempts == before && outstanding() == 2);
            printf("OK|%s|%lld|", kinds[i], (long long)result.value.f0);
            put_carrier(result.value.f1); putchar('\n');
        } else {
            assert(!result.ok && calls == i - 12 && outstanding() == 3);
            assert(!result.value.f1.ptr && result.error.message.storage == KU_PROJECTION_STRING_OWNED);
            fputs("ERR|", stdout); put_carrier(result.error.domain);
            putchar('|'); put_carrier(result.error.code); putchar('|'); put_carrier(result.error.message); putchar('\n');
        }
        __ENTRY___drop(&result);
        verify_borrow();
        drop_borrow();
        assert(!outstanding());
    }
    return 0;
}
"#;

fn linked_binary_operator(
    code: &str,
    stage0: &str,
    reject: &str,
    stage0_token: &str,
) -> (String, String) {
    let (symbols, code) = code.split_once('\n').unwrap();
    let (entry, dependency) = symbols.trim().split_once('|').unwrap();
    let signature = code
        .lines()
        .find(|line| line.contains(&format!(" {entry}(")))
        .unwrap();
    let (result, signature) = signature.split_once(' ').unwrap();
    let (_, arguments) = signature.split_once("(const ").unwrap();
    let (token, _) = arguments.split_once("* a0,").unwrap();
    assert!(code.contains("_clone_element(a0, &"));
    let driver = BRIDGE
        .replace("__RESULT__", result)
        .replace("__STAGE0_TOKEN__", stage0_token)
        .replace("__TOKEN__", token)
        .replace("__DEPENDENCY__", dependency)
        .replace("__REJECT__", reject)
        .replace("__ENTRY__", entry);
    let linked = format!("{CHECKED_ARRAY_ALLOCATOR}\n#define main origin_main\n{stage0}\n#undef main\n{code}{driver}");
    (code.to_owned(), linked)
}

#[test]
fn bootstrap_compiles_real_binary_operator_and_clones_borrow_only_for_error() {
    let work = Temp::new();
    let compiler = work.0.join("compiler");
    let project = work.0.join("project");
    fs::create_dir(&compiler).unwrap();
    fs::create_dir(&project).unwrap();
    stage_sources(&compiler);
    let ku = std::env::var_os("KU_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ku")));
    let name = format!("emit-binary{}", std::env::consts::EXE_SUFFIX);
    let script = compiler.join("semantics/emit_binary.ku");
    let built = compiler.join("semantics").join(&name);
    let emitter = project.join(name);
    fs::write(
        &script,
        PRIMARY_EMITTER.replace("\"ReadPrimaryAtom\"", "\"ReadBinaryOperator\""),
    )
    .unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(script)
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

    let source = project.join("oracle.ku");
    let oracle = project.join(format!("oracle{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, ORACLE).unwrap();
    assert_ok(&execute(
        Command::new(&ku)
            .args(["build", "--release"])
            .arg(&source)
            .arg("-o")
            .arg(&oracle),
        &project,
        90,
    ));
    let result = execute(&mut Command::new(oracle), &project, 20);
    assert_ok(&result);
    let expected = String::from_utf8(result.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(
        expected
            .lines()
            .filter(|line| line.starts_with("OK|"))
            .count(),
        13
    );
    assert!(expected.contains("OK|BangEqual|3|!=\n") && expected.contains("OK|Percent|6|%\n"));
    assert!(
        expected.contains("ERR|bootstrap.parser|trailing_token|")
            && expected.contains("ERR|bootstrap.parser.stage3|trailing_token|")
    );
    assert!(expected.contains("4:9@51..4:12@59"));
    assert_ok(&execute(
        Command::new(&ku).args(["build", "--native"]).arg(&source),
        &project,
        30,
    ));
    let stage0 = fs::read_to_string(project.join("oracle.c")).unwrap();
    let link = stage0
        .lines()
        .find(|line| {
            line.starts_with("KuResult_null ")
                && line.contains("_RejectAt(")
                && line.ends_with(" {")
        })
        .unwrap();
    let (reject, signature) = link
        .strip_prefix("KuResult_null ")
        .unwrap()
        .split_once('(')
        .unwrap();
    let (stage0_token, _) = signature.split_once(" token,").unwrap();

    let normal = execute(&mut Command::new(&emitter), &project, 30);
    assert_ok(&normal);
    let (strict, linked) = linked_binary_operator(
        &String::from_utf8(normal.stdout).unwrap(),
        &stage0,
        reject,
        stage0_token,
    );
    let strict_path = project.join("binary_operator.c");
    fs::write(&strict_path, strict).unwrap();
    let linked_path = project.join("binary_operator_linked.c");
    fs::write(&linked_path, linked).unwrap();

    // Only the independent failure fixtures differ from the real source.
    let input = project.join("stage2/expression_support.ku");
    let original = fs::read_to_string(&input).unwrap();
    let checked_call = "RejectAt(token.clone(), \"trailing_token\", \"expected end of expression\", stage3_domain)?";
    assert_eq!(original.matches(checked_call).count(), 1);
    fs::write(
        &input,
        original.replace(
            checked_call,
            "RejectAt(token, \"trailing_token\", \"expected end of expression\", stage3_domain)?",
        ),
    )
    .unwrap();
    let rejected = execute(&mut Command::new(&emitter), &project, 30);
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("struct_result_link_argument"));
    fs::write(&input, original.replace(checked_call, "RejectAt(token.clone(), \"trailing_token\", \"expected end of expression\", (9223372036854775807 + 1) > 0)?")).unwrap();
    let overflow = execute(&mut Command::new(&emitter), &project, 30);
    assert_ok(&overflow);
    let (_, later_failure) = linked_binary_operator(
        &String::from_utf8(overflow.stdout).unwrap(),
        &stage0,
        reject,
        stage0_token,
    );
    let overflow_path = project.join("binary_operator_overflow.c");
    fs::write(&overflow_path, later_failure).unwrap();

    for stage in ["stage1", "stage2"] {
        fs::remove_dir_all(project.join(stage)).unwrap();
    }
    fs::remove_file(source).unwrap();
    fs::remove_file(project.join("oracle.c")).unwrap();
    compile_projection_product(
        &strict_path,
        &project.join("binary_operator.o"),
        &project,
        true,
        true,
    );
    let app = project.join(format!("binary-linked{}", std::env::consts::EXE_SUFFIX));
    compile_projection_product(&linked_path, &app, &project, false, false);
    let output = execute(&mut Command::new(&app), &project, 20);
    assert_ok(&output);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        expected
    );
    for fail_at in ["0", "1"] {
        let trap = execute(Command::new(&app).arg(fail_at), &project, 10);
        assert_eq!(
            trap.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&trap.stderr)
        );
        assert!(String::from_utf8_lossy(&trap.stderr).contains("out of memory"));
    }
    let late_app = project.join(format!("binary-overflow{}", std::env::consts::EXE_SUFFIX));
    compile_projection_product(&overflow_path, &late_app, &project, false, false);
    let trap = execute(Command::new(late_app).arg("overflow"), &project, 10);
    assert_eq!(
        trap.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&trap.stderr)
    );
    assert!(String::from_utf8_lossy(&trap.stderr).contains("integer overflow"));
}
