use super::{assert_ok, compile_projection, execute, CHECKED_ARRAY_ALLOCATOR};
use ku::{span::Span, stdlib::json, value::Value};
use std::{fs, path::Path, process::Command};

const MANIFEST: &str = "stage2/parser.ku\nstage1/token.ku\nstage1/lexer.ku\nstage2/span.ku\nstage2/ast.ku\nstage2/context.ku\nstage2/expression_support.ku\nstage2/diagnostic.ku\n";

const DRIVER: &str = r#"
static __TOKEN__ tokens[4097];
static unsigned observed_calls;
static int64_t observed_line;
static bool observed_domain;

static KuProjectionString literal(const char *text) {
    return (KuProjectionString){(uint8_t*)text, strlen(text), 0, KU_PROJECTION_STRING_STATIC};
}
static KuProjectionString owned(const char *text) {
    size_t len = strlen(text);
    uint8_t *data = malloc(len);
    memcpy(data,text,len);
    return (KuProjectionString){data,len,len,KU_PROJECTION_STRING_OWNED};
}
static KuBootstrapResultNull failure(KuProjectionString code, KuProjectionString message, bool stage3) {
    assert(code.len == sizeof("invalid_token_stream") - 1);
    ++observed_calls;
    observed_domain = stage3;
    return (KuBootstrapResultNull){.error={.code=code,.message=message}};
}
KuBootstrapResultNull __POINT__(KuProjectionString code, KuProjectionString message, bool stage3) {
    observed_line = 1;
    return failure(code,message,stage3);
}
KuBootstrapResultNull __AT__(__TOKEN__ token, KuProjectionString code, KuProjectionString message, bool stage3) {
    observed_line = token.f3;
    ku_projection_string_drop(&token.f0);
    ku_projection_string_drop(&token.f1);
    return failure(code,message,stage3);
}
static void allocate_checked(void) {
    tokens[0].f0 = owned("Eof");
    tokens[0].f1 = owned("payload");
}
static void check_partial_clone(void) {
    if (observed_calls || outstanding() != 2) _Exit(85);
    assert(tokens[0].f0.len == 3 && memcmp(tokens[0].f0.ptr,"Eof",3) == 0);
    assert(tokens[0].f1.len == 7 && memcmp(tokens[0].f1.ptr,"payload",7) == 0);
    ku_projection_string_drop(&tokens[0].f0);
    ku_projection_string_drop(&tokens[0].f1);
    if (outstanding()) _Exit(86);
}
static void probe(size_t len, int64_t early, bool terminal, bool stage3, int64_t line, const char *message) {
    observed_calls = 0;
    observed_line = 0;
    observed_domain = !stage3;
    __ARRAY__ array = {len, len ? tokens : NULL, len};
    for (size_t i=0; i<len; i++) {
        bool eof = (int64_t)i == early || (terminal && i == len-1);
        tokens[i] = (__TOKEN__){.f0=literal(eof ? "Eof" : "Int"), .f1=literal("text"), .f3=(int64_t)i+8};
    }
    int before = attempts;
    const uint8_t *first = len ? tokens[0].f0.ptr : NULL;
    KuBootstrapResultNull result = __ENTRY__(&array,stage3);
    assert(attempts == before && array.len == len && array.capacity == len && array.data == (len ? tokens : NULL));
    if (len) assert(tokens[0].f0.ptr == first);
    if (message) {
        assert(!result.ok && observed_calls == 1 && observed_line == line && observed_domain == stage3);
        assert(result.error.message.len == strlen(message));
        assert(memcmp(result.error.message.ptr,message,result.error.message.len) == 0);
    } else {
        assert(result.ok && observed_calls == 0);
    }
    ku_projection_string_drop(&result.error.message);
    ku_projection_string_drop(&result.error.code);
}
int main(int argc, char **argv) {
    (void)test_realloc;
    (void)argv;
    if (argc == 2) {
        allocate_checked();
        __ARRAY__ array = {2,tokens,2};
        assert(atexit(check_partial_clone) == 0);
        fail_after = 1;
        (void)__ENTRY__(&array,false);
        return 87;
    }
    probe(0,-1,false,false,1,"token stream must end in exactly one EOF");
    probe(1,-1,true,true,0,NULL);
    probe(4097,0,false,true,8,"stage-2 parser accepts at most 4096 tokens");
    probe(4,1,true,true,9,"EOF must be the final token");
    probe(4,-1,false,false,11,"token stream is missing EOF");
    probe(4096,-1,true,false,0,NULL);
    allocate_checked();
    __ARRAY__ singleton = {1,tokens,1};
    int before = attempts;
    const uint8_t *kind = tokens[0].f0.ptr, *lexeme = tokens[0].f1.ptr;
    KuBootstrapResultNull accepted = __ENTRY__(&singleton,false);
    assert(accepted.ok && attempts == before && outstanding() == 2);
    assert(tokens[0].f0.ptr == kind && tokens[0].f1.ptr == lexeme);
    ku_projection_string_drop(&tokens[0].f0);
    ku_projection_string_drop(&tokens[0].f1);
    assert(!outstanding());
    return 0;
}
"#;

pub(super) fn assert_readonly_preflight(seed: &Path, cwd: &Path) {
    let bootstrap = Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap");
    for module in MANIFEST.lines() {
        let file = cwd.join(module);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::copy(bootstrap.join(module), file).unwrap();
    }
    fs::write(cwd.join("modules.txt"), MANIFEST).unwrap();
    fs::write(cwd.join("function.txt"), "ValidateTokenWindow").unwrap();
    let emit = |mode: &str| {
        fs::write(cwd.join("mode.txt"), mode).unwrap();
        execute(&mut Command::new(seed), cwd, 30)
    };
    let metadata = emit("linked-null-plan");
    assert_ok(&metadata);
    let Some(Value::Result { ok: true, value }) = json::eval(
        "decode",
        &[Value::String(String::from_utf8(metadata.stdout).unwrap())],
        Span::default(),
    )
    .unwrap() else {
        panic!("expected JSON link plan")
    };
    let Value::Object(plan) = *value else {
        panic!("expected link plan object")
    };
    let Value::Object(export) = &plan["export"] else {
        panic!("expected export")
    };
    let Value::Object(signature) = &export["signature"] else {
        panic!("expected export signature")
    };
    let Value::Array(params) = &signature["parameters"] else {
        panic!("expected view and bool")
    };
    let Value::Object(view) = &params[0] else {
        panic!("expected readonly token input")
    };
    assert_eq!(view["kind"], Value::String("StructArray".into()));
    assert_eq!(view["passing"], Value::String("view".into()));
    assert_eq!(view["source_path"], Value::String("stage1/token.ku".into()));
    let Value::String(view_type) = &view["c_type"] else {
        panic!("expected C array carrier")
    };
    let array = view_type
        .strip_prefix("const ")
        .unwrap()
        .strip_suffix('*')
        .unwrap();
    let token = array.strip_prefix("KuArray_").unwrap();
    let Value::Array(dependencies) = &plan["dependencies"] else {
        panic!("expected dependencies")
    };
    let [Value::Object(point), Value::Object(at)] = dependencies.as_slice() else {
        panic!("expected RejectAtPoint and RejectAt")
    };
    assert_eq!(point["source_name"], Value::String("RejectAtPoint".into()));
    assert_eq!(at["source_name"], Value::String("RejectAt".into()));
    let Value::String(point) = &point["symbol"] else {
        panic!("expected point symbol")
    };
    let Value::String(at) = &at["symbol"] else {
        panic!("expected span symbol")
    };
    let Value::String(entry) = &export["symbol"] else {
        panic!("expected entry symbol")
    };
    let output = emit("linked-null");
    assert_ok(&output);
    let program = String::from_utf8(output.stdout).unwrap();
    let driver = DRIVER
        .replace("__ENTRY__", entry)
        .replace("__ARRAY__", array)
        .replace("__TOKEN__", token)
        .replace("__POINT__", point)
        .replace("__AT__", at);
    let c = cwd.join("token-window.c");
    let exe = cwd.join(format!("token-window{}", std::env::consts::EXE_SUFFIX));
    fs::write(&c, format!("{CHECKED_ARRAY_ALLOCATOR}{program}{driver}")).unwrap();
    compile_projection(&c, &exe, cwd);
    assert_ok(&execute(&mut Command::new(&exe), cwd, 10));
    let oom = execute(Command::new(&exe).arg("clone-oom"), cwd, 10);
    assert_eq!(oom.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(oom.stderr).unwrap().trim(),
        "out of memory"
    );

    fs::write(cwd.join("modules.txt"), "bad-window.ku").unwrap();
    fs::write(cwd.join("function.txt"), "Start").unwrap();
    for (source, diagnostic) in [
        ("struct Token { kind: str }\nfn Start(&tokens: [Token]): null! { local = tokens[0] return ok(null) }",
         "linked_view_escape"),
        ("struct Token { kind: str }\nfn Start(&tokens: [Token]): null! { i = 0 while (i < tokens.len()) { i = i + 2 } return ok(null) }",
         "unsupported_linked_loop"),
        ("fn Target(msg: str): null! { return ok(null) }\nfn Start(msg: str, flag: bool): int! { if (flag) { Target(msg)? } return ok(msg.len()) }",
         "unavailable_linked_binding"),
    ] {
        fs::write(cwd.join("bad-window.ku"), source).unwrap();
        let failure = emit("linked-null");
        assert!(!failure.status.success() && failure.stdout.is_empty());
        let error = String::from_utf8(failure.stderr).unwrap();
        assert!(error.contains(diagnostic), "{diagnostic}, got {error}");
    }
}
