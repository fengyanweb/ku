use ku::{
    backend::c::generate_c_source,
    checker::Checker,
    ir::{
        lower_program, optimize_program, verify_borrow_contract, IrExpr, IrExprKind, IrInst,
        IrLValue, IrTerminator, IrType,
    },
    lexer::Lexer,
    parser::Parser,
};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[path = "support/bounded_process.rs"]
pub mod bounded_process;
use bounded_process::{run_bounded, OutputLimits};

fn ir(source: &str) -> ku::ir::IrProgram {
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let ir = lower_program(&ast).unwrap();
    let optimized = optimize_program(&ir);
    verify_borrow_contract(&optimized).unwrap();
    optimized
}

const SOURCE: &str = r#"
struct User { name: str, tags: [str], age: int }
enum Message { Text(value: str) }
enum Code { Number(value: int) }
fn Read(&text: str): int { return text.byte_len() }
fn First(&values: [int]): int { return values[0] }
fn FirstBool(&values: [bool]): bool { return values[0] }
fn Age(&user: User): int { return user.age }
fn Combine(left: int, right: int): int { return left * 10 + right }
fn Inspect(&user: User): str {
    println(Read(user.name))
    println(Read(user.tags[0]))
    return "Hello " + user.name
}
fn Copy(&text: str): str { return text.clone() }
fn Make(): User { return User { name: "世" + "界", tags: ["Ku" + "!"], age: 7 } }
fn Apply(&op: fn(&str): str, &text: str): str { return op(text) }
fn CloneResult(&value: str!): str! { return value.clone() }
fn CloneMessage(&value: Message): Message { return value.clone() }
fn MessageText(value: Message): str { return match value { Message.Text(text) => text } }
fn JsonArray(&values: [int]): str! { return json.stringify(values) }
fn JsonEncode(&values: [int]): str! { return json.encode(values) }
fn JsonDecode(&text: str): str! { value = json.decode(text)? return value.as_str() }
fn JsonParse(&text: str): str! { value = json.parse(text)? return value.as_str() }
fn JsonTryParse(&text: str): str! { value = json.try_parse(text)? return value.as_str() }
fn ReadCode(&code: Code): int { return match code { Code.Number(value) => value } }
fn Finish(&text: str): str! {
    try { return ok(text.clone()) } finally { println(text) }
    return ok(text.clone())
}
fn main(): null! {
    user = Make()
    println(Inspect(user))
    println(Inspect(user))
    println(user.name)
    println(Inspect(Make()))
    op: fn(&str): str = Copy
    println(Apply(op, user.name))
    println(op(user.name))
    println(Finish(user.name)?)
    readers = [Read]
    if (readers[0](user.name) != 6 || readers[0](user.name) != 6) { panic("borrowed function array element") }
    result: str! = ok("clo" + "sed")
    if (CloneResult(result)? != "closed" || CloneResult(result)? != "closed") { panic("borrowed Result clone") }
    message = Message.Text("pay" + "load")
    code = Code.Number(7)
    if (ReadCode(code) != 7 || ReadCode(code) != 7) { panic("borrowed Copy enum payload") }
    if (MessageText(CloneMessage(message)) != "payload" || MessageText(CloneMessage(message)) != "payload") { panic("borrowed enum clone") }
    numbers = [1, 2, 3]
    flags = [true, false]
    if (First(numbers) != 1 || First(numbers) != 1 || !FirstBool(flags) || !FirstBool(flags)) { panic("borrowed Copy array projection") }
    if (Age(user) != 7 || Age(user) != 7) { panic("borrowed Copy struct projection") }
    changeAge = fn(): int { user.age = 9 return 1 }
    if (Combine(user.age, changeAge()) != 71 || Age(user) != 9) { panic("Copy field snapshot before later argument effect") }
    if (JsonEncode(numbers)? != JsonArray(numbers)? || JsonEncode(numbers)? != "[1,2,3]" || numbers.len() != 3) { panic("borrowed JSON array") }
    object = { name: "Ku" }
    if (json.encode(object)? != json.stringify(object)? || json.encode(object)? != json.encode(object)?) { panic("borrowed JSON object") }
    json_text = "\"" + "中文😀\\u0000tail\""
    parsed = JsonDecode(json_text)?
    if (JsonParse(json_text)? != parsed || JsonTryParse(json_text)? != parsed) { panic("borrowed JSON parser aliases") }
    json_text = "changed"
    if (parsed.byte_len() != 15 || !parsed.starts_with("中文😀")) { panic("borrowed JSON decoded storage") }
    json_bad = "{\"a\":[\"owned\",]}"
    json_caught = 0
    try { JsonDecode(json_bad)? } catch(error) { if (error.domain == "json" && error.code == "parse_error") { json_caught++ } }
    try { JsonParse(json_bad)? } catch(error) { if (error.domain == "json" && error.code == "parse_error") { json_caught++ } }
    try { JsonTryParse(json_bad)? } catch(error) { if (error.domain == "json" && error.code == "parse_error") { json_caught++ } }
    json_bad = "\"recovered\""
    if (json_caught != 3 || JsonDecode(json_bad)? != "recovered") { panic("borrowed JSON error cleanup") }
    failure: str! = err("expected")
    caught = 0
    finalized = 0
    try { CloneResult(failure)? } catch(error) { caught++ } finally { finalized++ }
    try { CloneResult(failure)? } catch(error) { caught++ } finally { finalized++ }
    if (caught != 2 || finalized != 2) { panic("borrowed error Result lifecycle") }
    return ok(null)
}
"#;

#[test]
fn borrow_native_c_uses_const_parameter_abi_and_preserves_modes() {
    let lowered = ir(SOURCE);
    let dump = lowered.to_string();
    assert!(dump.contains("&text: str"), "{dump}");
    assert!(dump.contains("Read(borrow(borrowed user.name))"), "{dump}");
    let c = generate_c_source(&lowered).unwrap();
    assert!(c.contains("const KuString* text"));
    assert!(c.contains("const KuStruct_User* user"));
    assert!(c.contains("view_str__to_str"));
    let read = c
        .split("int64_t Read(")
        .last()
        .expect("Read definition")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        !read.contains("ku_string_drop"),
        "borrowed parameter must not be dropped: {read}"
    );
    assert!(!read.contains("ku_string_clone"));
    for name in ["JsonDecode", "JsonParse", "JsonTryParse"] {
        let body = c
            .split(&format!("KuResult_str {name}(const KuString* text) {{"))
            .nth(1)
            .unwrap_or_else(|| panic!("missing borrowed parser definition: {name}"))
            .split("\n}\n")
            .next()
            .unwrap();
        assert!(!body.contains("ku_string_clone"), "{name}: {body}");
        assert!(!body.contains("ku_string_drop(text)"), "{name}: {body}");
    }
    assert!(!c.contains("run_source") && !c.contains("const SOURCE"));
}

#[test]
fn borrow_ir_function_field_call_preserves_parameter_modes() {
    let program = ir("struct Reader { read: fn(&str): int } fn Read(&text: str): int { return text.len() } fn Use(&reader: Reader, &text: str): int { return reader.read(text) } fn main() { text = \"Ku\" reader = Reader { read: Read } println(Use(reader, text)) println(reader.read(text)) }");
    let dump = program.to_string();
    assert!(
        dump.contains("borrow(borrowed text)") || dump.contains("borrow(text)"),
        "{dump}"
    );
    let c = generate_c_source(&program).unwrap();
    let signature = c
        .find("struct KuClosure_fn_view_str__to_int { int64_t (*invoke)(void*, const KuString*);")
        .expect("complete typed closure ABI");
    let reader = c
        .find("struct KuStruct_Reader {\n  KuClosure_fn_view_str__to_int read;")
        .expect("closure field stored by value");
    assert!(signature < reader);
    assert!(c.contains("value.read = ku_closure_clone_fn_view_str__to_int(value.read);"));
    // Adding KuValue.take must not reserve the same spelling for ordinary
    // callable struct fields. Keep the old indirect-call path and its modes.
    let take = ir("struct Reader { take: fn(&str): int } fn Read(&text: str): int { return text.len() } fn Use(&reader: Reader, &text: str): int { return reader.take(text) } fn main() { text = \"Ku\" reader = Reader { take: Read } println(Use(reader, text)) println(reader.take(text)) }");
    let dump = take.to_string();
    assert!(
        dump.contains("borrow(borrowed text)") || dump.contains("borrow(text)"),
        "{dump}"
    );
    assert!(generate_c_source(&take).is_ok());

    let unused = ir("struct Unused { pick: fn(): bool } fn main() {}");
    let c = generate_c_source(&unused).unwrap();
    assert!(c.contains("(*invoke)(void*); void* env;"));
    assert!(c.contains("struct KuStruct_Unused {"));

    let checked = ir("struct Checked { read: fn(&str): str! } fn main() {}");
    let c = generate_c_source(&checked).unwrap();
    let result_decl = c.find("typedef struct KuResult_str KuResult_str;").unwrap();
    let field_signature = c
        .find("KuResult_str (*invoke)(void*, const KuString*);")
        .unwrap();
    let containing_record = c.find("struct KuStruct_Checked {").unwrap();
    assert!(result_decl < field_signature && field_signature < containing_record);
    assert!(c.contains("struct KuResult_str { bool ok; KuString value; KuError error; };"));

    let aggregate =
        ir("struct Payload { value: int } struct Reader { read: fn(&Payload): int } fn main() {}");
    let error = generate_c_source(&aggregate).unwrap_err();
    assert!(
        error
            .message
            .contains("struct 'Reader.read' requires a synchronous callable field signature"),
        "{error}"
    );

    let enum_payload = ir("struct Reader { read: fn(&str): int } enum Operation { Invoke(read: fn(&str): int) } fn main() {}");
    let error = generate_c_source(&enum_payload).unwrap_err();
    assert!(
        error
            .message
            .contains("enum 'Operation.Invoke.read' does not support payload type closure"),
        "{error}"
    );
}

#[test]
fn borrow_native_mixed_http_rejects_async_callable_field() {
    let source = r#"
import http from "std.http"
struct Reader { read: async fn(int): int }
fn main(): null! {
    async fn Handler(req: HttpRequest): HttpResponse! { return ok(http.text(req.method)) }
    server = http.server()
    server.get("/", Handler)
    server.listen("127.0.0.1:0")?
    return ok(null)
}
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:http".into(),
            span: Default::default(),
        }),
    );
    Checker::new().check(&ast).unwrap();
    let (program, handlers) = ku::ir::lower_mixed_http_setup(&ast).unwrap();
    assert_eq!(handlers.len(), 1);
    let error = generate_c_source(&program).unwrap_err();
    assert!(
        error
            .message
            .contains("struct 'Reader.read' does not support async callable fields"),
        "{error}"
    );
}

#[test]
fn borrow_ir_verifier_rejects_owned_escape_write_and_signature_erasure() {
    let good = ir("fn Read(&text: str): int { return text.len() } fn main() { value = \"Ku\" println(Read(value)) }");
    for return_escape in [true, false] {
        let mut bad = good.clone();
        let f = bad.functions.iter_mut().find(|f| f.name == "Read").unwrap();
        let value = IrExpr {
            kind: IrExprKind::BorrowedParam("text".into()),
            ty: IrType::Str,
        };
        if return_escape {
            f.blocks[0].terminator = IrTerminator::Return(Some(value));
        } else {
            f.blocks[0].instructions.push(IrInst::Store {
                target: IrLValue::Local("text".into()),
                value,
            });
        }
        assert!(verify_borrow_contract(&bad).is_err());
        assert!(generate_c_source(&bad).is_err());
    }
    let mut bad = good;
    bad.functions
        .iter_mut()
        .find(|f| f.name == "Read")
        .unwrap()
        .params[0]
        .mode = ku::ast::ParamMode::Owned;
    assert!(verify_borrow_contract(&bad).is_err());
}

#[test]
fn borrow_ir_verifier_rejects_erased_aliases_bad_temp_order_and_deep_input() {
    use ku::ir::TempId;
    let good = ir("fn Read(&text: str): int { return text.len() } fn main() {}");
    for variant in 0..5 {
        let mut bad = good.clone();
        let f = bad.functions.iter_mut().find(|f| f.name == "Read").unwrap();
        let root = IrExpr {
            kind: IrExprKind::BorrowedParam("text".into()),
            ty: IrType::Str,
        };
        f.blocks[0].instructions = vec![IrInst::Temp {
            id: TempId(0),
            ty: IrType::Str,
            value: root.clone(),
        }];
        f.blocks[0].terminator = IrTerminator::Return(None);
        let value = match variant {
            0 => IrExpr {
                kind: IrExprKind::Temp(TempId(0)),
                ty: IrType::Str,
            },
            1 => IrExpr {
                kind: IrExprKind::BorrowedTemp(TempId(0)),
                ty: IrType::Array(Box::new(IrType::Str)),
            },
            2 => IrExpr {
                kind: IrExprKind::Temp(TempId(2)),
                ty: IrType::Str,
            },
            3 => root,
            _ => {
                let mut expr = root;
                for _ in 0..130 {
                    expr = IrExpr {
                        ty: IrType::Str,
                        kind: IrExprKind::Borrow(Box::new(expr)),
                    };
                }
                expr
            }
        };
        f.blocks[0].instructions.push(IrInst::Temp {
            id: TempId(if variant == 3 { 0 } else { 1 }),
            ty: value.ty.clone(),
            value,
        });
        assert!(verify_borrow_contract(&bad).is_err(), "variant {variant}");
        assert!(generate_c_source(&bad).is_err(), "variant {variant}");
    }
}

#[test]
fn borrow_ir_verifier_checks_assignment_target_expressions() {
    use ku::ir::{IrCallKind, TempId};
    let good = ir("struct Box { value: int } fn Consume(text: str): int { return text.len() } fn Read(&text: str, nums: [int]): int { return text.len() } fn main() {}");
    let consume = good
        .functions
        .iter()
        .find(|f| f.name == "Consume")
        .unwrap()
        .id;
    for location in 0..4 {
        let mut bad = good.clone();
        let f = bad.functions.iter_mut().find(|f| f.name == "Read").unwrap();
        let number = IrExpr {
            kind: IrExprKind::Literal("0".into()),
            ty: IrType::Int,
        };
        let hidden_move = IrExpr {
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    kind: IrExprKind::Local("Consume".into()),
                    ty: IrType::Function,
                }),
                args: vec![IrExpr {
                    kind: IrExprKind::Temp(TempId(0)),
                    ty: IrType::Str,
                }],
                kind: IrCallKind::Direct(consume),
            },
            ty: IrType::Int,
        };
        f.blocks[0].instructions = vec![IrInst::Temp {
            id: TempId(0),
            ty: IrType::Str,
            value: IrExpr {
                kind: IrExprKind::BorrowedParam("text".into()),
                ty: IrType::Str,
            },
        }];
        let instruction = match location {
            0 => IrInst::Store {
                target: IrLValue::Index {
                    target: IrExpr {
                        kind: IrExprKind::Local("nums".into()),
                        ty: IrType::Array(Box::new(IrType::Int)),
                    },
                    index: hidden_move,
                },
                value: number.clone(),
            },
            1 => IrInst::Store {
                target: IrLValue::Index {
                    target: IrExpr {
                        kind: IrExprKind::Array(vec![hidden_move]),
                        ty: IrType::Array(Box::new(IrType::Int)),
                    },
                    index: number.clone(),
                },
                value: number.clone(),
            },
            2 => IrInst::Store {
                target: IrLValue::Field {
                    target: IrExpr {
                        kind: IrExprKind::StructLiteral {
                            name: "Box".into(),
                            fields: vec![("value".into(), hidden_move)],
                        },
                        ty: IrType::Named("Box".into()),
                    },
                    name: "value".into(),
                },
                value: number.clone(),
            },
            _ => {
                f.blocks[0].instructions.push(IrInst::CellNew {
                    name: "cell".into(),
                    ty: IrType::Int,
                    init: number.clone(),
                });
                let cell_ty = IrType::Cell(Box::new(IrType::Int));
                IrInst::CellStore {
                    cell: IrExpr {
                        kind: IrExprKind::Index {
                            target: Box::new(IrExpr {
                                kind: IrExprKind::Array(vec![IrExpr {
                                    kind: IrExprKind::Local("cell".into()),
                                    ty: cell_ty.clone(),
                                }]),
                                ty: IrType::Array(Box::new(cell_ty.clone())),
                            }),
                            index: Box::new(hidden_move),
                        },
                        ty: cell_ty,
                    },
                    value: number,
                }
            }
        };
        f.blocks[0].instructions.push(instruction);
        f.blocks[0].terminator = IrTerminator::Return(None);
        let error = verify_borrow_contract(&bad).expect_err("assignment child must be checked");
        assert!(
            error
                .to_string()
                .contains("cannot move, store or return borrowed value"),
            "location {location}: {error}"
        );
        assert!(generate_c_source(&bad).is_err(), "location {location}");
    }
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn borrow_native_runs_without_sources_and_matches_interpreter() {
    let dir = Fixture(std::env::temp_dir().join(format!(
            "ku-borrow-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir(&dir.0).unwrap();
    let source = dir.0.join("main.ku");
    fs::write(&source, SOURCE).unwrap();
    let limits = OutputLimits::new(2 * 1024 * 1024, 4 * 1024 * 1024);
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .current_dir(&dir.0)
            .args(["run", "main.ku"]),
        Duration::from_secs(20),
        limits,
    )
    .unwrap();
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    let exe = if cfg!(windows) { "out.exe" } else { "out" };
    let built = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .current_dir(&dir.0)
            .args(["build", "--native", "main.ku", "-o", exe]),
        Duration::from_secs(120),
        limits,
    )
    .unwrap();
    if !built.status.success() {
        let error = String::from_utf8_lossy(&built.stderr);
        if error.contains("C compiler not found") {
            eprintln!("skip binary execution: C compiler unavailable; C artifact contract still mandatory");
            return;
        }
        panic!("native build: {error}");
    }
    fs::remove_file(source).unwrap();
    let moved = dir
        .0
        .join(if cfg!(windows) { "moved.exe" } else { "moved" });
    fs::rename(dir.0.join(exe), &moved).unwrap();
    let native = run_bounded(
        Command::new(moved).current_dir(&dir.0),
        Duration::from_secs(20),
        limits,
    )
    .unwrap();
    assert!(
        native.status.success(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    let native_text = String::from_utf8(native.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    let interpreted_text = String::from_utf8(interpreted.stdout)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(native_text, interpreted_text);
    assert_eq!(
        native_text,
        "6\n3\nHello 世界\n6\n3\nHello 世界\n世界\n6\n3\nHello 世界\n世界\n世界\n世界\n世界\n"
    );
}
