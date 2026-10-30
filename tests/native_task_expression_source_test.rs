//! Copy expression lowering and real generated short-circuit execution.
//! No fixture implements expression evaluation, Start, Await or scope cleanup.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{
        task::{self, TaskBinaryOp, TaskConstant, TaskOp, TaskTerminator, TaskUnaryOp},
        task_lower,
    },
    lexer::Lexer,
    parser::Parser,
};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

fn lowered_json(source: &str) -> task_lower::NativeTaskProgram {
    let mut ast = parsed(source);
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:json".into(),
            span: Default::default(),
        }),
    );
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    task_lower::lower_program(&ast).unwrap_or_else(|error| panic!("{error}\n{source}"))
}

#[test]
fn native_task_json_literal_keeps_source_order_unique_keys_and_owned_inputs() {
    let source = r#"
import json from "std.json"
async fn next(value:int):int! { println(value) return ok(value) }
async fn main():null! {
    value="中文😀\"\\\n\u0000"
    body=json.encode({z:(await next(1))?,y:(await next(2))?,x:(await next(3))?,a:value.clone(),low:-9223372036854775807 - 1,high:9223372036854775807,b:true,n:null})?
    println(body)
    println(value.byte_len())
    return ok(null)
}
"#;
    let source = source.replace("\\u0000", "\0");
    let native = lowered_json(&source);
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let directory = TempDir::new("task-json-literal");
    let path = directory.path().join("literal.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "literal") else {
        return;
    };
    let result = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(&lines[..3], ["1", "2", "3"]);
    assert_eq!(
        lines[3],
        r#"{"a":"中文😀\"\\\n\u0000","b":true,"high":9223372036854775807,"low":-9223372036854775808,"n":null,"x":3,"y":2,"z":1}"#
    );
    assert_eq!(lines[4], "中文😀\"\\\n\0".len().to_string());
    assert_eq!(lines.len(), 5);
    let moved = r#"import json from "std.json" async fn main():null! {value="x" body=json.encode({a:value})? println(value) return ok(null)}"#;
    assert!(ku::cli::check_source("task-json-moved.ku", moved).is_err());
    let duplicate = r#"import json from "std.json" async fn main():null! {body=json.encode({a:1,a:2})? return ok(null)}"#;
    assert!(ku::cli::check_source("task-json-duplicate.ku", duplicate)
        .unwrap_err()
        .message
        .contains("duplicate field"));
    assert!(task_lower::lower_program(&parsed(duplicate))
        .unwrap_err()
        .message
        .contains("duplicate field"));
    let unsupported = r#"import json from "std.json" async fn main():null! {body=json.encode({a:1.5})? return ok(null)}"#;
    assert!(task_lower::lower_program(&parsed(unsupported)).is_err());
    let local = lowered_json(
        r#"import json from "std.json" async fn main():null! {value={a:1} body=json.encode(value)? return ok(null)}"#,
    );
    task::verify_and_plan(&local.tasks, Default::default())
        .expect("a JSON object local is supported");
}

#[test]
fn native_task_json_frame_values_cannot_escape_or_be_reused_after_consumption() {
    use ku::ir::{
        task::{SlotId, TaskJsonOperation, TaskSlotType},
        IrType,
    };
    let native = lowered_json(
        r#"
import json from "std.json"
async fn main():null! {
    value=json.decode("{\"a\":1}")?
    leaf=value.take("a")?
    number=leaf.as_int()?
    println(number)
    return ok(null)
}
"#,
    );
    task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    let json_slot=native.tasks.functions[0].slots.iter().position(|slot|
        matches!(&slot.ty,TaskSlotType::Value{ty:IrType::Named(name),..} if name=="__ku_value")).unwrap();
    let mut parameter = native.tasks.clone();
    parameter.functions[0].parameters.push(SlotId(json_slot));
    assert!(task::verify_and_plan(&parameter, Default::default())
        .unwrap_err()
        .message
        .contains("cannot enter async parameters"));
    let mut outcome = native.tasks.clone();
    outcome.functions[0].result = IrType::Result(Box::new(IrType::Named("__ku_value".into())));
    assert!(task::verify_and_plan(&outcome, Default::default())
        .unwrap_err()
        .message
        .contains("function completion"));
    for consuming in [TaskJsonOperation::Take, TaskJsonOperation::AsInt] {
        let mut reused = native.tasks.clone();
        let (state, index, source) = reused.functions[0]
            .states
            .iter()
            .enumerate()
            .find_map(|(state, block)| {
                block
                    .operations
                    .iter()
                    .enumerate()
                    .find_map(|(index, operation)| match operation {
                        TaskOp::JsonValue {
                            source, operation, ..
                        } if *operation == consuming => Some((state, index, *source)),
                        _ => None,
                    })
            })
            .unwrap();
        reused.functions[0].states[state]
            .operations
            .insert(index + 1, TaskOp::Read { slot: source });
        assert!(task::verify_and_plan(&reused, Default::default())
            .unwrap_err()
            .message
            .contains("not definitely initialized"));
    }
}

#[test]
fn native_task_json_bytes_and_existing_methods_dispatch_receivers_once() {
    let source = r#"
import json from "std.json"
import bytes from "std.bytes"
async fn Text(marker:int,value:str):str! { println(marker) return ok(value) }
async fn Index(marker:int):int! { println(marker) return ok(0) }
async fn Key(marker:int):str! { println(marker) return ok("a") }
async fn ByteNamed():null! {
    value=bytes.from_str((await Text(1,"AB"))?)?
    code=value.get((await Index(2))?)?
    println(code)
    copied=value.clone()
    println(value.len())
    println(copied.len())
    return ok(null)
}
async fn JsonNamed():null! {
    value=json.decode((await Text(5,"{\"a\":\"named\"}"))?)?
    selected=value[(await Key(6))?]?
    println(selected.as_str()?)
    println(json.encode(value)?)
    return ok(null)
}
async fn JsonTemporary():null! {
    selected=(json.decode((await Text(7,"{\"a\":9}"))?)?)[(await Key(8))?]?
    println(selected.as_int()?)
    return ok(null)
}
async fn JsonTake():null! {
    selected=(json.decode((await Text(9,"{\"a\":\"taken\"}"))?)?).take((await Key(10))?)?
    println(selected.as_str()?)
    return ok(null)
}
async fn ExistingMethods():null! {
    values=[11,12]
    cloned=values.clone()
    println(values.len())
    println(cloned.try_get((await Index(11))?)?)
    value=(await Text(12,"plain"))?
    copied=value.clone()
    println(value.len())
    println(copied.byte_len())
    return ok(null)
}
async fn main():null! {
    done=(await ByteNamed())?
    done=(await JsonNamed())?
    done=(await JsonTemporary())?
    done=(await JsonTake())?
    done=(await ExistingMethods())?
    return ok(null)
}
"#;
    let mut ast = parsed(source);
    for name in ["std:json", "std:bytes"] {
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: name.into(),
                span: Default::default(),
            }),
        );
    }
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    let native =
        task_lower::lower_program(&ast).unwrap_or_else(|error| panic!("{error}\n{source}"));
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let directory = TempDir::new("task-json-typed-methods");
    let path = directory.path().join("methods.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "methods") else {
        return;
    };
    let result = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap().replace('\r', "");
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "1",
            "2",
            "65",
            "2",
            "2",
            "5",
            "6",
            "named",
            r#"{"a":"named"}"#,
            "7",
            "8",
            "9",
            "9",
            "10",
            "taken",
            "2",
            "11",
            "11",
            "12",
            "5",
            "5",
        ]
    );
}

#[test]
fn native_task_json_literal_allocation_refusals_are_results_and_reclaim_charge() {
    let native = lowered_json(
        r#"import json from "std.json" async fn main():null! { try {text=json.encode({a:"hello",b:1})? println(text)} catch(error){println(error.code)} return ok(null)}"#,
    );
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for (name,needle,replacement,expected) in [
        ("oom","buffer.data=(uint8_t*)malloc(capacity);buffer.cap=capacity;","buffer.data=NULL;buffer.cap=capacity;","out_of_memory"),
        ("limit","if(status==KU_TASK_DRIVER_OK)status=ku_task_driver_grow_charge(host->ticket,capacity);","if(status==KU_TASK_DRIVER_OK)status=KU_TASK_DRIVER_LIMIT;","resource_limit"),
        ("output-limit","#define KU_JSON_MAX_OUTPUT_BYTES ((size_t)1000000)","#define KU_JSON_MAX_OUTPUT_BYTES ((size_t)18)","stringify_error"),
        // Integer capacity is conservatively 20 bytes. Saturating the prepaid
        // capacity at the output limit must still permit a smaller actual
        // representation that fits the exact bound.
        ("output-exact","#define KU_JSON_MAX_OUTPUT_BYTES ((size_t)1000000)","#define KU_JSON_MAX_OUTPUT_BYTES ((size_t)19)",r#"{"a":"hello","b":1}"#),
    ] {
        assert_eq!(generated.matches(needle).count(),1);
        let directory=TempDir::new(&format!("task-json-{name}"));
        let path=directory.path().join("failure.c");fs::write(&path,generated.replacen(needle,replacement,1)).unwrap();
        let Some(executable)=compile_harness(directory.path(),&path,"failure") else {return;};
        let output=run_bounded(Command::new(executable).current_dir(directory.path()),RUN_TIMEOUT,RUN_LIMITS).unwrap();
        assert!(output.status.success(),"{name}: {}{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(),expected);
    }
}

fn parsed(source: &str) -> ku::ast::Program {
    Parser::new(Lexer::new(source).lex().expect("source lexes"))
        .parse_program()
        .expect("source parses")
}
fn lowered(source: &str) -> task_lower::NativeTaskProgram {
    let ast = parsed(source);
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    task_lower::lower_program(&ast).unwrap_or_else(|error| panic!("{error}\n{source}"))
}

#[test]
fn native_task_copy_expression_source_emits_only_the_exact_typed_operators() {
    for (expression, result, expected) in [
        ("x + y", "int", TaskBinaryOp::Add),
        ("x - y", "int", TaskBinaryOp::Subtract),
        ("x * y", "int", TaskBinaryOp::Multiply),
        ("x / y", "int", TaskBinaryOp::Divide),
        ("x % y", "int", TaskBinaryOp::Remainder),
        ("x == y", "bool", TaskBinaryOp::Equal),
        ("x != y", "bool", TaskBinaryOp::NotEqual),
        ("x < y", "bool", TaskBinaryOp::Less),
        ("x <= y", "bool", TaskBinaryOp::LessEqual),
        ("x > y", "bool", TaskBinaryOp::Greater),
        ("x >= y", "bool", TaskBinaryOp::GreaterEqual),
        ("b == c", "bool", TaskBinaryOp::Equal),
        ("b != c", "bool", TaskBinaryOp::NotEqual),
    ] {
        let source = format!("async fn Probe(x: int, y: int, b: bool, c: bool): {result}! {{ return ok({expression}) }} async fn main(): null! {{ return ok(null) }}");
        let native = lowered(&source);
        let operations = native.tasks.functions[0]
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .collect::<Vec<_>>();
        let binary = operations
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| match operation {
                TaskOp::Binary {
                    op,
                    left,
                    right,
                    dst,
                } => Some((index, *op, *left, *right, *dst)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(binary.len(), 1);
        let (index, actual, left, right, dst) = binary[0];
        assert_eq!(actual, expected);
        assert_ne!(dst, left);
        assert_ne!(dst, right);
        assert!(operations[..index]
            .iter()
            .any(|operation| matches!(operation, TaskOp::Copy { dst, .. } if *dst == left)));
        let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
        assert!(plan
            .functions
            .iter()
            .all(|frame| frame.hosted && frame.exit_bridge));
        assert!(plan
            .functions
            .iter()
            .all(|frame| frame.scope_task_mask == 0));
    }
    for (expression, parameter, result, expected) in [
        ("-x", "x: int", "int", TaskUnaryOp::Negate),
        ("!b", "b: bool", "bool", TaskUnaryOp::Not),
    ] {
        let native = lowered(&format!("async fn Probe({parameter}): {result}! {{ return ok({expression}) }} async fn main(): null! {{ return ok(null) }}"));
        let unary = native.tasks.functions[0]
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter_map(|operation| match operation {
                TaskOp::Unary { op, .. } => Some(*op),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(unary, [expected]);
    }
}

#[test]
fn native_task_copy_expression_lhs_snapshot_crosses_rhs_await() {
    let native = lowered("async fn Left(): int! { return ok(7) } async fn Right(): int! { return ok(2) } async fn main(): null! { value = (await Left())? + (await Right())? println(value) return ok(null) }");
    let main = &native.tasks.functions[native.entry.0];
    let left = main
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .find_map(|operation| match operation {
            TaskOp::Binary {
                op: TaskBinaryOp::Add,
                left,
                ..
            } => Some(*left),
            _ => None,
        })
        .unwrap();
    let right_start = main.states.iter().find_map(|state| {
        state.operations.iter().position(|operation| matches!(operation, TaskOp::Start { function, .. } if function.0 == 1))
            .map(|position| (&state.operations, position))
    }).unwrap();
    assert!(right_start.0[..right_start.1]
        .iter()
        .any(|operation| matches!(operation, TaskOp::Copy { dst, .. } if *dst == left)));
    let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert!(plan.functions[native.entry.0].slots.contains(&left));
    assert!(plan.functions[native.entry.0]
        .suspensions
        .iter()
        .any(|suspension| suspension.slots.contains(&left)));
    let awaits = main
        .states
        .iter()
        .filter(|state| matches!(state.terminator, TaskTerminator::Await { .. }))
        .collect::<Vec<_>>();
    assert_eq!(awaits.len(), 2);
    assert!(awaits.iter().all(|state| state.operations.is_empty()));
    // Source exits use one typed bridge. Only Await cancellation regions keep
    // the synthetic reverse-order Value cleanup; ordinary Exit must not free
    // scope Values before the adapter has handed off remaining Task owners.
    assert!(native.tasks.functions.iter().all(|function| {
        function.states.iter().all(|state| match state.terminator {
            TaskTerminator::Complete { .. } => false,
            TaskTerminator::Exit { .. } => !state
                .operations
                .iter()
                .any(|operation| matches!(operation, TaskOp::DropIfInit { .. })),
            _ => true,
        })
    }));
    for state in awaits {
        let TaskTerminator::Await { cleanup, .. } = state.terminator else {
            unreachable!()
        };
        let cleanup = &main.states[cleanup.0];
        assert!(matches!(cleanup.terminator, TaskTerminator::Terminate));
        assert!(!cleanup.operations.is_empty());
        assert!(cleanup
            .operations
            .iter()
            .all(|operation| matches!(operation, TaskOp::DropIfInit { .. })));
    }
}

#[test]
fn native_task_string_concat_snapshots_before_rhs_move_and_await() {
    let native=lowered("async fn Right(value: str): str! { return ok(value) } async fn main(): null! { value=\"中\0😀\" joined=value+(await Right(value))? println(joined) return ok(null) }");
    let main = &native.tasks.functions[native.entry.0];
    let (concat_state, concat_position, left) = main
        .states
        .iter()
        .enumerate()
        .find_map(|(state, body)| {
            body.operations
                .iter()
                .enumerate()
                .find_map(|(position, op)| match op {
                    TaskOp::StringConcat { left, .. } => Some((state, position, *left)),
                    _ => None,
                })
        })
        .unwrap();
    let (start_state, start_position) = main
        .states
        .iter()
        .enumerate()
        .find_map(|(state, body)| {
            body.operations
                .iter()
                .position(|op| matches!(op,TaskOp::Start {function,..} if function.0==0))
                .map(|position| (state, position))
        })
        .unwrap();
    let snapshot = main.states[start_state].operations[..start_position]
        .iter()
        .position(|op| matches!(op,TaskOp::StringClone {dst,..} if *dst==left))
        .unwrap();
    let source = match main.states[start_state].operations[snapshot] {
        TaskOp::StringClone { src, .. } => src,
        _ => unreachable!(),
    };
    assert_ne!(source, left);
    assert!(
        main.states[start_state].operations[snapshot + 1..start_position]
            .iter()
            .any(|op| matches!(op,TaskOp::Move {src,..} if *src==source))
    );
    assert!(
        matches!(main.states[concat_state].operations[concat_position+1],TaskOp::Drop {slot} if slot==left)
    );
    let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert!(plan.functions[native.entry.0]
        .suspensions
        .iter()
        .any(|suspension| suspension.slots.contains(&left)));
}

#[test]
fn native_task_short_circuit_nested_rhs_joins_from_its_actual_final_state() {
    let native = lowered("async fn Flag(): bool! { return ok(true) } async fn main(): null! { value = false || ((await Flag())? && true) println(value) return ok(null) }");
    let main = &native.tasks.functions[native.entry.0];
    let entry = &main.states[main.entry.0];
    let TaskTerminator::Branch {
        then_state: join,
        else_state: rhs,
        ..
    } = entry.terminator
    else {
        panic!("OR must branch before RHS Start")
    };
    let destination = match entry.operations.last().unwrap() {
        TaskOp::Init {
            dst,
            value: TaskConstant::Bool(true),
        } => *dst,
        _ => panic!("OR default must dominate both paths"),
    };
    assert!(main.states[rhs.0]
        .operations
        .iter()
        .any(|operation| matches!(operation, TaskOp::Start { .. })));
    assert!(!main.states[rhs.0]
        .operations
        .iter()
        .any(|operation| matches!(operation, TaskOp::Copy { dst, .. } if *dst == destination)));
    let writers = main
        .states
        .iter()
        .enumerate()
        .filter(|(_, state)| {
            state.operations.iter().any(
                |operation| matches!(operation, TaskOp::Copy { dst, .. } if *dst == destination),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(writers.len(), 1);
    assert_ne!(writers[0].0, rhs.0);
    assert_eq!(
        writers[0].1.terminator,
        TaskTerminator::Jump { target: join }
    );
    let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert!(plan.functions[native.entry.0].hosted);
    assert_ne!(plan.functions[native.entry.0].scope_task_mask, 0);
}

#[test]
fn native_task_copy_expression_type_and_skipped_rhs_budget_gates_remain_strict() {
    for expression in [
        "!1",
        "-true",
        "true + false",
        "1 == true",
        "true < false",
        "null == null",
        "\"a\" + 1",
        "ok(1) == ok(1)",
        "Child() == Child()",
        "1 && true",
        "true || 1",
        "false && (1 + true)",
        "true || (1 == true)",
    ] {
        // Deliberately bypass the checker: this internal backend API must still
        // reject wider types and must validate an unreachable RHS.
        let source = format!("async fn Child(): int! {{ return ok(1) }} async fn main(): null! {{ value = {expression} return ok(null) }}");
        let error = task_lower::lower_program(&parsed(&source)).expect_err(expression);
        assert!(
            error.message.contains("not support"),
            "{expression}: {error}"
        );
    }
    // String equality is already a supported read-only operation. Keep its
    // positive source contract separate from genuinely invalid operand types.
    for expression in ["\"a\" == \"b\"", "true || (\"a\" == \"a\")"] {
        let source = format!("async fn main():null! {{ value={expression} return ok(null) }}");
        task_lower::lower_program(&parsed(&source)).expect("supported string comparison");
    }
    // Parser/checker retain their separate, tighter depth limits. Construct a
    // typed AST only here to reach the lowerer's independent slot/depth gates;
    // this is not a claim that deeply nested source syntax has been opened.
    let budget_ast = |count, skipped| {
        use ku::ast::{BinaryOp, Expr, ExprKind, Item, Literal, Stmt, UnaryOp};
        let mut ast = parsed("async fn Probe(flag: bool): bool! { return ok(flag) } async fn main(): null! { return ok(null) }");
        let Item::Function(function) = &mut ast.items[0] else {
            unreachable!()
        };
        let Stmt::Return {
            value: Some(value), ..
        } = &mut function.body[0]
        else {
            unreachable!()
        };
        let ExprKind::Call { args, .. } = &mut value.kind else {
            unreachable!()
        };
        let span = args[0].span;
        let mut expression = args[0].clone();
        for _ in 0..count {
            expression = Expr::new(
                ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expression),
                },
                span,
            );
        }
        if skipped {
            expression = Expr::new(
                ExprKind::Binary {
                    left: Box::new(Expr::new(ExprKind::Literal(Literal::Bool(false)), span)),
                    op: BinaryOp::And,
                    right: Box::new(expression),
                },
                span,
            );
        }
        args[0] = expression;
        ast
    };
    assert_eq!(
        task_lower::lower_program(&budget_ast(62, false))
            .unwrap()
            .tasks
            .functions[0]
            .slots
            .len(),
        64
    );
    let compacted = task_lower::lower_program(&budget_ast(63, false)).unwrap();
    assert!(
        compacted.tasks.functions[0].slots.len() < 64,
        "dead Copy temporaries beyond logical64 must reuse physical slots"
    );
    task::verify_and_plan(&compacted.tasks, task::TaskLimits::default()).unwrap();
    let error = task_lower::lower_program(&budget_ast(64, true)).unwrap_err();
    assert!(
        error
            .message
            .contains("expressions beyond the Task analysis budget"),
        "{error}"
    );
    // The internal API must reject deeply nested call inputs without exhausting
    // the unchanged worker stack, even when the checker was bypassed.
    let mut nested_calls = budget_ast(0, false);
    let ku::ast::Item::Function(function) = &mut nested_calls.items[0] else {
        unreachable!()
    };
    let ku::ast::Stmt::Return {
        value: Some(value), ..
    } = &mut function.body[0]
    else {
        unreachable!()
    };
    let ku::ast::ExprKind::Call { args, .. } = &mut value.kind else {
        unreachable!()
    };
    let mut nested = args[0].clone();
    for _ in 0..63 {
        let span = nested.span;
        nested = ku::ast::Expr::new(
            ku::ast::ExprKind::Call {
                callee: Box::new(ku::ast::Expr::new(
                    ku::ast::ExprKind::Variable("Probe".into()),
                    span,
                )),
                args: vec![nested],
            },
            span,
        );
    }
    args[0] = nested;
    let error = task_lower::lower_program(&nested_calls).unwrap_err();
    assert!(
        error.message.contains("mismatched async argument types"),
        "{error}"
    );
}

fn existing_task_short_circuit_source(expression: &str, continuation: &str) -> String {
    format!("async fn Broken(): bool! {{ fail \"boom\" }} async fn main(): null! {{ existing = Broken() value = {expression} (await existing)? {continuation} return ok(null) }}")
}

#[test]
fn native_task_existing_handle_starts_before_short_circuit_and_awaits_only_on_rhs() {
    for (expression, skipped_value) in [("false &&", false), ("true ||", true)] {
        let native = lowered(&existing_task_short_circuit_source(
            expression,
            "println(value)",
        ));
        let main = &native.tasks.functions[native.entry.0];
        let entry = &main.states[main.entry.0];
        let starts = main
            .states
            .iter()
            .enumerate()
            .flat_map(|(state, block)| {
                block
                    .operations
                    .iter()
                    .filter_map(move |operation| match operation {
                        TaskOp::Start { dst, .. } => Some((state, *dst)),
                        _ => None,
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            starts.len(),
            1,
            "existing Task must not become lazy or duplicate"
        );
        assert_eq!(starts[0].0, main.entry.0, "Start precedes the Branch");
        let existing = entry
            .operations
            .iter()
            .find_map(|operation| match operation {
                TaskOp::Move { dst, src } if *src == starts[0].1 => Some(*dst),
                _ => None,
            })
            .expect("entry retains the started Task in its existing owner slot");
        let TaskTerminator::Branch {
            condition,
            then_state,
            else_state,
        } = entry.terminator
        else {
            panic!("existing Task must start before the logical Branch");
        };
        assert!(entry.operations.iter().any(|operation| matches!(
            operation, TaskOp::Init { dst, value: TaskConstant::Bool(value) }
                if *dst == condition && *value == skipped_value
        )));
        let (rhs, join) = if skipped_value {
            (else_state, then_state)
        } else {
            (then_state, else_state)
        };
        let TaskTerminator::Jump { target: poll } = main.states[rhs.0].terminator else {
            panic!("only the logical RHS enters the Await poll");
        };
        let TaskTerminator::Await {
            task: hidden,
            dst,
            ready,
            ..
        } = main.states[poll.0].terminator
        else {
            panic!("RHS must retain its real Await");
        };
        assert!(main.states[rhs.0]
            .operations
            .iter()
            .any(|operation| matches!(
                operation, TaskOp::Move { dst, src } if *src == existing && *dst == hidden
            )));
        assert_eq!(
            main.states
                .iter()
                .filter(|state| matches!(state.terminator, TaskTerminator::Await { .. }))
                .count(),
            1
        );
        assert!(matches!(main.states[ready.0].terminator,
            TaskTerminator::TryResult { src, .. } if src == dst));
        let skipped = &main.states[join.0];
        assert!(matches!(skipped.terminator, TaskTerminator::Exit { .. }));
        assert!(!skipped.operations.iter().any(|operation| matches!(
            operation, TaskOp::Move { src, .. } if *src == existing
        )));
        let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
        let frame = &plan.functions[native.entry.0];
        assert!(
            frame.exit_bridge && frame.scope_task_mask & (1u64 << existing.0) != 0,
            "skipped RHS leaves the existing Task for real scope-exit handoff"
        );
    }
}

#[test]
fn native_task_copy_expression_order_and_short_circuit_execute_without_source() {
    let existing_and = existing_task_short_circuit_source("false &&", "println(value)");
    let existing_or = existing_task_short_circuit_source("true ||", "println(value)");
    let observed_error = existing_task_short_circuit_source("true &&", "println(\"BAD\")");
    let nul_methods = r#"async fn main(): null! { text="中\0😀" println(text.len()) println(text.byte_len()) copy=text.clone() println(copy.len()) println(text.byte_len()) owned=text+"!" again=owned.clone() println(again.len()) println(owned.byte_len()) empty=""+"" copied=empty.clone() println(copied.len()) println(empty.byte_len()) return ok(null) }"#.replace("\\0", "\0");
    let nul_await = r#"async fn Right(value: str): str! { println("R") return ok(value) } async fn main(): null! { text="中\0😀" joined=text+(await Right(text))? println(joined.len()) println(joined.byte_len()) println(joined) return ok(null) }"#.replace("\\0", "\0");
    let cases = [
        (r#"
async fn Cloned(): null! { text="x" index=0 while (index<2 && text.clone().len()>0) { index=index+1 } println(index) return ok(null) }
async fn RightJoined(): null! { text="x" index=0 while (index<2 && (text+"x").len()>0) { index=index+1 } println(index) return ok(null) }
async fn LeftJoined(): null! { text="x" index=0 while (index<2 && ("x"+text).len()>0) { index=index+1 } println(index) return ok(null) }
async fn main(): null! { (await Cloned())?; (await RightJoined())?; (await LeftJoined())?; return ok(null) }
"#, "2\n2\n2\n", "", true),
        ("async fn Left(): int! { println(\"L\") return ok(7) } async fn Right(): int! { println(\"R\") return ok(2) } async fn Combine(a: int, b: int): int! { println(\"C\") return ok(a + b) } async fn main(): null! { value = (await Combine((await Left())? + 1, (await Right())? * 3))? println(value) return ok(null) }", "L\nR\nC\n14\n", "", true),
        ("async fn Flag(value: bool): bool! { println(\"R\") return ok(value) } async fn main(): null! { a = false && (await Flag(true))? println(a) b = true || (await Flag(false))? println(b) c = true && (await Flag(true))? println(c) d = false || (await Flag(false))? println(d) return ok(null) }", "false\ntrue\nR\ntrue\nR\nfalse\n", "", true),
        ("async fn Broken(): bool! { println(\"BAD\") fail \"boom\" } async fn main(): null! { a = false && (await Broken())? println(a) b = true || ((await Broken())? && ((1 / 0) == 0)) println(b) c = false && ((1 / 0) == 0) println(c) return ok(null) }", "false\ntrue\nfalse\n", "", true),
        ("async fn Broken(): bool! { println(\"B\") fail \"boom\" } async fn main(): null! { value = true && (await Broken())? println(\"BAD\") return ok(null) }", "B\n", "boom\n", false),
        // An already-started child may run before scope exit cancels it. Its
        // recoverable error has no output and is observed only by a real Await.
        (existing_and.as_str(), "false\n", "", true),
        (existing_or.as_str(), "true\n", "", true),
        (observed_error.as_str(), "", "boom\n", false),
        (nul_methods.as_str(), "3\n8\n3\n8\n4\n9\n0\n0\n", "", true),
        (nul_await.as_str(), "R\n6\n16\n中\0😀中\0😀\n", "", true),
        (r#"async fn Broken(value: str): str! { println(value) fail "concat RHS failed" } async fn main(): null! { text="owned"+" left" result=text+(await Broken(text))? println(result) return ok(null) }"#, "owned left\n", "concat RHS failed\n", false),
    ];
    for (index, (source, stdout, stderr, success)) in cases.into_iter().enumerate() {
        let native = lowered(source);
        let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
        assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
        let directory = TempDir::new(&format!("native-task-expression-{index}"));
        let path = directory.path().join("program.c");
        fs::write(&path, generated).unwrap();
        let Some(executable) = compile_harness(directory.path(), &path, "program") else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must execute real native expressions"
            );
            continue;
        };
        fs::remove_file(path).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("expressions terminate under the real process watchdog");
        assert_eq!(
            output.status.success(),
            success,
            "case {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            stdout,
            "case {index}"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().replace('\r', ""),
            stderr,
            "case {index}"
        );
    }
}

#[test]
fn native_task_array_try_get_source_keeps_pure_indices_free_of_receiver_copies() {
    for (element, index) in [
        ("int", "0"),
        ("bool", "index"),
        ("str", "index + values.len() - 1"),
        ("str", "text.byte_len() - text.len()"),
        ("int", "-1"),
        ("int", "9223372036854775807"),
    ] {
        let source = format!("async fn Probe(values: [{element}], index: int, text: str): {element}! {{ return values.try_get({index}) }} async fn main(): null! {{ return ok(null) }}");
        let native = lowered(&source);
        let probe = &native.tasks.functions[0];
        let reads = probe
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter_map(|op| match op {
                TaskOp::ArrayTryGet { dst, array, index } => Some((*dst, *array, *index)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 1, "{source}");
        assert_eq!(
            reads[0].1, probe.parameters[0],
            "pure index keeps the original View"
        );
        assert!(!probe
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .any(|op| matches!(op, TaskOp::ArrayClone { .. })));
        task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    }
    fn sum(leaves: usize) -> String {
        if leaves == 1 {
            "values.len()".into()
        } else {
            format!("({}+{})", sum(leaves / 2), sum(leaves - leaves / 2))
        }
    }
    // These sources remain below the original slot/depth limits. The bounded
    // purity precheck falls back rather than extending its 64-node worklist.
    for (leaves, snapshots) in [(16usize, 0usize), (17, 1)] {
        let source = format!("async fn Probe(values: [int]): int! {{ return values.try_get({}) }} async fn main(): null! {{ return ok(null) }}",sum(leaves));
        let native = lowered(&source);
        let actual = native.tasks.functions[0]
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, TaskOp::ArrayClone { .. }))
            .count();
        assert_eq!(actual, snapshots, "{leaves} read-only calls");
        task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    }
}

#[test]
fn native_task_array_try_get_freezes_effectful_indices_and_drops_anonymous_receivers() {
    for (receiver, argument, snapshots) in [
        ("values", "values", 1usize),
        ("values", "values.clone()", 2),
        ("values.clone()", "values", 1),
    ] {
        let source = format!("async fn Consume(values: [int]): int! {{ return ok(values.len()-1) }} async fn Probe(values: [int]): int! {{ return {receiver}.try_get((await Consume({argument}))?) }} async fn main(): null! {{ return ok(null) }}");
        let native = lowered(&source);
        let probe = &native.tasks.functions[1];
        let (array, dst) = probe
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .find_map(|op| match op {
                TaskOp::ArrayTryGet { dst, array, .. } => Some((*array, *dst)),
                _ => None,
            })
            .unwrap();
        let entry = &probe.states[probe.entry.0].operations;
        let (cloned, clone_position) = entry
            .iter()
            .enumerate()
            .find_map(|(position, op)| match op {
                TaskOp::ArrayClone { dst, src } if *src == probe.parameters[0] => {
                    Some((*dst, position))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(cloned, array, "{source}");
        let start = entry
            .iter()
            .position(|op| matches!(op, TaskOp::Start { .. }))
            .unwrap();
        assert!(
            clone_position < start,
            "receiver finishes before the index starts"
        );
        assert_eq!(
            probe
                .states
                .iter()
                .flat_map(|state| &state.operations)
                .filter(|op| matches!(op, TaskOp::ArrayClone { .. }))
                .count(),
            snapshots
        );
        assert!(probe.states.iter().any(|state| state.operations.windows(2).any(|pair| matches!((&pair[0],&pair[1]),(TaskOp::ArrayTryGet{dst:actual,..},TaskOp::Drop{slot}) if *actual==dst && *slot==array))));
        let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
        assert!(plan.functions[1]
            .suspensions
            .iter()
            .any(|suspension| suspension.slots.contains(&array)));
    }
    let native = lowered("async fn main(): null! { index=0 while (index<2 && ([10,20].try_get(0))?>0) { index=index+1 } return ok(null) }");
    let main = &native.tasks.functions[native.entry.0];
    assert!(!main
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .any(|op| matches!(op, TaskOp::ArrayClone { .. })));
    assert!(main.states.iter().any(|state|state.operations.windows(2).any(|pair|matches!((&pair[0],&pair[1]),(TaskOp::ArrayTryGet{array,..},TaskOp::Drop{slot}) if array==slot))));
    task::verify_and_plan(&native.tasks, Default::default()).unwrap();
}
