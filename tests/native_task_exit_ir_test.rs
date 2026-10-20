//! R5h.3a typed Exit planning only. These cases do not prove native execution,
//! Task handoff, runtime failure publication or allocator reclamation.
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

fn state(operations: Vec<TaskOp>, terminator: TaskTerminator) -> TaskState {
    TaskState {
        operations,
        terminator,
    }
}

fn init(dst: usize, value: TaskConstant) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value,
    }
}

fn exit(value: usize) -> TaskTerminator {
    TaskTerminator::Exit {
        value: SlotId(value),
    }
}

fn program(
    slots: Vec<TaskSlot>,
    parameters: &[usize],
    states: Vec<TaskState>,
    output: IrType,
) -> TaskProgram {
    TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "ExitFixture".into(),
            slots,
            parameters: parameters.iter().copied().map(SlotId).collect(),
            entry: StateId(0),
            states,
            result: output,
        }],
    }
}

fn minimal() -> TaskProgram {
    program(
        vec![value(result(IrType::Null))],
        &[0],
        vec![state(vec![], exit(0))],
        result(IrType::Null),
    )
}

fn accepted(program: &TaskProgram) -> TaskFramePlan {
    verify_and_plan(program, TaskLimits::default())
        .unwrap_or_else(|error| panic!("valid Exit IR rejected: {error}\n{program:?}"))
}

fn rejected(program: &TaskProgram, message: &str) {
    let error = verify_and_plan(program, TaskLimits::default()).unwrap_err();
    assert!(
        error.message.contains(message),
        "expected {message:?}, got {error}\n{program:?}"
    );
}

#[test]
fn native_task_exit_ir_stages_all_supported_results_and_preserves_owned_without_tasks() {
    for (ty, constant) in [
        (IrType::Int, TaskConstant::Int(7)),
        (IrType::Bool, TaskConstant::Bool(true)),
        (IrType::Null, TaskConstant::Null),
        (IrType::Str, TaskConstant::Str("value".into())),
    ] {
        let output = result(ty);
        for returned in [
            TaskConstant::Ok(Box::new(constant)),
            TaskConstant::Err {
                result: output.clone(),
                domain: "domain".into(),
                code: "code".into(),
                message: "message".into(),
            },
        ] {
            let fixture = program(
                vec![
                    value(output.clone()),
                    value(IrType::Str),
                    value(result(IrType::Int)),
                    value(IrType::Int),
                ],
                &[],
                vec![state(
                    vec![
                        init(0, returned),
                        init(1, TaskConstant::Str("retained".into())),
                        init(
                            2,
                            TaskConstant::Err {
                                result: result(IrType::Int),
                                domain: "retained-domain".into(),
                                code: "retained-code".into(),
                                message: "retained-message".into(),
                            },
                        ),
                        init(3, TaskConstant::Int(19)),
                    ],
                    exit(0),
                )],
                output.clone(),
            );
            let plan = accepted(&fixture);
            let frame = &plan.functions[0];
            assert!(frame.exit_bridge && frame.hosted);
            assert_eq!(frame.scope_task_mask, 0);
            // All Result types are Owned because their Err contains KuStrings.
            // The dead Copy local is not promoted into persistent storage.
            assert_eq!(frame.slots, vec![SlotId(0), SlotId(1), SlotId(2)]);
            assert!(frame.suspensions.is_empty());
        }
    }
}

#[test]
fn native_task_exit_ir_keeps_conditional_owners_and_sparse_slot_63() {
    let mut fixture = program(
        vec![
            value(IrType::Bool),
            value(result(IrType::Null)),
            value(IrType::Str),
        ],
        &[0, 1],
        vec![
            state(
                vec![],
                TaskTerminator::Branch {
                    condition: SlotId(0),
                    then_state: StateId(1),
                    else_state: StateId(2),
                },
            ),
            state(
                vec![init(2, TaskConstant::Str("conditional".into()))],
                TaskTerminator::Jump { target: StateId(2) },
            ),
            state(vec![], exit(1)),
        ],
        result(IrType::Null),
    );
    let frame = &accepted(&fixture).functions[0];
    assert!(frame.exit_bridge && frame.hosted);
    assert_eq!(frame.slots, vec![SlotId(0), SlotId(1), SlotId(2)]);
    // Legacy Complete's mandatory explicit cleanup check is not relaxed.
    fixture.functions[0].states[2].terminator = TaskTerminator::Complete { value: SlotId(1) };
    rejected(&fixture, "completion leaves owned slots without cleanup");

    let mut slots = vec![value(IrType::Int); 64];
    slots[0] = value(IrType::Str);
    slots[63] = value(result(IrType::Str));
    let fixture = program(
        slots,
        &[0, 63],
        vec![state(vec![], exit(63))],
        result(IrType::Str),
    );
    let plan = accepted(&fixture);
    assert!(plan.functions[0].exit_bridge && plan.functions[0].hosted);
    assert_eq!(plan.functions[0].scope_task_mask, 0);
    assert_eq!(plan.functions[0].slots, vec![SlotId(0), SlotId(63)]);
}

#[test]
fn native_task_exit_ir_retains_real_start_owner_for_required_handoff() {
    let mut fixture = program(
        vec![
            value(result(IrType::Null)),
            value(IrType::Str),
            TaskSlot {
                ty: TaskSlotType::Task {
                    result: result(IrType::Null),
                },
            },
        ],
        &[0, 1],
        vec![state(
            vec![TaskOp::Start {
                dst: SlotId(2),
                function: TaskFunctionId(1),
                arguments: vec![],
            }],
            exit(0),
        )],
        result(IrType::Null),
    );
    let mut child = minimal().functions.remove(0);
    child.id = TaskFunctionId(1);
    child.name = "LegacyChild".into();
    child.parameters.clear();
    child.states[0] = state(
        vec![init(0, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
        TaskTerminator::Complete { value: SlotId(0) },
    );
    fixture.functions.push(child);

    let plan = accepted(&fixture);
    assert!(plan.functions[0].exit_bridge && plan.functions[0].hosted);
    assert_eq!(plan.functions[0].scope_task_mask, 1u64 << 2);
    assert_eq!(
        plan.functions[0].slots,
        vec![SlotId(0), SlotId(1), SlotId(2)]
    );
    // Mode is per function: a separately verified legacy callee remains valid.
    assert!(!plan.functions[1].exit_bridge && !plan.functions[1].hosted);
}

#[test]
fn native_task_exit_ir_requires_one_matching_initialized_owned_result() {
    let mut fixture = minimal();
    fixture.functions[0].parameters.clear();
    rejected(&fixture, "Exit reads an uninitialized Result");

    let mut fixture = minimal();
    fixture.functions[0].states[0].terminator = exit(usize::MAX);
    rejected(&fixture, "unknown slot");

    let mut fixture = minimal();
    fixture.functions[0].slots[0] = value(IrType::Null);
    rejected(&fixture, "Exit must move the matching owned Result");

    let mut fixture = minimal();
    fixture.functions[0].slots[0] = value(result(IrType::Int));
    rejected(&fixture, "Exit must move the matching owned Result");

    let mut fixture = minimal();
    fixture.functions[0].slots.push(value(result(IrType::Null)));
    fixture.functions[0].states[0].operations = vec![TaskOp::Move {
        dst: SlotId(1),
        src: SlotId(0),
    }];
    rejected(&fixture, "Exit reads an uninitialized Result");
    fixture.functions[0].states[0].terminator = exit(1);
    accepted(&fixture);
    fixture.functions[0].states[0]
        .operations
        .push(TaskOp::Read { slot: SlotId(0) });
    rejected(&fixture, "not definitely initialized");
}

#[test]
fn native_task_exit_ir_rejects_owned_result_borrows_and_initialized_borrow_boundaries() {
    let mut fixture = minimal();
    fixture.functions[0].parameters.clear();
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: result(IrType::Null),
        borrowed: true,
    };
    fixture.functions[0].states[0].operations =
        vec![init(0, TaskConstant::Ok(Box::new(TaskConstant::Null)))];
    rejected(&fixture, "Exit must move the matching owned Result");

    let mut fixture = minimal();
    fixture.functions[0].slots.push(TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Str,
            borrowed: true,
        },
    });
    // Merely declaring unused borrow storage does not put it into the frame.
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].slots, vec![SlotId(0)]);
    fixture.functions[0].states[0].operations = vec![init(1, TaskConstant::Str("borrowed".into()))];
    rejected(&fixture, "borrowed value cannot cross Exit boundary");
}

#[test]
fn native_task_exit_ir_try_result_moves_the_complete_result_on_both_edges() {
    let fixture = program(
        vec![
            value(result(IrType::Str)),
            value(IrType::Str),
            value(IrType::Str),
            value(result(IrType::Null)),
            value(result(IrType::Null)),
        ],
        &[0, 1],
        vec![
            state(
                vec![],
                TaskTerminator::TryResult {
                    src: SlotId(0),
                    ok_value: SlotId(2),
                    err_result: SlotId(3),
                    ok: StateId(1),
                    err: StateId(2),
                },
            ),
            state(
                vec![init(4, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
                exit(4),
            ),
            state(vec![], exit(3)),
        ],
        result(IrType::Null),
    );
    let plan = accepted(&fixture);
    assert_eq!(
        plan.functions[0].slots,
        vec![SlotId(0), SlotId(1), SlotId(2), SlotId(3), SlotId(4)]
    );
    for branch in [1, 2] {
        let mut invalid = fixture.clone();
        invalid.functions[0].states[branch]
            .operations
            .push(TaskOp::Read { slot: SlotId(0) });
        rejected(&invalid, "not definitely initialized");
    }
}

#[test]
fn native_task_exit_ir_rejects_legacy_completion_cleanup_exit_and_unsuspended_cycles() {
    let mut fixture = minimal();
    fixture.functions[0]
        .states
        .push(state(vec![], TaskTerminator::Complete { value: SlotId(0) }));
    rejected(&fixture, "Exit bridge cannot contain legacy Complete");
    // Reachability does not permit a caller to hide a mode mismatch.
    fixture.functions[0].states.swap(0, 1);
    rejected(&fixture, "Exit bridge cannot contain legacy Complete");

    let mut fixture = minimal();
    fixture.functions[0].states = vec![
        state(
            vec![],
            TaskTerminator::Suspend {
                resume: StateId(1),
                cleanup: StateId(2),
            },
        ),
        state(vec![], exit(0)),
        state(vec![], exit(0)),
    ];
    rejected(&fixture, "cleanup cannot stage a normal Exit");

    let fixture = program(
        vec![value(IrType::Bool), value(result(IrType::Null))],
        &[0, 1],
        vec![
            state(
                vec![],
                TaskTerminator::Branch {
                    condition: SlotId(0),
                    then_state: StateId(0),
                    else_state: StateId(1),
                },
            ),
            state(vec![], exit(1)),
        ],
        result(IrType::Null),
    );
    rejected(&fixture, "cycle without suspension");
}

#[test]
fn native_task_exit_ir_preserves_bounded_planning_exact_limits() {
    let fixture = minimal();
    // Single parameter, one Exit, no TaskOp. Existing charged passes:
    // ids 1 + shape 1 + Start graph 2 + regions 2 + init 1 + live 2 + plan 1.
    let exact = TaskLimits {
        max_functions: 1,
        max_states: 1,
        max_slots: 1,
        max_operations: 0,
        max_literal_bytes: "ExitFixture".len(),
        max_analysis_work: 10,
    };
    let plan = verify_and_plan(&fixture, exact).unwrap();
    assert!(plan.functions[0].exit_bridge && plan.functions[0].hosted);
    assert_eq!(plan.functions[0].slots, vec![SlotId(0)]);
    let one_less = TaskLimits {
        max_analysis_work: 9,
        ..exact
    };
    let error = verify_and_plan(&fixture, one_less).unwrap_err();
    assert!(error.message.contains("analysis work limit exceeded"));
    for below in [
        TaskLimits {
            max_states: 0,
            ..exact
        },
        TaskLimits {
            max_slots: 0,
            ..exact
        },
        TaskLimits {
            max_literal_bytes: "ExitFixture".len() - 1,
            ..exact
        },
    ] {
        assert!(verify_and_plan(&fixture, below).is_err());
    }

    let mut legacy = fixture;
    legacy.functions[0].states[0].terminator = TaskTerminator::Complete { value: SlotId(0) };
    let plan = verify_and_plan(&legacy, exact).unwrap();
    assert!(!plan.functions[0].exit_bridge && !plan.functions[0].hosted);
}
