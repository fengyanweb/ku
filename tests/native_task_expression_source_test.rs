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
        "\"a\" + \"b\"",
        "\"a\" == \"b\"",
        "ok(1) == ok(1)",
        "Child() == Child()",
        "1 && true",
        "true || 1",
        "false && (1 + true)",
        "true || (\"a\" == \"a\")",
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
    let error = task_lower::lower_program(&budget_ast(63, false)).unwrap_err();
    assert!(
        error
            .message
            .contains("more than 64 generated Task frame slots"),
        "{error}"
    );
    let error = task_lower::lower_program(&budget_ast(64, true)).unwrap_err();
    assert!(
        error
            .message
            .contains("expressions beyond the Task analysis budget"),
        "{error}"
    );
}

#[test]
fn native_task_copy_expression_order_and_short_circuit_execute_without_source() {
    let cases = [
        ("async fn Left(): int! { println(\"L\") return ok(7) } async fn Right(): int! { println(\"R\") return ok(2) } async fn Combine(a: int, b: int): int! { println(\"C\") return ok(a + b) } async fn main(): null! { value = (await Combine((await Left())? + 1, (await Right())? * 3))? println(value) return ok(null) }", "L\nR\nC\n14\n", "", true),
        ("async fn Flag(value: bool): bool! { println(\"R\") return ok(value) } async fn main(): null! { a = false && (await Flag(true))? println(a) b = true || (await Flag(false))? println(b) c = true && (await Flag(true))? println(c) d = false || (await Flag(false))? println(d) return ok(null) }", "false\ntrue\nR\ntrue\nR\nfalse\n", "", true),
        ("async fn Broken(): bool! { println(\"BAD\") fail \"boom\" } async fn main(): null! { a = false && (await Broken())? println(a) b = true || ((await Broken())? && ((1 / 0) == 0)) println(b) c = false && ((1 / 0) == 0) println(c) return ok(null) }", "false\ntrue\nfalse\n", "", true),
        ("async fn Broken(): bool! { println(\"B\") fail \"boom\" } async fn main(): null! { value = true && (await Broken())? println(\"BAD\") return ok(null) }", "B\n", "boom\n", false),
        ("async fn Flag(): bool! { println(\"BAD\") return ok(true) } async fn main(): null! { existing = Flag() value = false && (await existing)? println(value) return ok(null) }", "false\n", "", true),
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
