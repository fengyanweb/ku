//! Structural source-to-IR contracts for synchronous checked arithmetic.
//! These are not native execution/ownership evidence; companion real-C tests
//! must validate the mailbox, typed drops, callbacks and request-root handling.
use ku::{
    ast::{BinaryOp, UnaryOp},
    backend::llvm,
    checker::Checker,
    ir::{
        self, BlockId, FunctionId, IrBlock, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst,
        IrProgram, IrTerminator, IrType, TempId,
    },
    lexer::Lexer,
    parser::Parser,
};
use std::collections::HashSet;

fn lower(source: &str) -> IrProgram {
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let program = ir::lower_program(&ast).unwrap();
    ir::verify_borrow_contract(&program).unwrap();
    program
}

fn function<'a>(program: &'a IrProgram, name: &str) -> &'a IrFunction {
    program
        .functions
        .iter()
        .find(|function| function.name == name)
        .unwrap()
}

fn block(function: &IrFunction, id: BlockId) -> &IrBlock {
    function.blocks.iter().find(|block| block.id == id).unwrap()
}

fn inst_expr(inst: &IrInst) -> Option<&IrExpr> {
    match inst {
        IrInst::Temp { value, .. } | IrInst::Expr(value) => Some(value),
        _ => None,
    }
}

fn is_direct(expr: &IrExpr, id: FunctionId) -> bool {
    matches!(&expr.kind, IrExprKind::Call { kind: IrCallKind::Direct(actual), .. } if *actual == id)
}

fn call_block(function: &IrFunction, id: FunctionId) -> &IrBlock {
    function
        .blocks
        .iter()
        .find(|block| {
            block
                .instructions
                .iter()
                .filter_map(inst_expr)
                .any(|expr| is_direct(expr, id))
        })
        .unwrap()
}

fn guard(block: &IrBlock) -> (BlockId, BlockId) {
    match block.terminator {
        IrTerminator::SyncGuard {
            continue_block,
            cleanup_block,
        } => (continue_block, cleanup_block),
        ref other => panic!("expected immediate signal guard, got {other:?}"),
    }
}

fn output(block: &IrBlock) -> TempId {
    match block.instructions.last().unwrap() {
        IrInst::Temp { id, .. } => *id,
        other => panic!("last instruction is not the producing temp: {other:?}"),
    }
}

fn drop_ids(block: &IrBlock) -> Vec<TempId> {
    block
        .instructions
        .iter()
        .filter_map(|inst| {
            let IrInst::Expr(IrExpr {
                kind:
                    IrExprKind::Call {
                        kind: IrCallKind::Intrinsic(name),
                        args,
                        ..
                    },
                ..
            }) = inst
            else {
                return None;
            };
            if name != "__ku_drop_borrow_temp" {
                return None;
            }
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                IrExprKind::Temp(id) => Some(*id),
                other => panic!("abort cleanup must not drop a borrowed/local source: {other:?}"),
            }
        })
        .collect()
}

fn edges(block: &IrBlock) -> Vec<BlockId> {
    match block.terminator {
        IrTerminator::Jump(target) | IrTerminator::JumpErr { target, .. } => vec![target],
        IrTerminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![then_block, else_block],
        IrTerminator::Safepoint {
            continue_block,
            timeout_block,
        } => vec![continue_block, timeout_block],
        IrTerminator::SyncGuard {
            continue_block,
            cleanup_block,
        } => vec![continue_block, cleanup_block],
        IrTerminator::ResultBranch {
            ok_block,
            err_block,
            ..
        } => vec![ok_block, err_block],
        IrTerminator::ForEach {
            body_block,
            after_block,
            ..
        } => vec![body_block, after_block],
        IrTerminator::Next => panic!("finished fixture IR must have explicit control flow"),
        IrTerminator::Return(_) | IrTerminator::PropagateErr(_) | IrTerminator::Unreachable => {
            vec![]
        }
    }
}

fn reachable(function: &IrFunction, from: BlockId, target: BlockId) -> bool {
    let mut todo = vec![from];
    let mut seen = HashSet::new();
    while let Some(id) = todo.pop() {
        if id == target {
            return true;
        }
        if seen.insert(id) {
            todo.extend(edges(block(function, id)));
        }
        assert!(seen.len() <= function.blocks.len());
    }
    false
}

#[test]
fn native_sync_guard_all_six_integer_operations_are_immediate_and_typed() {
    let program = lower(
        r#"
        fn Math(a: int, b: int): int {
            x = -a
            x = a + b
            x = a - b
            x = a * b
            x = a / b
            x = a % b
            return x
        }
        fn Compare(a: int, b: int): bool { return !(a < b) }
        fn Float(a: float, b: float): float { return -a + b }
        fn main(): null! { println(Math(9, 2)) return ok(null) }
    "#,
    );
    let math = function(&program, "Math");
    let mut checked = 0;
    for current in &math.blocks {
        for (index, inst) in current.instructions.iter().enumerate() {
            let Some(value) = inst_expr(inst) else {
                continue;
            };
            let arithmetic = match &value.kind {
                IrExprKind::Unary {
                    op: UnaryOp::Negate,
                    expr,
                } => expr.ty == IrType::Int,
                IrExprKind::Binary { op, left, right } => {
                    left.ty == IrType::Int
                        && right.ty == IrType::Int
                        && matches!(
                            op,
                            BinaryOp::Add
                                | BinaryOp::Subtract
                                | BinaryOp::Multiply
                                | BinaryOp::Divide
                                | BinaryOp::Remainder
                        )
                }
                _ => false,
            };
            if arithmetic {
                checked += 1;
                assert_eq!(index + 1, current.instructions.len());
                let (_, cleanup) = guard(current);
                assert!(matches!(
                    block(math, cleanup).terminator,
                    IrTerminator::Return(_)
                ));
            }
        }
    }
    assert_eq!(checked, 6);
    for name in ["Compare", "Float"] {
        assert!(!function(&program, name)
            .blocks
            .iter()
            .any(|block| matches!(block.terminator, IrTerminator::SyncGuard { .. })));
    }
}

#[test]
fn native_sync_guard_call_precedes_result_unwrap_and_void_return() {
    let program = lower(
        r#"
        fn Fallible(n: int): int! { return ok(n + 1) }
        fn Ping() { println(1) }
        fn Forward() { return Ping() }
        fn main(): null! { Forward() println(Fallible(3)?) return ok(null) }
    "#,
    );
    let main = function(&program, "main");
    let call = call_block(main, function(&program, "Fallible").id);
    let (after_signal, _) = guard(call);
    let after_timeout = match block(main, after_signal).terminator {
        IrTerminator::Safepoint { continue_block, .. } => continue_block,
        ref other => panic!("post-call timeout follows signal, got {other:?}"),
    };
    assert!(matches!(
        block(main, after_timeout).terminator,
        IrTerminator::ResultBranch { .. }
    ));

    let forward = function(&program, "Forward");
    let ping = function(&program, "Ping").id;
    let call = call_block(forward, ping);
    assert!(
        matches!(call.instructions.last(), Some(IrInst::Expr(value)) if is_direct(value, ping))
    );
    let (after_signal, _) = guard(call);
    let after_timeout = match block(forward, after_signal).terminator {
        IrTerminator::Safepoint { continue_block, .. } => continue_block,
        ref other => panic!("void return call must still poll after signal: {other:?}"),
    };
    assert!(matches!(
        block(forward, after_timeout).terminator,
        IrTerminator::Return(None)
    ));
    assert_eq!(
        forward
            .blocks
            .iter()
            .flat_map(|block| &block.instructions)
            .filter_map(inst_expr)
            .filter(|value| is_direct(value, ping))
            .count(),
        1
    );
}

#[test]
fn native_sync_guard_fresh_borrow_arguments_survive_until_guard_not_user_sources() {
    let program = lower(
        r#"
        fn Fresh(): str { return "fresh-" + "argument" }
        fn Calc(n: int): int { return n + 1 }
        fn Use(&value: str, n: int): str { return value.clone() }
        fn Probe(n: int): str {
            stable = "keep-" + "alive"
            try { return Use(Fresh(), Calc(n)) }
            finally { println(stable) }
            return "unreachable"
        }
        fn main(): null! { println(Probe(1)) return ok(null) }
    "#,
    );
    let probe = function(&program, "Probe");
    let fresh_call = call_block(probe, function(&program, "Fresh").id);
    let fresh = output(fresh_call);
    let (_, fresh_abort) = guard(fresh_call);
    assert_eq!(
        drop_ids(block(probe, fresh_abort)),
        vec![fresh],
        "returned owner must be covered before borrow-root registration"
    );

    let calc_call = call_block(probe, function(&program, "Calc").id);
    let (_, calc_abort) = guard(calc_call);
    assert_eq!(
        drop_ids(block(probe, calc_abort)),
        vec![fresh],
        "later argument failure abandons the earlier fresh root"
    );

    let use_call = call_block(probe, function(&program, "Use").id);
    let result = output(use_call);
    let (use_success, use_abort) = guard(use_call);
    assert_eq!(
        drop_ids(block(probe, use_abort)),
        vec![result, fresh],
        "call-completion guard must see both new result and current borrowed arguments"
    );
    assert_eq!(
        drop_ids(block(probe, use_success)),
        vec![fresh],
        "successful continuation keeps only the returned owner"
    );

    let optimized = ir::optimize_program(&program);
    ir::verify_borrow_contract(&optimized).unwrap();
    assert!(optimized.to_string().contains("sync_guard continue block"));
}

#[test]
fn native_sync_guard_short_circuit_does_not_eagerly_visit_rhs_call() {
    let program = lower(
        r#"
        fn Right(n: int): bool { return n > 0 }
        fn Choose(flag: bool, n: int): bool { return flag && Right(n) }
        fn main(): null! { println(Choose(false, 1)) return ok(null) }
    "#,
    );
    let choose = function(&program, "Choose");
    let rhs = call_block(choose, function(&program, "Right").id);
    guard(rhs);
    let branch = choose
        .blocks
        .iter()
        .find_map(|block| match &block.terminator {
            IrTerminator::Branch {
                condition,
                then_block,
                else_block,
            } if matches!(&condition.kind, IrExprKind::Local(name) if name == "flag") => {
                Some((*then_block, *else_block))
            }
            _ => None,
        })
        .unwrap();
    assert!(reachable(choose, branch.0, rhs.id));
    assert!(!reachable(choose, branch.1, rhs.id));
}

#[test]
fn native_sync_guard_indirect_map_and_lifted_return_types_are_preserved() {
    let program = lower(
        r#"
        fn Add(n: int): int { return n + 1 }
        fn Apply(&op: fn(int): int, n: int): int { return op(n) }
        fn main(): null! {
            op: fn(int): int = Add
            println(Apply(op, 1))
            values = [1, 2].map(x => {
                try { if (x < 0) { return 9 } }
                finally { println(x + 1) }
                return x + 2
            })
            println(values[0])
            return ok(null)
        }
    "#,
    );
    let mut indirect = 0;
    let mut map = 0;
    for function in &program.functions {
        for current in &function.blocks {
            for (index, inst) in current.instructions.iter().enumerate() {
                let Some(expr) = inst_expr(inst) else {
                    continue;
                };
                let IrExprKind::Call { kind, .. } = &expr.kind else {
                    continue;
                };
                if matches!(kind, IrCallKind::Indirect) {
                    indirect += 1;
                } else if matches!(kind, IrCallKind::Intrinsic(name) if name == "array.map") {
                    map += 1;
                } else {
                    continue;
                }
                assert_eq!(index + 1, current.instructions.len());
                guard(current);
            }
            if function.is_closure_body {
                if let IrTerminator::Return(Some(value)) = &current.terminator {
                    assert_eq!(
                        value.ty, function.return_type,
                        "synthetic cleanup zero must be resolved"
                    );
                }
            }
        }
    }
    assert_eq!(indirect, 1);
    assert_eq!(map, 1);
    assert!(program
        .functions
        .iter()
        .any(|function| function.is_closure_body));
}

#[test]
fn native_sync_guard_optimizer_and_llvm_preserve_both_structural_edges() {
    let program = lower(
        r#"
        fn Sum(a: int, b: int): int { return a + b }
        fn main(): int { return Sum(1, 2) }
    "#,
    );
    let optimized = ir::optimize_program(&program);
    let mut count = 0;
    for function in &optimized.functions {
        let ids = function
            .blocks
            .iter()
            .map(|block| block.id)
            .collect::<HashSet<_>>();
        for block in &function.blocks {
            for target in edges(block) {
                assert!(ids.contains(&target));
            }
            if matches!(block.terminator, IrTerminator::SyncGuard { .. }) {
                count += 1;
            }
        }
    }
    assert_eq!(count, 2);
    // Text-only validation: LLVM remains outside the checked-C safety claim.
    let llvm = llvm::generate_llvm_ir(&optimized).unwrap();
    assert!(llvm.matches("br i1 false").count() >= count);

    // The false cleanup edge contains C-owned drops even when the LLVM
    // successful path only returns a string pointer or a by-value Result.
    for source in [
        "fn Text(): str { return \"text\" } fn main(): str { return Text() }",
        "fn Text(): str { return \"<native-zero>\" } fn main(): str { return Text() }",
        "struct User { id: int } fn Load(): User! { return ok(User { id: 7 }) } fn main(): User! { value = Load()? return ok(value) }",
    ] {
        let program = ir::optimize_program(&lower(source));
        llvm::generate_llvm_ir(&program)
            .unwrap_or_else(|error| panic!("{error}\nsource: {source}"));
    }
}

#[test]
fn native_sync_guard_llvm_cleanup_placeholder_rejects_nonmaterialized_or_unsupported_inputs() {
    let reference = lower("fn Text(): str { return \"text\" } fn main() { println(Text()) }");
    for variant in 0..7 {
        let mut program = reference.clone();
        let value = program
            .functions
            .iter_mut()
            .flat_map(|function| &mut function.blocks)
            .flat_map(|block| &mut block.instructions)
            .find_map(|inst| match inst {
                IrInst::Expr(value)
                    if matches!(&value.kind,
                    IrExprKind::Call { kind: IrCallKind::Intrinsic(name), .. }
                        if name == "__ku_drop_borrow_temp") =>
                {
                    Some(value)
                }
                _ => None,
            })
            .unwrap();
        let IrExprKind::Call { args, .. } = &mut value.kind else {
            unreachable!()
        };
        let original = args[0].clone();
        match variant {
            0 => args.clear(),
            1 => args.push(original),
            2 => args[0].kind = IrExprKind::Literal("\"not-a-temp\"".into()),
            3 => {
                let IrExprKind::Temp(id) = original.kind else {
                    unreachable!()
                };
                args[0].kind = IrExprKind::BorrowedTemp(id);
            }
            4 => args[0].ty = IrType::Array(Box::new(IrType::Int)),
            5 => value.ty = IrType::Int,
            6 => {
                args[0].kind = IrExprKind::Call {
                    callee: Box::new(IrExpr {
                        kind: IrExprKind::Local("Text".into()),
                        ty: IrType::Function,
                    }),
                    args: vec![],
                    kind: IrCallKind::Direct(FunctionId(0)),
                }
            }
            _ => unreachable!(),
        }
        assert!(
            llvm::generate_llvm_ir(&program).is_err(),
            "invalid cleanup variant {variant}"
        );
    }
}
