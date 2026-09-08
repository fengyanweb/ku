//! Candidate only until normal Scope and frontend-depth gates are applied.
use ku::{
    ast::{Expr, ExprKind, FnDecl, Item, Literal, Program, Stmt, TypeName},
    checker::Checker,
    ir::{
        task::{self, SlotId, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        task_lower, IrType,
    },
    lexer::Lexer,
    parser::Parser,
    span::Span,
};

fn parsed(source: &str) -> Program {
    Parser::new(Lexer::new(source).lex().expect("lex"))
        .parse_program()
        .expect("parse")
}

fn checked(source: &str) -> Program {
    let ast = parsed(source);
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    ast
}

#[test]
fn native_task_copy_assignment_self_reads_and_preserves_ir_alias_rejection() {
    let native = task_lower::lower_program(&checked(
        "async fn main(): null! { number = 1 flag = true unit = null number = number flag = flag unit = unit number = 7 flag = false unit = null println(number) println(flag) println(unit) return ok(null) }",
    ))
    .expect("Copy locals can be reassigned without new bindings");
    let main = &native.tasks.functions[native.entry.0];
    let locals: Vec<_> = main.states[main.entry.0]
        .operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::Copy { dst, .. } => Some(*dst),
            _ => None,
        })
        .take(3)
        .collect();
    assert_eq!(locals.len(), 3);
    for local in &locals {
        assert!(main
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .any(|op| matches!(op, TaskOp::Read { slot } if slot == local)));
    }
    assert!(main
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .all(|op| !matches!(op, TaskOp::Copy { dst, src } if dst == src)));
    task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();

    let mut aliased = native.tasks.clone();
    aliased.functions[native.entry.0].states[main.entry.0]
        .operations
        .push(TaskOp::Copy {
            dst: locals[0],
            src: locals[0],
        });
    let error = task::verify_and_plan(&aliased, TaskLimits::default()).unwrap_err();
    assert!(
        error
            .message
            .contains("copy or move has invalid source/destination types"),
        "{error}"
    );

    let mut uninitialized = native.tasks.clone();
    let operations = &mut uninitialized.functions[native.entry.0].states[main.entry.0].operations;
    let first_binding = operations
        .iter()
        .position(|op| matches!(op, TaskOp::Copy { dst, .. } if *dst == locals[0]))
        .unwrap();
    operations.remove(first_binding);
    let error = task::verify_and_plan(&uninitialized, TaskLimits::default()).unwrap_err();
    assert!(
        error.message.contains("not definitely initialized"),
        "{error}"
    );
}

#[test]
fn native_task_copy_assignment_rhs_await_commits_only_on_success() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn Broken(value: int): int! { println(value) fail "assignment rhs failed" }
async fn Parent(base: int): int! {
    current = base
    current = current + (await Broken(current))?
    return ok(current)
}
async fn main(): null! { return ok(null) }
"#,
    ))
    .expect("the checker accepts the assignment even when its RHS can fail");
    let parent = native
        .tasks
        .functions
        .iter()
        .find(|f| f.name == "Parent")
        .unwrap();
    let current = parent.states[parent.entry.0]
        .operations
        .iter()
        .find_map(|op| match op {
            TaskOp::Copy { dst, src } if *src == parent.parameters[0] => Some(*dst),
            _ => None,
        })
        .unwrap();
    let (ok, err) = parent
        .states
        .iter()
        .find_map(|state| match state.terminator {
            TaskTerminator::TryResult { ok, err, .. } => Some((ok, err)),
            _ => None,
        })
        .unwrap();
    let (sum_index, sum, frozen_lhs) = parent.states[ok.0]
        .operations
        .iter()
        .enumerate()
        .find_map(|(index, op)| match op {
            TaskOp::Binary { dst, left, .. } => Some((index, *dst, *left)),
            _ => None,
        })
        .unwrap();
    assert_ne!(frozen_lhs, current);
    assert!(parent.states[parent.entry.0].operations.iter().any(
        |op| matches!(op, TaskOp::Copy { dst, src } if *dst == frozen_lhs && *src == current)
    ));
    let writes: Vec<_> = parent
        .states
        .iter()
        .enumerate()
        .flat_map(|(state, body)| {
            body.operations
                .iter()
                .enumerate()
                .filter_map(move |(index, op)| match op {
                    TaskOp::Copy { dst, src } if *dst == current => Some((state, index, *src)),
                    _ => None,
                })
        })
        .collect();
    assert_eq!(writes.len(), 2, "initial binding and one successful commit");
    assert_eq!(
        (writes[0].0, writes[0].2),
        (parent.entry.0, parent.parameters[0])
    );
    assert_eq!((writes[1].0, writes[1].2), (ok.0, sum));
    assert!(writes[1].1 > sum_index);
    assert!(parent.states[err.0].operations.is_empty());
    assert!(matches!(
        parent.states[err.0].terminator,
        TaskTerminator::Exit { .. }
    ));
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    assert!(plan.functions[parent.id.0].slots.contains(&frozen_lhs));
}

#[test]
fn native_task_copy_assignment_updates_outer_binding_but_not_a_shadow() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn Probe(gate: bool): int! {
    value = 10
    if (gate) { value = value + 1 } else { value = value + 2 }
    if (gate) { value: int = value + 100 value = value + 1 println(value) }
    return ok(value)
}
async fn main(): null! { return ok(null) }
"#,
    ))
    .unwrap();
    let probe = native
        .tasks
        .functions
        .iter()
        .find(|f| f.name == "Probe")
        .unwrap();
    let outer = probe.states[probe.entry.0]
        .operations
        .iter()
        .find_map(|op| match op {
            TaskOp::Copy { dst, .. } => Some(*dst),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        probe
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, TaskOp::Copy { dst, .. } if *dst == outer))
            .count(),
        3,
        "one initial binding and both first If arms; shadow writes a different slot"
    );
    assert!(probe
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .any(|op| matches!(op, TaskOp::WrapOk { src, .. } if *src == outer)));
    task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
}

#[test]
fn native_task_copy_assignment_raw_type_owned_task_and_declaration_gates_remain() {
    let mismatched = parsed("async fn main(): null! { value = 1 value = true return ok(null) }");
    assert!(Checker::new().check(&mismatched).is_err());
    let error = task_lower::lower_program(&mismatched).unwrap_err();
    assert!(
        error.message.contains("a mismatched Copy reassignment"),
        "{error}"
    );
    for source in [
        "async fn main(): null! { value = \"first\" value = \"second\" return ok(null) }",
        "async fn main(): null! { value = ok(1) value = ok(2) return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { child = Child() child = Child() return ok(null) }",
    ] {
        let error = task_lower::lower_program(&checked(source)).unwrap_err();
        assert!(error.message.contains("reassignment of Owned or Task values"), "{error}");
    }
    let duplicate =
        parsed("async fn main(): null! { value: int = 1 value: int = 2 return ok(null) }");
    assert!(Checker::new().check(&duplicate).is_err());
    let error = task_lower::lower_program(&duplicate).unwrap_err();
    assert!(
        error.message.contains("duplicate local declarations"),
        "{error}"
    );
    for source in [
        "async fn main(): null! { LIMIT = 1 LIMIT = 2 return ok(null) }",
        "async fn Parent(value: int): null! { value = 2 return ok(null) } async fn main(): null! { return ok(null) }",
    ] {
        let error = Checker::new().check(&parsed(source)).unwrap_err();
        assert!(error.message.contains("cannot assign to immutable variable"), "{error}");
    }
    for body in [
        "value = 1 value += 1",
        "value = 1 value++",
        "for item in 0 {}",
    ] {
        let source = format!("async fn main(): null! {{ {body} return ok(null) }}");
        let error = task_lower::lower_program(&checked(&source)).unwrap_err();
        assert!(error.message.contains("this statement"), "{error}");
    }
}

#[test]
fn native_task_while_latch_follows_scope_value_cleanup_and_saves_copy_state() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn main(): null! {
    index = 0
    outer = "kept"
    while (index < 3) {
        text = "body"
        unused = ok(index)
        println(text)
        index = index + 1
    }
    println(outer)
    println(index)
    return ok(null)
}
"#,
    ))
    .expect("a bounded Copy loop reuses its lexical body");
    let main = &native.tasks.functions[native.entry.0];
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let frame = &plan.functions[native.entry.0];
    assert_eq!(frame.scopes.len(), 1);
    let index = main.states[main.entry.0]
        .operations
        .iter()
        .find_map(|op| match op {
            TaskOp::Copy { dst, .. } => Some(*dst),
            _ => None,
        })
        .unwrap();
    assert!(
        frame.slots.contains(&index),
        "loop-carried Copy must survive Suspend"
    );
    let body = main
        .states
        .iter()
        .find(|state| {
            state
                .operations
                .iter()
                .any(|op| matches!(op, TaskOp::ScopeEnter { .. }))
        })
        .unwrap();
    let body_owned: Vec<_> = body
        .operations
        .iter()
        .filter_map(|op| {
            let dst = match op {
                TaskOp::Init { dst, .. }
                | TaskOp::Move { dst, .. }
                | TaskOp::WrapOk { dst, .. } => *dst,
                _ => return None,
            };
            matches!(
                &main.slots[dst.0].ty,
                TaskSlotType::Value {
                    ty: IrType::Str | IrType::Result(_),
                    ..
                }
            )
            .then_some(dst)
        })
        .collect();
    assert_eq!(
        body_owned.len(),
        4,
        "literal/Result temporaries and both bindings"
    );
    let (ready, scope_cleanup) = match body.terminator {
        TaskTerminator::ScopeDrain { ready, cleanup, .. } => (ready, cleanup),
        ref other => panic!("body must request scope drain, got {other:?}"),
    };
    let (header, latch_cleanup) = match main.states[ready.0].terminator {
        TaskTerminator::Suspend { resume, cleanup } => (resume, cleanup),
        ref other => panic!("post-drain endpoint must force Suspend, got {other:?}"),
    };
    assert_ne!(header, main.entry, "preheader must not repeat");
    assert_ne!(scope_cleanup, latch_cleanup);
    let expected_drops: Vec<_> = body_owned
        .iter()
        .rev()
        .map(|slot| TaskOp::DropIfInit { slot: *slot })
        .collect();
    assert_eq!(main.states[ready.0].operations, expected_drops);
    assert!(
        matches!(main.states[main.entry.0].terminator, TaskTerminator::Jump { target } if target == header)
    );
    assert!(main.states[header.0].operations.iter().any(|op| matches!(
        op,
        TaskOp::Binary {
            op: task::TaskBinaryOp::Less,
            ..
        }
    )));
    assert!(
        main.states[header.0].operations.iter().all(|op| !matches!(
            op, TaskOp::Init { dst, .. } | TaskOp::Copy { dst, .. } if *dst == index
        )),
        "header reads the updated binding rather than resetting it"
    );
    let all_owned: Vec<_> = main
        .slots
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, slot)| {
            matches!(
                &slot.ty,
                TaskSlotType::Value {
                    ty: IrType::Str | IrType::Result(_),
                    ..
                }
            )
            .then_some(TaskOp::DropIfInit {
                slot: SlotId(index),
            })
        })
        .collect();
    for cleanup in [scope_cleanup, latch_cleanup] {
        assert_eq!(main.states[cleanup.0].operations, all_owned);
        assert_eq!(main.states[cleanup.0].terminator, TaskTerminator::Terminate);
    }
    let mut no_yield = native.tasks.clone();
    no_yield.functions[native.entry.0].states[ready.0].terminator =
        TaskTerminator::Jump { target: header };
    let error = task::verify_and_plan(&no_yield, TaskLimits::default()).unwrap_err();
    assert!(error.message.contains("cycle"), "{error}");
}

#[test]
fn native_task_while_condition_reenters_before_short_circuit_await_and_try() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn Text(): str! { return ok("probe") }
async fn Flag(index: int, text: str): bool! { println(text) return ok(index < 2) }
async fn main(): null! {
    index = 0
    while ((index < 2) && (await Flag(index, (await Text())?))?) {
        index = index + 1
    }
    return ok(null)
}
"#,
    ))
    .expect("condition temporaries are consumed before the next evaluation");
    let main = &native.tasks.functions[native.entry.0];
    let headers: Vec<_> = main
        .states
        .iter()
        .filter_map(|state| match state.terminator {
            TaskTerminator::Suspend { resume, .. } => Some(resume),
            _ => None,
        })
        .collect();
    assert_eq!(headers.len(), 1);
    let header = headers[0];
    assert_ne!(header, main.entry);
    assert!(main.states[header.0].operations.iter().any(|op| matches!(
        op,
        TaskOp::Binary {
            op: task::TaskBinaryOp::Less,
            ..
        }
    )));
    let awaits: Vec<_> = main
        .states
        .iter()
        .filter_map(|state| match state.terminator {
            TaskTerminator::Await { dst, ready, .. } => {
                assert!(
                    state.operations.is_empty(),
                    "Pending must not replay Start/operands"
                );
                Some((dst, ready))
            }
            _ => None,
        })
        .collect();
    assert_eq!(awaits.len(), 2);
    for (dst, ready) in awaits {
        assert_ne!(header, ready, "backedge cannot skip a condition Await");
        assert!(
            main.states.iter().any(|state| matches!(state.terminator,
                TaskTerminator::TryResult { src, .. } if src == dst
            )),
            "each successful condition Result is consumed"
        );
    }
    assert_eq!(
        main.states
            .iter()
            .filter(|state| matches!(state.terminator, TaskTerminator::Branch { .. }))
            .count(),
        2
    );
    assert_eq!(
        main.states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, TaskOp::Start { .. }))
            .count(),
        2
    );
    task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
}

#[test]
fn native_task_while_empty_false_and_no_backedge_owned_consumption_lower() {
    let empty = task_lower::lower_program(&checked(
        "async fn main(): null! { while (false) {} return ok(null) }",
    ))
    .unwrap();
    let function = &empty.tasks.functions[empty.entry.0];
    assert!(function
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .all(|op| !matches!(op, TaskOp::ScopeEnter { .. })));
    assert_eq!(
        function
            .states
            .iter()
            .filter(|state| matches!(state.terminator, TaskTerminator::Suspend { .. }))
            .count(),
        1,
        "even the statically empty true edge must force yield"
    );
    for source in [
        "async fn main(): null! { result = ok(true) while (result?) { return ok(null) } return ok(null) }",
        "async fn Flag(): bool! { return ok(true) } async fn main(): null! { pending = Flag() while ((await pending)?) { return ok(null) } return ok(null) }",
    ] {
        let native = task_lower::lower_program(&checked(source)).expect("no backedge means one legal consume");
        let main = &native.tasks.functions[native.entry.0];
        assert!(main.states.iter().all(|state| !matches!(state.terminator, TaskTerminator::Suspend { .. })),
            "an exiting body must not acquire a synthetic backedge");
        task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    }
}

#[test]
fn native_task_while_raw_loop_carried_consumes_and_bad_condition_reject() {
    // Bypass the ordinary checker deliberately: lowering/IR must independently
    // reject second-iteration consumption, even if frontend checks regress.
    for source in [
        "async fn main(): null! { result = ok(true) while (result?) {} return ok(null) }",
        "async fn Flag(): bool! { return ok(true) } async fn main(): null! { pending = Flag() while ((await pending)?) {} return ok(null) }",
    ] {
        let error = task_lower::lower_program(&parsed(source)).unwrap_err();
        assert!(error.message.contains("not definitely initialized"), "{error}");
    }
    let error = task_lower::lower_program(&parsed(
        "async fn main(): null! { while (1) {} return ok(null) }",
    ))
    .unwrap_err();
    assert!(
        error.message.contains("non-bool while conditions"),
        "{error}"
    );
}

#[test]
fn native_task_while_raw_depth_and_flat_state_limits_are_unchanged() {
    let nested = |depth| {
        let mut ast = raw_nested(0);
        let Item::Function(function) = &mut ast.items[0] else {
            unreachable!()
        };
        let mut body = Vec::new();
        for _ in 0..depth {
            body = vec![Stmt::While {
                condition: Expr::new(ExprKind::Literal(Literal::Bool(false)), Span::default()),
                body,
                span: Span::default(),
            }];
        }
        body.append(&mut function.body);
        function.body = body;
        ast
    };
    task_lower::lower_program(&nested(32)).expect("root does not consume a statement level");
    let error = task_lower::lower_program(&nested(33)).unwrap_err();
    assert!(
        error
            .message
            .contains("statement nesting beyond 32 Task levels"),
        "{error}"
    );
    let mut ast = checked("async fn Probe(gate: bool): null! { return ok(null) } async fn main(): null! { return ok(null) }");
    let Item::Function(function) = &mut ast.items[0] else {
        unreachable!()
    };
    let last = function.body.pop().unwrap();
    for _ in 0..64 {
        function.body.push(Stmt::While {
            condition: Expr::new(ExprKind::Variable("gate".into()), Span::default()),
            body: Vec::new(),
            span: Span::default(),
        });
    }
    function.body.push(last);
    let error = task_lower::lower_program(&ast).unwrap_err();
    assert!(
        error
            .message
            .contains("more than 256 generated Task states"),
        "{error}"
    );
}

#[test]
fn native_task_if_lexical_names_and_terminating_move_paths_lower() {
    for body in [
        "if (true) { println(1) } else { println(2) } return ok(null)",
        "if (false) { println(1) } return ok(null)",
        "if (true) { if (false) { println(1) } else { println(2) } } else if (true) { println(3) } return ok(null)",
        "if (true) { return ok(null) } else { fail \"other\" }",
        "outer = \"kept\" if (true) { moved = outer return ok(null) } else { println(outer) } println(outer) return ok(null)",
        "value: int = 1 if (true) { value: int = 2 println(value) } else { value: int = 3 println(value) } println(value) return ok(null)",
    ] {
        let native = task_lower::lower_program(&checked(&format!("async fn main(): null! {{ {body} }}"))).unwrap();
        let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
        assert!(!plan.functions[0].scopes.is_empty());
        assert!(native.tasks.functions[0].states.iter().all(|state| !matches!(state.terminator, TaskTerminator::Complete { .. })));
    }
}

#[test]
fn native_task_if_condition_uses_real_await_and_short_circuit_endpoints() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn Flag(value: bool): bool! { return ok(value) }
async fn main(): null! {
    if ((await Flag(true))? && (await Flag(false))?) { println(1) } else { println(2) }
    return ok(null)
}
"#,
    ))
    .unwrap();
    let main = &native.tasks.functions[native.entry.0];
    let await_states: Vec<_> = main
        .states
        .iter()
        .filter(|state| matches!(state.terminator, TaskTerminator::Await { .. }))
        .collect();
    assert_eq!(await_states.len(), 2);
    assert!(await_states.iter().all(|state| state.operations.is_empty()));
    assert_eq!(
        main.states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, TaskOp::Start { .. }))
            .count(),
        2
    );
    assert_eq!(
        main.states
            .iter()
            .filter(|state| matches!(state.terminator, TaskTerminator::TryResult { .. }))
            .count(),
        2
    );
    assert_eq!(
        main.states
            .iter()
            .filter(|state| matches!(state.terminator, TaskTerminator::Branch { .. }))
            .count(),
        2
    );
}

#[test]
fn native_task_if_outer_await_keeps_original_owner_and_inner_masks_disjoint() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    outer = Child(1)
    retained = Child(2)
    if (true) {
        value = (await outer)?
        inner = Child(value)
        println(value)
    }
    return ok(null)
}
"#,
    ))
    .unwrap();
    let main = &native.tasks.functions[native.entry.0];
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let frame = &plan.functions[native.entry.0];
    assert_eq!(frame.scopes.len(), 1);
    let mask = frame.scopes[0].task_mask;
    assert_eq!(
        mask.count_ones(),
        2,
        "Start temporary and inner binding only"
    );
    let awaited = main
        .states
        .iter()
        .find_map(|state| match state.terminator {
            TaskTerminator::Await { task, .. } => Some(task),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        mask & (1u64 << awaited.0),
        0,
        "outer Await must not transfer duty into the arm"
    );
    let moves_to_awaited = main
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter(|op| matches!(op, TaskOp::Move { dst, .. } if *dst == awaited))
        .count();
    assert_eq!(
        moves_to_awaited, 1,
        "only the original ROOT binding Move, no hidden cross-scope Move"
    );
}

#[test]
fn native_task_if_nested_task_birth_scopes_form_disjoint_masks() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    root = Child(1)
    if (true) {
        outer = Child(2)
        if (true) { inner = Child(3) }
    }
    return ok(null)
}
"#,
    ))
    .unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let frame = &plan.functions[native.entry.0];
    assert_eq!(frame.scopes.len(), 2);
    let outer = frame.scopes[0].task_mask;
    let inner = frame.scopes[1].task_mask;
    assert_eq!(outer.count_ones(), 2);
    assert_eq!(inner.count_ones(), 2);
    assert_eq!(outer & inner, 0);
    let main = &native.tasks.functions[native.entry.0];
    let all = main
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot.ty, TaskSlotType::Task { .. }))
        .fold(0u64, |mask, (index, _)| mask | (1u64 << index));
    assert_eq!(
        (all & !(outer | inner)).count_ones(),
        2,
        "ROOT Start and binding stay outside both scopes"
    );
    let drains: Vec<_> = main
        .states
        .iter()
        .filter_map(|state| match state.terminator {
            TaskTerminator::ScopeDrain { scope, .. } => Some(scope),
            _ => None,
        })
        .collect();
    assert_eq!(drains.len(), 2);
    assert!(drains.contains(&frame.scopes[0].scope) && drains.contains(&frame.scopes[1].scope));
}

#[test]
fn native_task_if_ready_drops_only_local_values_and_cleanup_drops_all_values() {
    let native = task_lower::lower_program(&checked(
        r#"
async fn main(): null! {
    outer = "kept"
    if (true) { local = "arm" wrapped = ok(local) println(outer) }
    println(outer)
    return ok(null)
}
"#,
    ))
    .unwrap();
    let main = &native.tasks.functions[0];
    let entry = &main.states[main.entry.0];
    let outer_values: Vec<_> = entry
        .operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::Init { dst, .. } | TaskOp::Move { dst, .. }
                if matches!(
                    &main.slots[dst.0].ty,
                    TaskSlotType::Value {
                        ty: IrType::Str,
                        ..
                    }
                ) =>
            {
                Some(*dst)
            }
            _ => None,
        })
        .collect();
    assert_eq!(outer_values.len(), 2);
    let all_owned: Vec<_> = main
        .slots
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, slot)| {
            matches!(
                &slot.ty,
                TaskSlotType::Value {
                    ty: IrType::Str | IrType::Result(_),
                    ..
                }
            )
            .then_some(TaskOp::DropIfInit {
                slot: SlotId(index),
            })
        })
        .collect();
    let (ready, cleanup) = main
        .states
        .iter()
        .find_map(|state| match state.terminator {
            TaskTerminator::ScopeDrain { ready, cleanup, .. } => Some((ready, cleanup)),
            _ => None,
        })
        .unwrap();
    let drops: Vec<_> = main.states[ready.0]
        .operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::DropIfInit { slot } => Some(*slot),
            _ => None,
        })
        .collect();
    assert!(!drops.is_empty());
    assert!(drops.windows(2).all(|pair| pair[0].0 > pair[1].0));
    assert!(outer_values.iter().all(|slot| !drops.contains(slot)));
    assert_eq!(main.states[cleanup.0].operations, all_owned);
    assert_eq!(main.states[cleanup.0].terminator, TaskTerminator::Terminate);
}

#[test]
fn native_task_if_checker_still_rejects_escape_and_falling_path_moves() {
    for body in [
        "if (true) { inner = 1 } println(inner) return ok(null)",
        "outer = \"value\" if (true) { moved = outer } println(outer) return ok(null)",
        "if (true) { return ok(null) } else { println(missing) } return ok(null)",
    ] {
        let ast = parsed(&format!("async fn main(): null! {{ {body} }}"));
        assert!(Checker::new().check(&ast).is_err(), "{body}");
    }
}

#[test]
fn native_task_if_ancestor_await_may_join_is_rejected_without_checker() {
    let source = r#"
async fn Child(): int! { return ok(1) }
async fn Parent(gate: bool): null! {
    outer = Child()
    if (gate) { value = (await outer)? println(value) }
    again = (await outer)?
    println(again)
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#;
    let ast = parsed(source);
    let checked_error = Checker::new()
        .check(&ast)
        .expect_err("one falling arm consumed outer");
    assert!(
        checked_error.message.contains("moved")
            || checked_error.message.contains("already been awaited"),
        "{checked_error}"
    );
    // Do not call checked(): the lowerer must independently reject the raw AST.
    // Its post-join hidden Move may detect this before the later Await itself.
    let raw_error = task_lower::lower_program(&ast).expect_err("MAY is not definitely initialized");
    assert!(
        raw_error.message.contains("not definitely initialized"),
        "{raw_error}"
    );
}

#[test]
fn native_task_if_ancestor_await_both_arms_and_terminating_arm_keep_ownership() {
    for (body, same_await_slot) in [
        (
            "outer = Child() if (gate) { value = (await outer)? println(value) } else { value = (await outer)? println(value) } return ok(null)",
            true,
        ),
        (
            "outer = Child() if (gate) { value = (await outer)? println(value) return ok(null) } value = (await outer)? println(value) return ok(null)",
            false,
        ),
    ] {
        let source = format!(
            "async fn Child(): int! {{ return ok(1) }} async fn Parent(gate: bool): null! {{ {body} }} async fn main(): null! {{ return ok(null) }}"
        );
        let native = task_lower::lower_program(&checked(&source)).expect("valid ownership paths");
        let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
        let parent = native.tasks.functions.iter().find(|f| f.name == "Parent").unwrap();
        let frame = plan.functions.iter().find(|f| f.function == parent.id).unwrap();
        let awaited: Vec<_> = parent.states.iter().filter_map(|state| match state.terminator {
            TaskTerminator::Await { task, .. } => Some(task),
            _ => None,
        }).collect();
        assert_eq!(awaited.len(), 2);
        assert_eq!(awaited[0] == awaited[1], same_await_slot);
        assert!(frame.scopes.iter().all(|scope| awaited.iter().all(|slot| scope.task_mask & (1u64 << slot.0) == 0)));
        for op in parent.states.iter().flat_map(|state| &state.operations) {
            if let TaskOp::Move { dst, src } = op {
                if matches!(parent.slots[src.0].ty, TaskSlotType::Task { .. }) {
                    let owner = |slot: SlotId| frame.scopes.iter().find(|scope| scope.task_mask & (1u64 << slot.0) != 0).map(|scope| scope.scope);
                    assert_eq!(owner(*src), owner(*dst), "hidden Task Move crossed scope");
                }
            }
        }
    }
}

#[test]
fn native_task_if_shadow_initializer_reads_parent_and_global_callee_returns() {
    let source = r#"
async fn Child(value: int): int! { return ok(value) }
async fn Parent(gate: bool): null! {
    value: int = 4
    if (gate) {
        value: int = value + 1
        println(value)
        Child: int = 99
        println(Child)
    }
    result = (await Child(value))?
    println(result)
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).expect("shadow scope must be popped");
    let child = native
        .tasks
        .functions
        .iter()
        .find(|f| f.name == "Child")
        .unwrap()
        .id;
    let parent = native
        .tasks
        .functions
        .iter()
        .find(|f| f.name == "Parent")
        .unwrap();
    let starts: Vec<_> = parent
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|op| match op {
            TaskOp::Start { function, .. } => Some(*function),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [child]);

    let source = "async fn Child(value: int): int! { return ok(value) } async fn main(): null! { if (true) { Child: int = 1 inner = Child(2) } return ok(null) }";
    let ast = parsed(source);
    assert!(
        Checker::new().check(&ast).is_err(),
        "primitive local cannot be called"
    );
    let error = task_lower::lower_program(&ast).expect_err("do not fall back to the global Child");
    assert!(
        error.message.contains("a call through a local value"),
        "{error}"
    );
}

#[test]
fn native_task_if_owned_reassignment_cross_scope_task_moves_and_for_stay_gated() {
    for source in [
        "async fn main(): null! { value = \"first\" if (true) { value = \"second\" } return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { outer = Child() if (true) { inner = outer } return ok(null) }",
        "async fn main(): null! { for item in 1 { println(item) } return ok(null) }",
    ] {
        let error = task_lower::lower_program(&checked(source)).expect_err("outside the admitted slice");
        assert!(error.to_string().contains("native async subset"), "{error}");
    }
}

#[test]
fn native_task_if_non_root_grandparent_await_both_arms_keep_the_original_slot() {
    let source = r#"
async fn Child(value: int): int! { return ok(value) }
async fn Grand(gate: bool): null! {
    if (true) {
        grand = Child(17)
        if (true) {
            if (gate) { value = (await grand)? println(value) }
            else { value = (await grand)? println(value + 1) }
        }
        println(19)
    }
    return ok(null)
}
async fn main(): null! {
    first = (await Grand(true))?
    second = (await Grand(false))?
    return ok(null)
}
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let grand = native
        .tasks
        .functions
        .iter()
        .find(|f| f.name == "Grand")
        .unwrap();
    let frame = &plan.functions[grand.id.0];
    assert_eq!(frame.scopes.len(), 4, "outer, middle, then and else scopes");
    assert_eq!(
        frame.scopes[0].task_mask.count_ones(),
        2,
        "Start and binding belong to non-ROOT outer scope"
    );
    assert!(frame.scopes[1..].iter().all(|scope| scope.task_mask == 0));
    let awaited: Vec<_> = grand
        .states
        .iter()
        .filter_map(|state| match state.terminator {
            TaskTerminator::Await { task, .. } => Some(task),
            _ => None,
        })
        .collect();
    assert_eq!(awaited.len(), 2);
    assert_eq!(
        awaited[0], awaited[1],
        "neither arm introduces a cross-scope hidden Move"
    );
    assert_ne!(frame.scopes[0].task_mask & (1u64 << awaited[0].0), 0);
    assert_eq!(
        grand
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, TaskOp::Move { dst, .. } if *dst == awaited[0]))
            .count(),
        1
    );
    assert_eq!(
        grand
            .states
            .iter()
            .filter(|state| matches!(state.terminator, TaskTerminator::ScopeDrain { .. }))
            .count(),
        4
    );
}

#[test]
fn native_task_if_ancestor_owned_return_survives_inner_normal_drain() {
    let source = r#"
async fn NestedText(gate: bool, value: str): str! {
    if (true) {
        held = value
        if (true) {
            if (gate) { return ok(held) }
            println(held)
        }
        return ok(held)
    }
    return ok(value)
}
async fn main(): null! {
    first = (await NestedText(true, "first"))?
    second = (await NestedText(false, "second"))?
    println(first)
    println(second)
    return ok(null)
}
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let text = native
        .tasks
        .functions
        .iter()
        .find(|f| f.name == "NestedText")
        .unwrap();
    let frame = &plan.functions[text.id.0];
    assert!(frame.exit_bridge && frame.hosted);
    assert_eq!(
        frame.scope_task_mask, 0,
        "Owned cleanup must not depend on having a Task slot"
    );
    assert_eq!(frame.scopes.len(), 3);
    let input = text.parameters[1];
    let held: Vec<_> = text
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|op| match op {
            TaskOp::Move { dst, src } if *src == input => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(held.len(), 1);
    let held = held[0];
    assert_eq!(
        text.states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, TaskOp::WrapOk { src, .. } if *src == held))
            .count(),
        2,
        "deep early Exit and later outer Exit consume the same owner on exclusive paths"
    );
    let drains: Vec<_> = text
        .states
        .iter()
        .filter_map(|state| match state.terminator {
            TaskTerminator::ScopeDrain { ready, .. } => Some(ready),
            _ => None,
        })
        .collect();
    assert_eq!(
        drains.len(),
        1,
        "only the middle scope falls through; other scopes Exit"
    );
    assert!(text.states[drains[0].0]
        .operations
        .iter()
        .all(|op| !matches!(op, TaskOp::DropIfInit { slot } if *slot == held || *slot == input)));
    for (index, slot) in text.slots.iter().enumerate() {
        if matches!(
            slot.ty,
            TaskSlotType::Value {
                ty: IrType::Str | IrType::Result(_),
                ..
            }
        ) {
            assert!(frame.slots.contains(&SlotId(index)));
        }
    }
}

#[test]
fn native_task_if_consumed_grandparent_is_not_restored_by_two_empty_drains() {
    for other_arm in ["value = (await grand)? println(value)", "println(0)"] {
        let source = r#"
async fn Child(): int! { return ok(17) }
async fn Grand(gate: bool): null! {
    if (true) {
        grand = Child()
        if (true) {
            if (gate) { value = (await grand)? println(value) }
            else { @OTHER_ARM@ }
        } else { value = (await grand)? println(value) }
        again = (await grand)?
        println(again)
    }
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#
        .replace("@OTHER_ARM@", other_arm);
        let ast = parsed(&source);
        let error = Checker::new()
            .check(&ast)
            .expect_err("BOTH or MAY consumption reaches the later Await");
        assert!(
            error
                .message
                .contains("task 'grand' has already been awaited"),
            "{error}"
        );
        let raw_error = task_lower::lower_program(&ast)
            .expect_err("raw AST cannot resurrect an ancestor owner");
        assert!(
            raw_error.message.contains("not definitely initialized"),
            "{raw_error}"
        );
    }
}

fn raw_nested(depth: usize) -> Program {
    let span = Span::default();
    let mut body = Vec::new();
    for _ in 0..depth {
        body = vec![Stmt::If {
            condition: Expr::new(ExprKind::Literal(Literal::Bool(true)), span),
            then_branch: body,
            else_branch: Vec::new(),
            span,
        }];
    }
    body.push(Stmt::Return {
        value: Some(Expr::new(
            ExprKind::Call {
                callee: Box::new(Expr::new(ExprKind::Variable("ok".into()), span)),
                args: vec![Expr::new(ExprKind::Literal(Literal::Null), span)],
            },
            span,
        )),
        span,
    });
    Program {
        items: vec![Item::Function(FnDecl {
            name: "main".into(),
            is_async: true,
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: Some(TypeName::Result(Box::new(TypeName::Null))),
            body,
            span,
        })],
    }
}

#[test]
fn native_task_if_raw_lowerer_depth_is_independent_of_frontend_checks() {
    task_lower::lower_program(&raw_nested(32)).expect("root does not consume a statement level");
    let error = task_lower::lower_program(&raw_nested(33)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("statement nesting beyond 32 Task levels"),
        "{error}"
    );
}

#[test]
fn native_task_if_raw_state_budget_rejects_flat_branch_growth() {
    let mut ast = checked("async fn Probe(gate: bool): null! { return ok(null) } async fn main(): null! { return ok(null) }");
    let Item::Function(function) = &mut ast.items[0] else {
        unreachable!()
    };
    let last = function.body.pop().unwrap();
    for _ in 0..86 {
        function.body.push(Stmt::If {
            condition: Expr::new(ExprKind::Variable("gate".into()), Span::default()),
            then_branch: Vec::new(),
            else_branch: Vec::new(),
            span: Span::default(),
        });
    }
    function.body.push(last);
    let error = task_lower::lower_program(&ast).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("more than 256 generated Task states"),
        "{error}"
    );
}
