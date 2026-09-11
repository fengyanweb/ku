//! R5c typed ownership/edge planning. Execution is covered by generated C tests.
use ku::ir::{task::*, IrType};

fn result(ty: IrType) -> IrType {
    IrType::Result(Box::new(ty))
}
fn value(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}
fn task(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Task { result: result(ty) },
    }
}
fn state(operations: Vec<TaskOp>, terminator: TaskTerminator) -> TaskState {
    TaskState {
        operations,
        terminator,
    }
}
fn done(value: usize) -> TaskTerminator {
    TaskTerminator::Complete {
        value: SlotId(value),
    }
}
fn jump(target: usize) -> TaskTerminator {
    TaskTerminator::Jump {
        target: StateId(target),
    }
}
fn read(slot: usize) -> TaskOp {
    TaskOp::Read { slot: SlotId(slot) }
}
fn drop(slot: usize) -> TaskOp {
    TaskOp::Drop { slot: SlotId(slot) }
}
fn drop_if(slot: usize) -> TaskOp {
    TaskOp::DropIfInit { slot: SlotId(slot) }
}
fn null_result(dst: usize) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
    }
}
fn start(dst: usize, function: usize, arguments: &[usize]) -> TaskOp {
    TaskOp::Start {
        dst: SlotId(dst),
        function: TaskFunctionId(function),
        arguments: arguments.iter().copied().map(SlotId).collect(),
    }
}
fn leaf(ty: IrType) -> TaskFunction {
    TaskFunction {
        id: TaskFunctionId(9),
        name: "ForwardLeaf".into(),
        slots: vec![value(ty.clone()), value(result(ty.clone()))],
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        states: vec![state(
            vec![TaskOp::WrapOk {
                dst: SlotId(1),
                src: SlotId(0),
            }],
            done(1),
        )],
        result: result(ty),
    }
}
fn await_program(ty: IrType) -> TaskProgram {
    let mut print = vec![TaskOp::Print {
        value: SlotId(4),
        newline: true,
    }];
    if ty == IrType::Str {
        print.push(drop(4));
    }
    print.push(null_result(6));
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "Main".into(),
                slots: vec![
                    value(ty.clone()),
                    task(ty.clone()),
                    task(ty.clone()),
                    value(result(ty.clone())),
                    value(ty.clone()),
                    value(result(IrType::Null)),
                    value(result(IrType::Null)),
                ],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![
                    state(
                        vec![
                            start(1, 9, &[0]),
                            TaskOp::Move {
                                dst: SlotId(2),
                                src: SlotId(1),
                            },
                        ],
                        jump(1),
                    ),
                    state(
                        vec![],
                        TaskTerminator::Await {
                            task: SlotId(2),
                            dst: SlotId(3),
                            ready: StateId(2),
                            cleanup: StateId(5),
                        },
                    ),
                    state(
                        vec![],
                        TaskTerminator::TryResult {
                            src: SlotId(3),
                            ok_value: SlotId(4),
                            err_result: SlotId(5),
                            ok: StateId(3),
                            err: StateId(4),
                        },
                    ),
                    state(print, done(6)),
                    state(vec![], done(5)),
                    state(
                        vec![drop_if(0), drop_if(3), drop_if(4), drop_if(5), drop_if(6)],
                        TaskTerminator::Terminate,
                    ),
                ],
            },
            leaf(ty),
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
fn native_task_start_await_ir_forward_calls_and_all_primitive_payloads() {
    for ty in [IrType::Int, IrType::Bool, IrType::Null, IrType::Str] {
        let program = await_program(ty.clone());
        let plan = accepted(&program);
        assert!(plan.functions[0].hosted);
        assert_eq!(plan.functions[0].scope_task_mask, 6);
        assert!(!plan.functions[1].hosted);
        assert_eq!(plan.functions[1].scope_task_mask, 0);
        assert_eq!(plan.functions[0].suspensions[0].state, StateId(1));
        assert!(plan.functions[0].suspensions[0].slots.contains(&SlotId(2)));
        for slot in [1, 2, 3, 5, 6] {
            assert!(plan.functions[0].slots.contains(&SlotId(slot)));
        }
        if ty == IrType::Str {
            assert!(plan.functions[0].slots.contains(&SlotId(4)));
        }
        assert_eq!(plan, accepted(&program));
    }
}

#[test]
fn native_task_start_await_ir_owned_arguments_move_and_copy_arguments_remain() {
    let mut copy = await_program(IrType::Int);
    copy.functions[0].states[0].operations.push(read(0));
    accepted(&copy);
    let mut owned = await_program(IrType::Str);
    owned.functions[0].states[0].operations.push(read(0));
    rejected(&owned, "not definitely initialized");
    for ty in [IrType::Int, IrType::Str] {
        let mut moved = await_program(ty);
        moved.functions[0].states[0].operations.push(read(1));
        rejected(&moved, "not definitely initialized");
    }
}

#[test]
fn native_task_start_await_ir_task_is_not_copy_drop_wrap_print_or_parameter() {
    for operation in [
        TaskOp::Copy {
            dst: SlotId(2),
            src: SlotId(1),
        },
        drop(1),
        drop_if(1),
        TaskOp::WrapOk {
            dst: SlotId(3),
            src: SlotId(1),
        },
        TaskOp::Print {
            value: SlotId(1),
            newline: false,
        },
    ] {
        let mut program = await_program(IrType::Int);
        program.functions[0].states[0]
            .operations
            .insert(1, operation);
        assert!(verify_and_plan(&program, TaskLimits::default()).is_err());
    }
    let mut parameter = await_program(IrType::Int);
    parameter.functions[0].parameters.push(SlotId(1));
    rejected(&parameter, "requires a Value slot");
    let mut completion = await_program(IrType::Int);
    completion.functions[0].states[3].terminator = done(2);
    rejected(&completion, "requires a Value slot");
    let mut nested = await_program(IrType::Int);
    nested.functions[0].slots[1].ty = TaskSlotType::Task {
        result: result(result(IrType::Int)),
    };
    rejected(&nested, "unsupported slot payload type");
}

#[test]
fn native_task_start_await_ir_task_type_is_result_not_callee_identity() {
    let mut program = await_program(IrType::Int);
    let mut second = leaf(IrType::Int);
    second.id = TaskFunctionId(17);
    program.functions.push(second);
    program.functions[0].states[0].operations[0] = start(1, 17, &[0]);
    accepted(&program);
    program.functions[0].slots[2] = task(IrType::Bool);
    rejected(&program, "Task move requires distinct matching owned slots");
}

#[test]
fn native_task_start_await_ir_await_has_distinct_ready_and_host_cleanup_facts() {
    let mut program = await_program(IrType::Int);
    // A second unawaited task remains for final drain, but is already moved
    // by the host before entering the user Value-cleanup CFG.
    program.functions[0].states[0]
        .operations
        .push(start(1, 9, &[0]));
    accepted(&program);
    let mut ready = program.clone();
    ready.functions[0].states[2].operations.push(read(2));
    rejected(&ready, "not definitely initialized");
    for slot in [1, 2, 3] {
        let mut cleanup = program.clone();
        cleanup.functions[0].states[5]
            .operations
            .insert(0, read(slot));
        rejected(&cleanup, "not definitely initialized");
    }
    // A pre-existing owned Value is not erased with the Task bits.
    program.functions[0].slots.push(value(IrType::Str));
    program.functions[0].states[0]
        .operations
        .push(TaskOp::Init {
            dst: SlotId(7),
            value: TaskConstant::Str("retained".into()),
        });
    program.functions[0].states[3].operations.insert(0, drop(7));
    program.functions[0].states[4].operations.push(drop(7));
    program.functions[0].states[5].operations.push(drop(7));
    accepted(&program);
    program.functions[0].states[5].operations.pop();
    rejected(&program, "termination leaves owned slots without cleanup");
}

#[test]
fn native_task_start_await_ir_repeated_await_and_result_overwrite_are_rejected() {
    let mut repeated = await_program(IrType::Int);
    // Keep this ownership negative acyclic. A loop must now fail the earlier
    // progress gate rather than accidentally becoming its only rejection.
    repeated.functions[0].states[2] = state(
        vec![drop(3)],
        TaskTerminator::Await {
            task: SlotId(2),
            dst: SlotId(3),
            ready: StateId(6),
            cleanup: StateId(5),
        },
    );
    repeated.functions[0]
        .states
        .push(state(vec![drop(3), null_result(6)], done(6)));
    rejected(&repeated, "not definitely initialized");
    let mut overwritten = await_program(IrType::Null);
    overwritten.functions[0].states[0]
        .operations
        .push(null_result(3));
    rejected(&overwritten, "Await overwrites");
    let mut uninitialized = await_program(IrType::Int);
    uninitialized.functions[0].states[0].operations.clear();
    rejected(&uninitialized, "Await reads a Task");
}

#[test]
fn native_task_start_await_ir_try_result_moves_source_only_initializes_selected_branch() {
    let program = await_program(IrType::Str);
    accepted(&program);
    for (branch, slot) in [(3, 3), (4, 3), (3, 5), (4, 4)] {
        let mut bad = program.clone();
        bad.functions[0].states[branch]
            .operations
            .insert(0, read(slot));
        rejected(&bad, "not definitely initialized");
    }
    let mut missing_drop = program.clone();
    missing_drop.functions[0].states[3].operations.remove(1);
    rejected(
        &missing_drop,
        "completion leaves owned slots without cleanup",
    );
    let mut alias = program;
    alias.functions[0].states[2].terminator = TaskTerminator::TryResult {
        src: SlotId(3),
        ok_value: SlotId(4),
        err_result: SlotId(3),
        ok: StateId(3),
        err: StateId(4),
    };
    rejected(&alias, "TryResult requires distinct");
}

#[test]
fn native_task_start_await_ir_try_result_join_has_may_not_must_destinations() {
    let mut function = TaskFunction {
        id: TaskFunctionId(0),
        name: "ValueOnlyTry".into(),
        slots: vec![
            value(result(IrType::Str)),
            value(IrType::Str),
            value(result(IrType::Null)),
            value(result(IrType::Null)),
        ],
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        result: result(IrType::Null),
        states: vec![
            state(
                vec![],
                TaskTerminator::TryResult {
                    src: SlotId(0),
                    ok_value: SlotId(1),
                    err_result: SlotId(2),
                    ok: StateId(1),
                    err: StateId(1),
                },
            ),
            state(vec![drop_if(1), drop_if(2), null_result(3)], done(3)),
        ],
    };
    let plan = accepted(&TaskProgram {
        functions: vec![function.clone()],
    });
    assert!(!plan.functions[0].hosted);
    assert_eq!(plan.functions[0].scope_task_mask, 0);
    for slot in [0, 1, 2] {
        function.states[1].operations.insert(0, read(slot));
        rejected(
            &TaskProgram {
                functions: vec![function.clone()],
            },
            "not definitely initialized",
        );
        function.states[1].operations.remove(0);
    }
    function.states[1].operations.insert(0, null_result(2));
    rejected(
        &TaskProgram {
            functions: vec![function],
        },
        "possibly initialized owned slot",
    );
}

fn signature_program(ty: IrType, arguments: &[usize]) -> TaskProgram {
    let owned = !matches!(ty, IrType::Int | IrType::Bool | IrType::Null);
    let mut cleanup = vec![];
    if owned {
        cleanup.extend([drop(0), drop(1)]);
    }
    cleanup.push(null_result(2));
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "Caller".into(),
                slots: vec![
                    value(ty.clone()),
                    task(IrType::Null),
                    value(result(IrType::Null)),
                ],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![state(vec![start(1, 9, arguments), null_result(2)], done(2))],
            },
            TaskFunction {
                id: TaskFunctionId(9),
                name: "TwoParameters".into(),
                slots: vec![value(ty.clone()), value(ty), value(result(IrType::Null))],
                parameters: vec![SlotId(0), SlotId(1)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![state(cleanup, done(2))],
            },
        ],
    }
}

#[test]
fn native_task_start_await_ir_start_checks_signatures_duplicates_and_empty_destination() {
    accepted(&signature_program(IrType::Int, &[0, 0]));
    for ty in [IrType::Str, result(IrType::Int)] {
        rejected(
            &signature_program(ty, &[0, 0]),
            "consume an owned argument twice",
        );
    }
    let mut program = await_program(IrType::Str);
    program.functions[0].states[0].operations[0] = start(1, 555, &[0]);
    rejected(&program, "unknown function");
    program.functions[0].states[0].operations[0] = start(1, 9, &[]);
    rejected(&program, "argument count");
    program.functions[0].states[0].operations[0] = start(1, 9, &[999]);
    rejected(&program, "unknown slot");
    program = await_program(IrType::Int);
    program.functions[0].slots[0] = value(IrType::Bool);
    rejected(&program, "argument type");
    program = await_program(IrType::Int);
    program.functions[0].states[0]
        .operations
        .insert(1, start(1, 9, &[0]));
    rejected(&program, "possibly initialized owned slot");
    program = await_program(IrType::Int);
    program.functions[1].parameters[0] = SlotId(999);
    rejected(&program, "unknown callee parameter slot");
}

#[test]
fn native_task_start_await_ir_single_result_arguments_are_owned_even_when_ok_is_copy() {
    let ty = result(IrType::Int);
    let mut program = signature_program(ty.clone(), &[0, 1]);
    program.functions[0].slots.insert(1, value(ty));
    program.functions[0].parameters = vec![SlotId(0), SlotId(1)];
    program.functions[0].states[0] = state(vec![start(2, 9, &[0, 1]), null_result(3)], done(3));
    accepted(&program);
    program.functions[0].states[0].operations.insert(1, read(0));
    rejected(&program, "not definitely initialized");
}

#[test]
fn native_task_start_await_ir_cleanup_cannot_start_await_or_reenter_normal() {
    let program = await_program(IrType::Int);
    let mut start_cleanup = program.clone();
    start_cleanup.functions[0].states[5]
        .operations
        .insert(0, start(1, 9, &[0]));
    rejected(&start_cleanup, "cleanup cannot Start");
    for terminator in [
        TaskTerminator::Await {
            task: SlotId(2),
            dst: SlotId(3),
            ready: StateId(2),
            cleanup: StateId(5),
        },
        TaskTerminator::Suspend {
            resume: StateId(2),
            cleanup: StateId(5),
        },
        done(5),
    ] {
        let mut bad = program.clone();
        bad.functions[0].states[5].terminator = terminator;
        rejected(&bad, "cleanup cannot complete or suspend");
    }
    let mut reenter = program;
    reenter.functions[0].states[5].terminator = jump(2);
    rejected(&reenter, "cleanup cannot reenter");
}

#[test]
fn native_task_start_await_ir_only_suspend_guarantees_progress_on_every_cycle() {
    let mut program = TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "CooperativeInternalLoop".into(),
                slots: vec![
                    value(IrType::Int),
                    task(IrType::Int),
                    value(result(IrType::Int)),
                ],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![
                    state(vec![start(1, 9, &[0])], jump(1)),
                    state(
                        vec![],
                        TaskTerminator::Await {
                            task: SlotId(1),
                            dst: SlotId(2),
                            ready: StateId(2),
                            cleanup: StateId(3),
                        },
                    ),
                    state(vec![drop(2)], jump(0)),
                    state(vec![], TaskTerminator::Terminate),
                ],
            },
            leaf(IrType::Int),
        ],
    };
    rejected(&program, "cycle without suspension");
    let mut cooperative = program.clone();
    cooperative.functions[0].states[2].terminator = TaskTerminator::Suspend {
        resume: StateId(0),
        cleanup: StateId(3),
    };
    accepted(&cooperative);
    // A Suspend before entering an inner Await-only cycle is insufficient.
    let mut leading_suspend = program.clone();
    leading_suspend.functions[0].states.push(state(
        vec![],
        TaskTerminator::Suspend {
            resume: StateId(0),
            cleanup: StateId(3),
        },
    ));
    leading_suspend.functions[0].entry = StateId(4);
    rejected(&leading_suspend, "cycle without suspension");
    cooperative.functions[0].states[3].terminator = jump(3);
    rejected(&cooperative, "cycle without suspension");
    program.functions[0].states[1].terminator = jump(2);
    rejected(&program, "cycle without suspension");
}

fn spawning_function(id: usize, callee: usize) -> TaskFunction {
    TaskFunction {
        id: TaskFunctionId(id),
        name: format!("F{id}"),
        slots: vec![task(IrType::Null), value(result(IrType::Null))],
        parameters: vec![],
        entry: StateId(0),
        result: result(IrType::Null),
        states: vec![state(vec![start(0, callee, &[]), null_result(1)], done(1))],
    }
}

#[test]
fn native_task_start_await_ir_recursive_start_graph_is_rejected_before_lowering() {
    rejected(
        &TaskProgram {
            functions: vec![spawning_function(0, 0)],
        },
        "recursive Start graph",
    );
    rejected(
        &TaskProgram {
            functions: vec![spawning_function(0, 9), spawning_function(9, 0)],
        },
        "recursive Start graph",
    );
}

#[test]
fn native_task_start_await_ir_hosted_owned_persistence_does_not_relax_value_cleanup() {
    let mut program = await_program(IrType::Int);
    program.functions[0].slots.push(value(IrType::Str));
    program.functions[0].states[0].operations.extend([
        TaskOp::Init {
            dst: SlotId(7),
            value: TaskConstant::Str("temporary".into()),
        },
        drop(7),
    ]);
    let plan = accepted(&program);
    assert!(plan.functions[0].slots.contains(&SlotId(7)));
    program.functions[0].states[0].operations.pop();
    rejected(&program, "completion leaves owned slots without cleanup");
}

#[test]
fn native_task_start_await_ir_suspend_preserves_task_until_host_cleanup_handoff() {
    let mut program = TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "Unawaited".into(),
                slots: vec![
                    value(IrType::Int),
                    task(IrType::Int),
                    value(result(IrType::Null)),
                ],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![
                    state(
                        vec![start(1, 9, &[0])],
                        TaskTerminator::Suspend {
                            resume: StateId(1),
                            cleanup: StateId(2),
                        },
                    ),
                    state(vec![read(1), null_result(2)], done(2)),
                    state(vec![], TaskTerminator::Terminate),
                ],
            },
            leaf(IrType::Int),
        ],
    };
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scope_task_mask, 2);
    assert!(plan.functions[0].suspensions[0].slots.contains(&SlotId(1)));
    program.functions[0].states[2].operations.push(read(1));
    rejected(&program, "not definitely initialized");
}

#[test]
fn native_task_start_await_ir_copy_borrow_argument_is_materialized_not_spilled() {
    let mut program = await_program(IrType::Int);
    program.functions[0].slots.push(TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Int,
            borrowed: true,
        },
    });
    program.functions[0].states[0].operations[0] = start(1, 9, &[7]);
    program.functions[0].states[0].operations.insert(
        0,
        TaskOp::Init {
            dst: SlotId(7),
            value: TaskConstant::Int(7),
        },
    );
    let plan = accepted(&program);
    assert!(!plan.functions[0].slots.contains(&SlotId(7)));
    program.functions[0].states[2].operations.push(read(7));
    rejected(&program, "borrowed value cannot cross suspension");
    let mut owned = await_program(IrType::Str);
    owned.functions[0].slots.push(TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Str,
            borrowed: true,
        },
    });
    owned.functions[0].states[0].operations[0] = start(1, 9, &[7]);
    owned.functions[0].states[0].operations.insert(
        0,
        TaskOp::Init {
            dst: SlotId(7),
            value: TaskConstant::Str("view".into()),
        },
    );
    rejected(&owned, "argument type or ownership");
}

#[test]
fn native_task_start_await_ir_slot_63_and_new_argument_work_keep_existing_limits() {
    let mut root = spawning_function(0, 9);
    root.slots = (0..64).map(|_| value(IrType::Int)).collect();
    root.slots[0] = value(result(IrType::Null));
    root.slots[63] = task(IrType::Null);
    root.states[0] = state(vec![start(63, 9, &[]), null_result(0)], done(0));
    let end = TaskFunction {
        id: TaskFunctionId(9),
        name: "End".into(),
        slots: vec![value(result(IrType::Null))],
        parameters: vec![],
        entry: StateId(0),
        result: result(IrType::Null),
        states: vec![state(vec![null_result(0)], done(0))],
    };
    let mut program = TaskProgram {
        functions: vec![root, end],
    };
    let plan = accepted(&program);
    assert_eq!(plan.functions[0].scope_task_mask, 1u64 << 63);
    assert!(plan.functions[0].slots.contains(&SlotId(63)));
    program.functions[0].slots.push(task(IrType::Null));
    rejected(&program, "state or slot limit");
    program = signature_program(IrType::Int, &[0, 0]);
    let cost = program
        .functions
        .iter()
        .flat_map(|f| &f.states)
        .flat_map(|s| &s.operations)
        .map(|op| {
            1 + if let TaskOp::Start { arguments, .. } = op {
                arguments.len()
            } else {
                0
            }
        })
        .sum();
    verify_and_plan(
        &program,
        TaskLimits {
            max_operations: cost,
            ..TaskLimits::default()
        },
    )
    .unwrap();
    assert!(verify_and_plan(
        &program,
        TaskLimits {
            max_operations: cost - 1,
            ..TaskLimits::default()
        }
    )
    .is_err());
    assert!(verify_and_plan(
        &program,
        TaskLimits {
            max_analysis_work: 10,
            ..TaskLimits::default()
        }
    )
    .is_err());
    program.functions[0].states[0].operations[0] = TaskOp::Start {
        dst: SlotId(1),
        function: TaskFunctionId(9),
        arguments: vec![SlotId(usize::MAX); 65],
    };
    rejected(&program, "Start argument limit");
}
