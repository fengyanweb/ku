//! Internal scope verifier tests only; generated C/source integration is separate.
use ku::ir::{task::*, IrType};

fn result() -> IrType {
    IrType::Result(Box::new(IrType::Null))
}
fn value(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}
fn task() -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Task { result: result() },
    }
}
fn state(operations: Vec<TaskOp>, terminator: TaskTerminator) -> TaskState {
    TaskState {
        operations,
        terminator,
    }
}
fn enter(id: usize, tasks: &[usize]) -> TaskOp {
    TaskOp::ScopeEnter {
        scope: TaskScopeId(id),
        tasks: tasks.iter().copied().map(SlotId).collect(),
    }
}
fn drain(scope: usize, ready: usize, cleanup: usize) -> TaskTerminator {
    TaskTerminator::ScopeDrain {
        scope: TaskScopeId(scope),
        ready: StateId(ready),
        cleanup: StateId(cleanup),
    }
}
fn start(dst: usize) -> TaskOp {
    TaskOp::Start {
        dst: SlotId(dst),
        function: TaskFunctionId(9),
        arguments: vec![],
    }
}
fn moved(src: usize, dst: usize) -> TaskOp {
    TaskOp::Move {
        src: SlotId(src),
        dst: SlotId(dst),
    }
}
fn read(slot: usize) -> TaskOp {
    TaskOp::Read { slot: SlotId(slot) }
}
fn drop_if(slot: usize) -> TaskOp {
    TaskOp::DropIfInit { slot: SlotId(slot) }
}
fn init_int(dst: usize, number: i64) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value: TaskConstant::Int(number),
    }
}
fn init_result(dst: usize) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
    }
}
fn exit(value: usize) -> TaskTerminator {
    TaskTerminator::Exit {
        value: SlotId(value),
    }
}
fn jump(target: usize) -> TaskTerminator {
    TaskTerminator::Jump {
        target: StateId(target),
    }
}
fn branch(then_state: usize, else_state: usize) -> TaskTerminator {
    TaskTerminator::Branch {
        condition: SlotId(8),
        then_state: StateId(then_state),
        else_state: StateId(else_state),
    }
}
fn leaf() -> TaskFunction {
    TaskFunction {
        id: TaskFunctionId(9),
        name: "Leaf".into(),
        parameters: vec![],
        slots: vec![value(result())],
        entry: StateId(0),
        result: result(),
        states: vec![state(
            vec![init_result(0)],
            TaskTerminator::Complete { value: SlotId(0) },
        )],
    }
}
fn scoped() -> TaskProgram {
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "Scoped".into(),
                parameters: vec![SlotId(6), SlotId(8)],
                slots: vec![
                    value(result()),     // 0: final Result
                    task(),              // 1: ROOT owner
                    task(),              // 2: inner owner
                    task(),              // 3: inner spare / move destination
                    value(IrType::Int),  // 4: nonparameter Copy used after ACK
                    value(IrType::Int),  // 5: nonparameter Copy used by cleanup
                    value(IrType::Str),  // 6: outer Owned parameter
                    task(),              // 7: ROOT spare
                    value(IrType::Bool), // 8: condition
                    value(result()),     // 9: direct Await Result
                ],
                entry: StateId(0),
                result: result(),
                states: vec![
                    state(
                        vec![
                            start(1),
                            init_int(4, 7),
                            init_int(5, 11),
                            enter(0, &[2, 3]),
                            start(2),
                        ],
                        drain(0, 1, 2),
                    ),
                    state(vec![read(1), read(4), init_result(0)], exit(0)),
                    state(
                        vec![read(5), drop_if(0), drop_if(6), drop_if(9)],
                        TaskTerminator::Terminate,
                    ),
                ],
            },
            leaf(),
        ],
    }
}
fn accepted(program: &TaskProgram) -> TaskFramePlan {
    verify_and_plan(program, TaskLimits::default())
        .unwrap_or_else(|error| panic!("unexpected rejection: {error}\n{program:?}"))
}
fn rejected(program: &TaskProgram, message: &str) {
    let error = verify_and_plan(program, TaskLimits::default()).expect_err(message);
    assert!(
        error.message.contains(message),
        "expected {message:?}, got {error}"
    );
}

#[test]
fn native_task_scope_ir_drain_clears_only_inner_and_persists_copy_on_both_edges() {
    let program = scoped();
    let plan = accepted(&program);
    let frame = &plan.functions[0];
    assert!(frame.hosted && frame.exit_bridge);
    assert_eq!(frame.scope_task_mask, 2 | 4 | 8 | 128);
    assert_eq!(
        frame.scopes,
        vec![TaskScopeFrame {
            scope: TaskScopeId(0),
            task_mask: 12
        }]
    );
    assert_eq!(frame.suspensions.len(), 1);
    assert_eq!(frame.suspensions[0].state, StateId(0));
    for slot in [4, 5, 6] {
        assert!(
            frame.slots.contains(&SlotId(slot)),
            "frame lost slot {slot}"
        );
        assert!(frame.suspensions[0].slots.contains(&SlotId(slot)));
    }
    assert!(plan.functions[1].scopes.is_empty());
    assert!(!plan.functions[1].exit_bridge);
    assert_eq!(accepted(&program), plan);
    for slot in [2, 3] {
        let mut bad = program.clone();
        bad.functions[0].states[1].operations.insert(0, read(slot));
        rejected(&bad, "not definitely initialized");
    }
    for slot in [1, 2, 3, 7] {
        let mut bad = program.clone();
        bad.functions[0].states[2].operations.insert(0, read(slot));
        rejected(&bad, "not definitely initialized");
    }
}

#[test]
fn native_task_scope_ir_same_scope_move_and_direct_outer_await() {
    let mut moved_program = scoped();
    moved_program.functions[0].states[0]
        .operations
        .push(moved(2, 3));
    accepted(&moved_program);
    let mut awaited = scoped();
    // The real outer owner is consumed directly, not moved to an inner hidden slot.
    awaited.functions[0].states[0].terminator = TaskTerminator::Await {
        task: SlotId(1),
        dst: SlotId(9),
        ready: StateId(3),
        cleanup: StateId(2),
    };
    awaited.functions[0].states.push(state(
        vec![read(2), TaskOp::Drop { slot: SlotId(9) }],
        drain(0, 1, 2),
    ));
    awaited.functions[0].states[1].operations.remove(0);
    accepted(&awaited);
    awaited.functions[0].states[1].operations.insert(0, read(1));
    rejected(&awaited, "not definitely initialized");
}

#[test]
fn native_task_scope_ir_rejects_implicit_root_and_all_cross_scope_initialization() {
    for operation in [start(7), moved(1, 7), moved(2, 7)] {
        let mut bad = scoped();
        bad.functions[0].states[0].operations.push(operation);
        rejected(
            &bad,
            "Task destination must belong to the innermost active scope",
        );
    }
    let mut into_inner = scoped();
    into_inner.functions[0].states[0]
        .operations
        .push(moved(1, 3));
    rejected(&into_inner, "Task Move cannot cross ownership scopes");
    let mut before = scoped();
    before.functions[0].states[0].operations.insert(0, start(3));
    rejected(
        &before,
        "Task destination must belong to the innermost active scope",
    );
    let mut after = scoped();
    after.functions[0].states[1].operations.insert(0, start(3));
    rejected(
        &after,
        "Task destination must belong to the innermost active scope",
    );
}

#[test]
fn native_task_scope_ir_nesting_and_non_top_drain() {
    let mut program = scoped();
    program.functions[0].states[0]
        .operations
        .extend([enter(1, &[7]), start(7)]);
    program.functions[0].states[0].terminator = drain(1, 3, 2);
    program.functions[0]
        .states
        .push(state(vec![read(2)], drain(0, 1, 2)));
    let plan = accepted(&program);
    assert_eq!(
        plan.functions[0].scopes,
        vec![
            TaskScopeFrame {
                scope: TaskScopeId(0),
                task_mask: 12
            },
            TaskScopeFrame {
                scope: TaskScopeId(1),
                task_mask: 128
            },
        ]
    );
    assert_eq!(plan.functions[0].suspensions.len(), 2);
    let mut non_top = program.clone();
    non_top.functions[0].states[0].terminator = drain(0, 3, 2);
    rejected(&non_top, "ScopeDrain must close the innermost active scope");
    // Even a destination belonging to an active OUTER scope is not the top.
    program.functions[0].states[0].operations.push(start(3));
    rejected(
        &program,
        "Task destination must belong to the innermost active scope",
    );
}

fn scope_exit_program() -> TaskProgram {
    let mut program = scoped();
    program.functions[0].states[0].terminator = jump(3);
    program.functions[0].states.extend([
        state(
            vec![TaskOp::ScopeExitBegin {
                exit: TaskScopeExitId(0),
                retain: None,
            }],
            jump(4),
        ),
        state(vec![], drain(0, 5, 2)),
        state(
            vec![TaskOp::ScopeExitEnd {
                exit: TaskScopeExitId(0),
            }],
            jump(1),
        ),
    ]);
    program
}

#[test]
fn native_task_scope_exit_ir_map_and_legacy_empty_plan() {
    assert_eq!(
        accepted(&scoped()).functions[0].scope_exits,
        TaskScopeExitPlan::default()
    );
    let plan = accepted(&scope_exit_program());
    assert_eq!(
        plan.functions[0].scope_exits.operations,
        vec![TaskScopeExitFrame {
            exit: TaskScopeExitId(0),
            begin: StateId(3),
            end: StateId(5),
            retain: None,
        }]
    );
    assert_eq!(
        plan.functions[0].scope_exits.state_operations,
        vec![
            None,
            None,
            None,
            None,
            Some(TaskScopeExitId(0)),
            Some(TaskScopeExitId(0))
        ]
    );
}

#[test]
fn native_task_scope_exit_ir_nested_retain_is_real_ancestor() {
    let mut program = scope_exit_program();
    program.functions[0].states[0]
        .operations
        .extend([enter(1, &[7]), start(7)]);
    program.functions[0].states[4].terminator = drain(1, 6, 2);
    program.functions[0]
        .states
        .push(state(vec![], drain(0, 5, 2)));
    accepted(&program);
    // Keeping the outer lexical scope is legal; final Exit owns its cleanup.
    program.functions[0].states[3].operations[0] = TaskOp::ScopeExitBegin {
        exit: TaskScopeExitId(0),
        retain: Some(TaskScopeId(0)),
    };
    program.functions[0].states[6].terminator = jump(5);
    accepted(&program);
    program.functions[0].states[3].operations[0] = TaskOp::ScopeExitBegin {
        exit: TaskScopeExitId(0),
        retain: Some(TaskScopeId(1)),
    };
    rejected(&program, "retain must be a proper ancestor");
}

#[test]
fn native_task_scope_exit_ir_rejects_missing_sparse_and_mixed_markers() {
    let mut duplicate = scope_exit_program();
    duplicate.functions[0].states[5].terminator = jump(6);
    duplicate.functions[0].states.push(state(
        vec![TaskOp::ScopeExitEnd {
            exit: TaskScopeExitId(0),
        }],
        jump(1),
    ));
    rejected(&duplicate, "duplicate scope-exit marker");
    let mut missing = scope_exit_program();
    missing.functions[0].states[5].operations.clear();
    rejected(&missing, "missing scope-exit End");
    let mut sparse = scope_exit_program();
    sparse.functions[0].states[5].operations[0] = TaskOp::ScopeExitEnd {
        exit: TaskScopeExitId(99),
    };
    rejected(&sparse, "scope-exit ids must be dense");
    let mut mixed = scope_exit_program();
    mixed.functions[0].states[3].operations.push(read(4));
    rejected(&mixed, "dedicated normal Jump state");
    let mut wrong_retain = scope_exit_program();
    wrong_retain.functions[0].states[3].operations[0] = TaskOp::ScopeExitBegin {
        exit: TaskScopeExitId(0),
        retain: Some(TaskScopeId(99)),
    };
    rejected(&wrong_retain, "retain references an unknown scope");
}

#[test]
fn native_task_scope_exit_ir_rejects_entry_bypass_work_and_scope_mismatch() {
    let mut bypass = scope_exit_program();
    bypass.functions[0].states[0].terminator = branch(3, 4);
    rejected(&bypass, "crosses a scope-exit operation boundary");
    let mut work = scope_exit_program();
    work.functions[0].states[4].operations.push(start(3));
    rejected(&work, "permits only Value drops");
    let mut not_drained = scope_exit_program();
    not_drained.functions[0].states[4].terminator = jump(5);
    rejected(&not_drained, "End has not reached its retained scope");
    let mut not_suspended = scope_exit_program();
    not_suspended.functions[0].states[4].terminator = drain(0, 4, 2);
    rejected(&not_suspended, "cycle without suspension");
}

#[test]
fn native_task_scope_exit_ir_rejects_cleanup_marker_and_tight_budget() {
    let mut cleanup = scope_exit_program();
    cleanup.functions[0].states[2]
        .operations
        .push(TaskOp::ScopeExitEnd {
            exit: TaskScopeExitId(0),
        });
    rejected(
        &cleanup,
        "cleanup cannot enter a normal scope-exit operation",
    );
    let limits = TaskLimits {
        max_analysis_work: 1,
        ..TaskLimits::default()
    };
    assert!(verify_and_plan(&scope_exit_program(), limits)
        .unwrap_err()
        .message
        .contains("analysis work limit"));
}

#[test]
fn native_task_scope_exit_ir_does_not_enable_c_runtime() {
    let program = scope_exit_program();
    accepted(&program);
    let ast = ku::parser::Parser::new(
        ku::lexer::Lexer::new("fn main(): null! { return ok(null) }")
            .lex()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    let sync = ku::ir::lower_program(&ast).unwrap();
    let error = ku::backend::c::generate_task_frame_c_source(&sync, &program).unwrap_err();
    assert!(error
        .message
        .contains("scope-exit operation runtime is not implemented"));
}

fn sibling_branches() -> TaskProgram {
    let mut program = scoped();
    program.functions[0].states[0].operations.truncate(3);
    program.functions[0].states[0].terminator = branch(3, 4);
    program.functions[0]
        .states
        .push(state(vec![enter(0, &[2]), start(2)], drain(0, 1, 2)));
    program.functions[0]
        .states
        .push(state(vec![enter(1, &[3]), start(3)], drain(1, 1, 2)));
    program
}

#[test]
fn native_task_scope_ir_join_requires_identical_full_stack_but_cleanup_can_merge() {
    let program = sibling_branches();
    accepted(&program); // Both normal joins are ROOT; cleanup joins drop all Tasks.
    let mut bad = program.clone();
    bad.functions[0].states[3].terminator = jump(1);
    bad.functions[0].states[4].terminator = jump(1);
    rejected(&bad, "normal join has different active scope stacks");
    let mut one_missing = program;
    one_missing.functions[0].states[3].terminator = jump(1);
    rejected(
        &one_missing,
        "normal join has different active scope stacks",
    );
}

#[test]
fn native_task_scope_ir_one_scope_can_have_two_alternative_drain_states() {
    let mut program = scoped();
    program.functions[0].states[0].terminator = branch(3, 4);
    program.functions[0]
        .states
        .push(state(vec![read(2)], drain(0, 1, 2)));
    program.functions[0]
        .states
        .push(state(vec![read(2)], drain(0, 1, 2)));
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scopes.len(), 1);
    assert_eq!(plan.functions[0].suspensions.len(), 2);
    // A second sequential drain cannot claim the already-popped scope.
    program.functions[0].states[0].terminator = jump(3);
    program.functions[0].states[3].terminator = drain(0, 4, 2);
    rejected(&program, "ScopeDrain must close the innermost active scope");
}

#[test]
fn native_task_scope_ir_exit_may_leave_all_scopes_active() {
    let mut program = scoped();
    program.functions[0].states[0]
        .operations
        .extend([enter(1, &[7]), start(7), init_result(0)]);
    program.functions[0].states[0].terminator = exit(0);
    // Remove old drain successors; their instructions are not part of this graph.
    program.functions[0].states.truncate(1);
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scopes.len(), 2);
    assert!(plan.functions[0].suspensions.is_empty());
    assert!(plan.functions[0].slots.contains(&SlotId(6)));
}

#[test]
fn native_task_scope_ir_try_result_keeps_context_and_err_can_exit_without_pop() {
    let mut program = scoped();
    // Use two distinct Result destinations, and an ok null Value.
    program.functions[0]
        .slots
        .extend([value(IrType::Null), value(result())]);
    program.functions[0].states[0]
        .operations
        .push(init_result(9));
    program.functions[0].states[0].terminator = TaskTerminator::TryResult {
        src: SlotId(9),
        ok_value: SlotId(10),
        err_result: SlotId(11),
        ok: StateId(3),
        err: StateId(4),
    };
    program.functions[0]
        .states
        .push(state(vec![], drain(0, 1, 2)));
    program.functions[0].states.push(state(vec![], exit(11)));
    program.functions[0].states[2].operations.push(drop_if(11));
    accepted(&program);
}

#[test]
fn native_task_scope_ir_maybe_started_and_never_started_members_need_no_fake_owner() {
    let mut program = scoped();
    program.functions[0].states[0].operations.pop();
    program.functions[0].states[0].terminator = branch(3, 4);
    program.functions[0]
        .states
        .push(state(vec![start(2)], jump(5)));
    program.functions[0].states.push(state(vec![], jump(5)));
    program.functions[0]
        .states
        .push(state(vec![], drain(0, 1, 2)));
    accepted(&program);
    // The initialized union cannot justify reading the conditional owner.
    program.functions[0].states[5].operations.push(read(2));
    rejected(&program, "not definitely initialized");
}

#[test]
fn native_task_scope_ir_dense_ids_and_complete_disjoint_members() {
    let mut duplicate_member = scoped();
    duplicate_member.functions[0].states[0].operations[3] = enter(0, &[2, 2]);
    rejected(
        &duplicate_member,
        "duplicate or overlapping scope Task member",
    );
    let mut overlap = scoped();
    overlap.functions[0].states[0]
        .operations
        .push(enter(1, &[2]));
    rejected(&overlap, "duplicate or overlapping scope Task member");
    let mut duplicate_id = scoped();
    duplicate_id.functions[0].states[0]
        .operations
        .push(enter(0, &[]));
    rejected(&duplicate_id, "duplicate scope declaration");
    let mut sparse_id = scoped();
    sparse_id.functions[0].states[0].operations[3] = enter(usize::MAX, &[2, 3]);
    rejected(&sparse_id, "scope ids must be dense declaration indices");
    let mut value_member = scoped();
    value_member.functions[0].states[0].operations[3] = enter(0, &[4]);
    rejected(&value_member, "ScopeEnter members must be Task slots");
    let mut unknown_member = scoped();
    unknown_member.functions[0].states[0].operations[3] = enter(0, &[64]);
    rejected(&unknown_member, "unknown slot");
    let mut unknown_drain = scoped();
    unknown_drain.functions[0].states[0].terminator = drain(usize::MAX, 1, 2);
    rejected(&unknown_drain, "ScopeDrain references an unknown scope");
    let mut no_declaration = scoped();
    no_declaration.functions[0].states[0].operations.remove(3);
    rejected(&no_declaration, "ScopeDrain references an unknown scope");
}

#[test]
fn native_task_scope_ir_slot_63_and_empty_zero_task_scope() {
    let mut high = scoped();
    high.functions[0]
        .slots
        .resize_with(64, || value(IrType::Int));
    high.functions[0].slots[63] = task();
    high.functions[0].states[0].operations[3] = enter(0, &[63]);
    high.functions[0].states[0].operations[4] = start(63);
    let plan = accepted(&high);
    assert_eq!(plan.functions[0].scopes[0].task_mask, 1u64 << 63);
    assert!(plan.functions[0].slots.contains(&SlotId(63)));
    let zero = TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "ZeroTaskScope".into(),
            parameters: vec![],
            slots: vec![value(result()), value(IrType::Str), value(IrType::Int)],
            entry: StateId(0),
            result: result(),
            states: vec![
                state(
                    vec![
                        TaskOp::Init {
                            dst: SlotId(1),
                            value: TaskConstant::Str("held".into()),
                        },
                        init_int(2, 7),
                        enter(0, &[]),
                    ],
                    drain(0, 1, 2),
                ),
                state(vec![read(2), init_result(0)], exit(0)),
                state(vec![drop_if(0), drop_if(1)], TaskTerminator::Terminate),
            ],
        }],
    };
    let frame = &accepted(&zero).functions[0];
    assert!(frame.exit_bridge && frame.hosted);
    assert_eq!(frame.scope_task_mask, 0);
    assert_eq!(frame.scopes[0].task_mask, 0);
    assert!(frame.slots.contains(&SlotId(1)));
    assert!(frame.slots.contains(&SlotId(2)));
    assert_eq!(frame.suspensions.len(), 1); // storage, NOT mandatory progress
}

#[test]
fn native_task_scope_ir_rejects_borrow_even_when_dead_after_new_boundary() {
    let mut program = scoped();
    program.functions[0].slots.push(TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Int,
            borrowed: true,
        },
    });
    program.functions[0].states[0]
        .operations
        .push(init_int(10, 1));
    rejected(&program, "borrowed value cannot cross ScopeDrain boundary");
}

#[test]
fn native_task_scope_ir_cleanup_and_unreachable_constructs_are_rejected() {
    let mut enter_cleanup = scoped();
    enter_cleanup.functions[0].states[2]
        .operations
        .push(enter(1, &[]));
    rejected(&enter_cleanup, "cleanup cannot enter a scope");
    let mut drain_cleanup = scoped();
    drain_cleanup.functions[0].states[2].terminator = drain(0, 1, 2);
    rejected(&drain_cleanup, "cleanup cannot complete or suspend");
    let mut dead_enter = scoped();
    dead_enter.functions[0]
        .states
        .push(state(vec![enter(1, &[])], exit(0)));
    rejected(&dead_enter, "ScopeEnter must be normal-reachable");
    let mut dead_drain = scoped();
    dead_drain.functions[0]
        .states
        .push(state(vec![], drain(0, 1, 2)));
    rejected(&dead_drain, "ScopeDrain must be normal-reachable");
}

#[test]
fn native_task_scope_ir_scoped_cycles_preserve_ownership_and_require_real_suspend() {
    let mut reentry = scoped();
    reentry.functions[0]
        .states
        .push(state(vec![init_result(0)], exit(0)));
    reentry.functions[0].states[1].terminator = TaskTerminator::Suspend {
        resume: StateId(0),
        cleanup: StateId(2),
    };
    // This old fixture also repeats ROOT Start(1) and Result(0) initialization.
    // The inner drain clears neither: lifting the DAG gate must not accept it.
    rejected(
        &reentry,
        "overwriting a possibly initialized owned slot without drop",
    );
    let mut active = scoped();
    active.functions[0].states[0].terminator = TaskTerminator::Suspend {
        resume: StateId(3),
        cleanup: StateId(2),
    };
    active.functions[0].states.push(state(
        vec![],
        TaskTerminator::Await {
            task: SlotId(2),
            dst: SlotId(9),
            ready: StateId(3),
            cleanup: StateId(2),
        },
    ));
    // An earlier Suspend does not guard this later Await-only self-cycle.
    rejected(&active, "cycle without suspension is not supported");
    let mut empty = scoped();
    empty.functions[0].states[0].operations.truncate(3);
    empty.functions[0].states[0].operations.push(enter(0, &[]));
    empty.functions[0].states[1].terminator = jump(0);
    empty.functions[0]
        .states
        .push(state(vec![init_result(0)], exit(0)));
    rejected(&empty, "cycle without suspension is not supported");
}

#[test]
fn native_task_scope_ir_scopes_require_exit_and_legacy_complete_remains_separate() {
    let mut mixed = scoped();
    mixed.functions[0].states[1].terminator = TaskTerminator::Complete { value: SlotId(0) };
    rejected(&mixed, "Exit bridge cannot contain legacy Complete");
    let mut dead_complete = scoped();
    dead_complete.functions[0].states.push(state(
        vec![init_result(0)],
        TaskTerminator::Complete { value: SlotId(0) },
    ));
    rejected(&dead_complete, "Exit bridge cannot contain legacy Complete");
    let mut no_exit = scoped();
    no_exit.functions[0].states[1].terminator = jump(1);
    rejected(&no_exit, "scope graph requires typed Exit");
    let old = TaskProgram {
        functions: vec![leaf()],
    };
    let plan = accepted(&old);
    assert!(plan.functions[0].scopes.is_empty());
    assert!(!plan.functions[0].exit_bridge && !plan.functions[0].hosted);
}

#[test]
fn native_task_scope_ir_start_contract_and_owned_facts_are_not_weakened() {
    let mut overwrite = scoped();
    overwrite.functions[0].states[0].operations.push(start(2));
    rejected(&overwrite, "overwriting a possibly initialized owned slot");
    let mut arity = scoped();
    arity.functions[0].states[0].operations[4] = TaskOp::Start {
        dst: SlotId(2),
        function: TaskFunctionId(9),
        arguments: vec![SlotId(6)],
    };
    rejected(&arity, "Start argument count does not match parameters");
    let mut recursion = scoped();
    recursion.functions[0].states[0].operations[4] = TaskOp::Start {
        dst: SlotId(2),
        function: TaskFunctionId(0),
        arguments: vec![SlotId(6), SlotId(8)],
    };
    rejected(&recursion, "recursive Start graph is not supported");
    let mut missing_drop = scoped();
    missing_drop.functions[0].states[2]
        .operations
        .retain(|op| op != &drop_if(6));
    rejected(
        &missing_drop,
        "termination leaves owned slots without cleanup",
    );
    let mut moved_value = scoped();
    moved_value.functions[0].slots.push(value(IrType::Str));
    moved_value.functions[0].states[0]
        .operations
        .extend([moved(6, 10), read(6)]);
    moved_value.functions[0].states[2]
        .operations
        .push(drop_if(10));
    rejected(&moved_value, "not definitely initialized");
}

fn minimum_work(program: &TaskProgram) -> usize {
    accepted(program);
    let mut low = 0;
    let mut high = TaskLimits::default().max_analysis_work;
    while low < high {
        let middle = low + (high - low) / 2;
        let limits = TaskLimits {
            max_analysis_work: middle,
            ..TaskLimits::default()
        };
        if verify_and_plan(program, limits).is_ok() {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

#[test]
fn native_task_scope_ir_budget_charges_members_scope_edges_and_storage() {
    let program = scoped();
    let minimum = minimum_work(&program);
    assert!(minimum > 0);
    assert!(verify_and_plan(
        &program,
        TaskLimits {
            max_analysis_work: minimum,
            ..TaskLimits::default()
        }
    )
    .is_ok());
    let error = verify_and_plan(
        &program,
        TaskLimits {
            max_analysis_work: minimum - 1,
            ..TaskLimits::default()
        },
    )
    .unwrap_err();
    assert!(error.message.contains("analysis work limit exceeded"));
    let mut extra_members = program.clone();
    extra_members.functions[0].states[0].operations[3] = enter(0, &[2, 3, 7]);
    assert!(minimum_work(&extra_members) > minimum);
    let mut oversized = program.clone();
    oversized.functions[0].states[0].operations[3] = enter(0, &vec![2; 65]);
    rejected(&oversized, "ScopeEnter member limit exceeded");
    let operation_count: usize = program
        .functions
        .iter()
        .flat_map(|function| &function.states)
        .map(|state| state.operations.len())
        .sum();
    let limits = TaskLimits {
        max_operations: operation_count + 1,
        ..TaskLimits::default()
    };
    let error = verify_and_plan(&program, limits).unwrap_err(); // two declared members
    assert!(error.message.contains("operation limit exceeded"));
}

// Tests for the Suspend-only progress-cut slice. Slot schema is
// deliberately shared; unused slots never acquire an owner.
// 0 final Result; 1 Task; 2 Bool parameter; 3/4 Owned str; 5 Await Result.
fn loop_program(states: Vec<TaskState>) -> TaskProgram {
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "ScopedLoop".into(),
                parameters: vec![SlotId(2)],
                slots: vec![
                    value(result()),
                    task(),
                    value(IrType::Bool),
                    value(IrType::Str),
                    value(IrType::Str),
                    value(result()),
                ],
                entry: StateId(0),
                result: result(),
                states,
            },
            leaf(),
        ],
    }
}

fn loop_branch(then_state: usize, else_state: usize) -> TaskTerminator {
    TaskTerminator::Branch {
        condition: SlotId(2),
        then_state: StateId(then_state),
        else_state: StateId(else_state),
    }
}

fn loop_suspend(resume: usize, cleanup: usize) -> TaskTerminator {
    TaskTerminator::Suspend {
        resume: StateId(resume),
        cleanup: StateId(cleanup),
    }
}

fn loop_cleanup() -> TaskState {
    state(
        vec![drop_if(0), drop_if(3), drop_if(4), drop_if(5)],
        TaskTerminator::Terminate,
    )
}

fn loop_string(dst: usize) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value: TaskConstant::Str("loop-owned".into()),
    }
}

fn scope_exit_begin(id: usize) -> TaskOp {
    TaskOp::ScopeExitBegin {
        exit: TaskScopeExitId(id),
        retain: None,
    }
}

fn scope_exit_end(id: usize) -> TaskOp {
    TaskOp::ScopeExitEnd {
        exit: TaskScopeExitId(id),
    }
}

fn without_scope_exit_markers(program: &TaskProgram) -> TaskProgram {
    let mut control = program.clone();
    for function in &mut control.functions {
        for state in &mut function.states {
            state.operations.retain(|operation| {
                !matches!(
                    operation,
                    TaskOp::ScopeExitBegin { .. } | TaskOp::ScopeExitEnd { .. }
                )
            });
        }
    }
    control
}

fn scope_exit_loop_program(has_task: bool) -> TaskProgram {
    let mut body = vec![enter(0, if has_task { &[1] } else { &[] })];
    if has_task {
        body.push(start(1));
    }
    body.push(loop_string(3));
    loop_program(vec![
        state(body, jump(1)),
        state(vec![scope_exit_begin(0)], jump(2)),
        state(vec![], drain(0, 3, 8)),
        state(vec![drop_if(3)], jump(4)),
        state(vec![scope_exit_end(0)], jump(5)),
        state(vec![], loop_suspend(6, 8)),
        state(vec![], loop_branch(0, 7)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ])
}

#[test]
fn native_task_scope_exit_ir_distinct_operations_join_only_after_both_ends() {
    let program = loop_program(vec![
        state(vec![enter(0, &[1]), start(1)], loop_branch(1, 4)),
        state(vec![scope_exit_begin(0)], jump(2)),
        state(vec![], drain(0, 3, 8)),
        state(vec![scope_exit_end(0)], jump(7)),
        state(vec![scope_exit_begin(1)], jump(5)),
        state(vec![], drain(0, 6, 8)),
        state(vec![scope_exit_end(1)], jump(7)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scope_exits.operations.len(), 2);
    assert_eq!(
        plan.functions[0].scope_exits.state_operations,
        vec![
            None,
            None,
            Some(TaskScopeExitId(0)),
            Some(TaskScopeExitId(0)),
            None,
            Some(TaskScopeExitId(1)),
            Some(TaskScopeExitId(1)),
            None,
            None,
        ]
    );
    let mut bad = program;
    // Scope stacks are already ROOT on both incoming edges; only the
    // operation identity forbids using operation 1's End after End(0).
    bad.functions[0].states[3].terminator = jump(6);
    accepted(&without_scope_exit_markers(&bad));
    rejected(&bad, "crosses a scope-exit operation boundary");
}

#[test]
fn native_task_scope_exit_ir_same_markers_reenter_only_after_end_and_suspend() {
    let program = scope_exit_loop_program(true);
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scope_exits.operations.len(), 1);
    assert_eq!(
        plan.functions[0].scope_exits.operations[0],
        TaskScopeExitFrame {
            exit: TaskScopeExitId(0),
            begin: StateId(1),
            end: StateId(4),
            retain: None,
        }
    );
    assert_eq!(plan.functions[0].scope_exits.state_operations[5], None);
    assert!(plan.functions[0]
        .suspensions
        .iter()
        .any(|suspension| suspension.state == StateId(5)));
    let mut bad = program;
    bad.functions[0].states[5].terminator = jump(6);
    rejected(&bad, "cycle without suspension is not supported");
}

#[test]
fn native_task_scope_exit_ir_exact_budget_exceeds_same_cfg_marker_free_control() {
    let program = scope_exit_loop_program(true);
    let control = without_scope_exit_markers(&program);
    let expected = accepted(&program);
    let control_expected = accepted(&control);
    assert_eq!(
        control_expected.functions[0].scope_exits,
        TaskScopeExitPlan::default()
    );
    let minimum = minimum_work(&program);
    let control_minimum = minimum_work(&control);
    assert!(control_minimum > 0 && minimum > control_minimum);
    for (candidate, expected_plan, work) in [
        (&program, &expected, minimum),
        (&control, &control_expected, control_minimum),
    ] {
        let exact = TaskLimits {
            max_analysis_work: work,
            ..TaskLimits::default()
        };
        assert_eq!(&verify_and_plan(candidate, exact).unwrap(), expected_plan);
        let error = verify_and_plan(
            candidate,
            TaskLimits {
                max_analysis_work: work - 1,
                ..TaskLimits::default()
            },
        )
        .unwrap_err();
        assert!(error.message.contains("analysis work limit exceeded"));
    }
    let error = verify_and_plan(
        &program,
        TaskLimits {
            max_analysis_work: control_minimum,
            ..TaskLimits::default()
        },
    )
    .unwrap_err();
    assert!(error.message.contains("analysis work limit exceeded"));
}

#[test]
fn native_task_scope_exit_ir_empty_task_scope_keeps_final_value_drop_inside_operation() {
    let program = scope_exit_loop_program(false);
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scopes[0].task_mask, 0);
    assert_eq!(
        plan.functions[0].scope_exits.state_operations[3],
        Some(TaskScopeExitId(0))
    );
    assert_eq!(program.functions[0].states[3].operations, vec![drop_if(3)]);
    assert_eq!(plan.functions[0].scope_exits.state_operations[5], None);
    let mut missing_drop = program;
    missing_drop.functions[0].states[3].operations.clear();
    rejected(
        &missing_drop,
        "overwriting a possibly initialized owned slot without drop",
    );
}

#[test]
fn native_task_scope_exit_ir_usize_max_marker_ids_are_not_allocation_sizes() {
    let program = scope_exit_loop_program(false);
    accepted(&program);
    for (state_index, operation) in [
        (1, scope_exit_begin(usize::MAX)),
        (4, scope_exit_end(usize::MAX)),
    ] {
        let mut bad = program.clone();
        bad.functions[0].states[state_index].operations[0] = operation;
        rejected(&bad, "scope-exit ids must be dense Begin indices");
    }
}

fn reject_scope_exit_interior_terminator(terminator: TaskTerminator) {
    let program = loop_program(vec![
        state(vec![enter(0, &[1]), start(1)], jump(1)),
        state(vec![scope_exit_begin(0)], jump(2)),
        state(vec![], terminator),
        state(vec![drop_if(5)], drain(0, 4, 7)),
        state(vec![], jump(5)),
        state(vec![scope_exit_end(0)], jump(6)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    // Same CFG and actual Await ownership, without markers, are legal. A
    // Jump replacement is separately legal WITH the operation boundaries.
    accepted(&without_scope_exit_markers(&program));
    let mut jump_control = program.clone();
    jump_control.functions[0].states[2].terminator = jump(3);
    accepted(&jump_control);
    rejected(
        &program,
        "scope-exit operation requires a finite drain chain",
    );
}

#[test]
fn native_task_scope_exit_ir_interior_await_is_not_a_cleanup_boundary() {
    reject_scope_exit_interior_terminator(TaskTerminator::Await {
        task: SlotId(1),
        dst: SlotId(5),
        ready: StateId(3),
        cleanup: StateId(7),
    });
}

#[test]
fn native_task_scope_exit_ir_interior_suspend_is_not_an_operation_end() {
    reject_scope_exit_interior_terminator(loop_suspend(3, 7));
}

#[test]
fn native_task_scope_exit_ir_interior_branch_is_not_a_linear_drain_chain() {
    reject_scope_exit_interior_terminator(loop_branch(3, 3));
}

#[test]
fn native_task_scope_ir_loop_reenters_same_scope_and_task_slot_after_drain() {
    let program = loop_program(vec![
        state(vec![enter(0, &[1]), start(1)], drain(0, 1, 4)),
        state(vec![], loop_suspend(2, 4)),
        state(vec![], loop_branch(0, 3)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    let plan = accepted(&program);
    assert_eq!(
        plan.functions[0].scopes,
        vec![TaskScopeFrame {
            scope: TaskScopeId(0),
            task_mask: 1u64 << 1,
        }]
    );
    assert_eq!(plan.functions[0].suspensions.len(), 2);
    assert_eq!(accepted(&program), plan);
}

#[test]
fn native_task_scope_ir_loop_rejects_enter_while_same_scope_is_active() {
    let program = loop_program(vec![
        state(vec![enter(0, &[])], loop_branch(1, 2)),
        state(vec![], loop_suspend(0, 3)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    rejected(&program, "normal join has different active scope stacks");
}

#[test]
fn native_task_scope_ir_loop_rejects_ready_await_cycle_after_earlier_suspend() {
    let program = loop_program(vec![
        state(vec![enter(0, &[1])], loop_suspend(1, 5)),
        state(
            vec![start(1)],
            TaskTerminator::Await {
                task: SlotId(1),
                dst: SlotId(5),
                ready: StateId(2),
                cleanup: StateId(5),
            },
        ),
        state(vec![TaskOp::Drop { slot: SlotId(5) }], loop_branch(1, 3)),
        state(vec![], drain(0, 4, 5)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    // Start/Await/drop balance ownership. Only the lack of a Suspend on the
    // 1 -> 2 -> 1 cycle should reject it, not an unrelated ownership error.
    rejected(&program, "cycle without suspension is not supported");
}

#[test]
fn native_task_scope_ir_loop_rejects_empty_drain_only_cycle() {
    let program = loop_program(vec![
        state(vec![enter(0, &[])], drain(0, 1, 3)),
        state(vec![], loop_branch(0, 2)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    rejected(&program, "cycle without suspension is not supported");
}

#[test]
fn native_task_scope_ir_loop_drain_does_not_erase_owned_value_may_facts() {
    let mut program = loop_program(vec![
        state(vec![enter(0, &[]), loop_string(3)], drain(0, 1, 4)),
        state(vec![], loop_suspend(2, 4)),
        state(vec![], loop_branch(0, 3)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    rejected(
        &program,
        "overwriting a possibly initialized owned slot without drop",
    );
    // Match the source lowerer's normal ready-edge cleanup responsibility.
    program.functions[0].states[1].operations.push(drop_if(3));
    accepted(&program);
}

#[test]
fn native_task_scope_ir_loop_rejects_task_overwrite_but_can_hold_one_task_across_yields() {
    let mut program = loop_program(vec![
        state(vec![enter(0, &[1])], jump(1)),
        state(vec![start(1)], loop_suspend(2, 5)),
        state(vec![], loop_branch(1, 3)),
        state(vec![], drain(0, 4, 5)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    rejected(
        &program,
        "overwriting a possibly initialized owned slot without drop",
    );
    // ScopeEnter/Start once, then repeated suspension inside that scope is
    // legal; balanced reentry does not imply every yield must pop the scope.
    program.functions[0].states[0].operations.push(start(1));
    program.functions[0].states[1].operations.clear();
    let plan = accepted(&program);
    let suspension = plan.functions[0]
        .suspensions
        .iter()
        .find(|live| live.state == StateId(1))
        .expect("loop suspension is planned");
    assert!(suspension.slots.contains(&SlotId(1)));
}

#[test]
fn native_task_scope_ir_loop_must_reaches_fixpoint_after_owned_move() {
    let mut program = loop_program(vec![
        state(vec![enter(0, &[])], jump(1)),
        state(
            vec![read(3), moved(3, 4), TaskOp::Drop { slot: SlotId(4) }],
            loop_suspend(2, 5),
        ),
        state(vec![], loop_branch(1, 3)),
        state(vec![], drain(0, 4, 5)),
        state(vec![init_result(0)], exit(0)),
        loop_cleanup(),
    ]);
    program.functions[0].parameters.push(SlotId(3));
    rejected(
        &program,
        "read, move or drop of a slot that is not definitely initialized",
    );
    // The initial parameter is insufficient after a backedge; explicit
    // regeneration restores MUST and the carried owner is saved at Suspend.
    program.functions[0].states[1]
        .operations
        .push(loop_string(3));
    let plan = accepted(&program);
    let suspension = plan.functions[0]
        .suspensions
        .iter()
        .find(|live| live.state == StateId(1))
        .expect("loop suspension is planned");
    assert!(suspension.slots.contains(&SlotId(3)));
}
