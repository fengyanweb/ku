use ku::ir::{
    task::{
        verify_and_plan, SlotId, StateId, TaskConstant, TaskFramePlan, TaskFunction,
        TaskFunctionId, TaskLimits, TaskOp, TaskProgram, TaskSlot, TaskSlotType, TaskState,
        TaskTerminator,
    },
    IrType,
};

fn result(ty: IrType) -> IrType {
    IrType::Result(Box::new(ty))
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

fn read(slot: usize) -> TaskOp {
    TaskOp::Read { slot: SlotId(slot) }
}

fn drop_slot(slot: usize) -> TaskOp {
    TaskOp::Drop { slot: SlotId(slot) }
}

fn drop_if(slot: usize) -> TaskOp {
    TaskOp::DropIfInit { slot: SlotId(slot) }
}

fn jump(target: usize) -> TaskTerminator {
    TaskTerminator::Jump {
        target: StateId(target),
    }
}

fn complete(value: usize) -> TaskTerminator {
    TaskTerminator::Complete {
        value: SlotId(value),
    }
}

fn suspend(resume: usize, cleanup: usize) -> TaskTerminator {
    TaskTerminator::Suspend {
        resume: StateId(resume),
        cleanup: StateId(cleanup),
    }
}

fn branch(condition: usize, then_state: usize, else_state: usize) -> TaskTerminator {
    TaskTerminator::Branch {
        condition: SlotId(condition),
        then_state: StateId(then_state),
        else_state: StateId(else_state),
    }
}

fn program(types: Vec<IrType>, parameters: &[usize], states: Vec<TaskState>) -> TaskProgram {
    TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "Fixture".into(),
            slots: types
                .into_iter()
                .map(|ty| TaskSlot {
                    ty: TaskSlotType::Value {
                        ty,
                        borrowed: false,
                    },
                })
                .collect(),
            parameters: parameters.iter().copied().map(SlotId).collect(),
            entry: StateId(0),
            states,
            result: result(IrType::Null),
        }],
    }
}

fn minimal() -> TaskProgram {
    program(
        vec![result(IrType::Null)],
        &[],
        vec![state(
            vec![init(0, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
            complete(0),
        )],
    )
}

fn accepted(program: &TaskProgram) -> TaskFramePlan {
    verify_and_plan(program, TaskLimits::default())
        .unwrap_or_else(|error| panic!("valid R1 frame rejected: {error}\n{program:?}"))
}

fn rejected(program: &TaskProgram, case: &str) {
    assert!(
        verify_and_plan(program, TaskLimits::default()).is_err(),
        "accepted invalid frame: {case}\n{program:?}"
    );
}

#[test]
fn native_task_frame_ir_accepts_exact_r1_primitive_results() {
    for (ty, constant) in [
        (IrType::Int, TaskConstant::Int(7)),
        (IrType::Bool, TaskConstant::Bool(true)),
        (IrType::Null, TaskConstant::Null),
        (IrType::Str, TaskConstant::Str("中\0😀".into())),
    ] {
        let mut fixture = program(
            vec![result(ty.clone())],
            &[],
            vec![state(
                vec![init(0, TaskConstant::Ok(Box::new(constant)))],
                complete(0),
            )],
        );
        fixture.functions[0].result = result(ty.clone());
        accepted(&fixture);
        fixture.functions[0].states[0].operations = vec![init(
            0,
            TaskConstant::Err {
                result: result(ty),
                domain: "fixture".into(),
                code: "failure".into(),
                message: "owned error".into(),
            },
        )];
        accepted(&fixture);
    }
    assert!(accepted(&TaskProgram { functions: vec![] })
        .functions
        .is_empty());
}

#[test]
fn native_task_frame_ir_rejects_unknown_ids_without_index_allocation() {
    accepted(&minimal());
    let mut fixture = minimal();
    fixture.functions[0].entry = StateId(usize::MAX);
    rejected(&fixture, "invalid entry");
    fixture = minimal();
    fixture.functions[0].states[0].terminator = jump(usize::MAX);
    rejected(&fixture, "invalid branch target");
    fixture = minimal();
    fixture.functions[0].states[0]
        .operations
        .insert(0, read(usize::MAX));
    rejected(&fixture, "invalid slot read");
    fixture = minimal();
    fixture.functions[0].states[0].terminator = complete(usize::MAX);
    rejected(&fixture, "invalid completion slot");
    fixture = minimal();
    fixture.functions[0].parameters = vec![SlotId(usize::MAX)];
    rejected(&fixture, "invalid parameter slot");
    fixture = minimal();
    fixture.functions.push(fixture.functions[0].clone());
    rejected(&fixture, "duplicate function identity");
    fixture.functions[1].id = TaskFunctionId(7);
    fixture.functions[1].name = "Second".into();
    accepted(&fixture);
}

#[test]
fn native_task_frame_ir_parameters_are_unique_owned_initialized_slots() {
    let mut fixture = program(
        vec![result(IrType::Null)],
        &[0],
        vec![state(vec![], complete(0))],
    );
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].slots, vec![SlotId(0)]);
    fixture.functions[0].parameters.push(SlotId(0));
    rejected(&fixture, "duplicate owning parameter");
    fixture.functions[0].parameters.pop();
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: result(IrType::Null),
        borrowed: true,
    };
    rejected(&fixture, "borrowed async parameter");
    fixture.functions[0].parameters.clear();
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: result(IrType::Null),
        borrowed: false,
    };
    rejected(&fixture, "uninitialized completion result");
}

#[test]
fn native_task_frame_ir_rejects_types_outside_the_r1_contract() {
    for unsupported in [
        IrType::Float,
        IrType::Unknown,
        IrType::Void,
        IrType::Function,
        IrType::Array(Box::new(IrType::Int)),
        IrType::Named("NotAFrameType".into()),
        IrType::Cell(Box::new(IrType::Int)),
        result(result(IrType::Int)),
        IrType::Closure {
            params: vec![],
            param_modes: vec![],
            ret: Box::new(IrType::Int),
        },
    ] {
        let mut fixture = minimal();
        fixture.functions[0].slots.push(TaskSlot {
            ty: TaskSlotType::Value {
                ty: unsupported,
                borrowed: false,
            },
        });
        rejected(&fixture, "unsupported slot type, even when unused");
    }
    let mut fixture = minimal();
    fixture.functions[0].result = IrType::Null;
    rejected(&fixture, "function result must be Result primitive");
}

#[test]
fn native_task_frame_ir_rejects_mistyped_constants_branches_and_results() {
    let mut fixture = minimal();
    fixture.functions[0].states[0].operations = vec![init(0, TaskConstant::Int(7))];
    rejected(&fixture, "constant does not match result slot");
    fixture.functions[0].states[0].operations = vec![init(
        0,
        TaskConstant::Err {
            result: result(IrType::Int),
            domain: "d".into(),
            code: "c".into(),
            message: "m".into(),
        },
    )];
    rejected(&fixture, "Err annotation differs from destination Result");
    fixture = program(
        vec![IrType::Int, result(IrType::Null)],
        &[0],
        vec![
            state(vec![], branch(0, 1, 1)),
            state(
                vec![init(1, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
                complete(1),
            ),
        ],
    );
    rejected(&fixture, "branch condition is not bool");
    fixture = program(vec![IrType::Int], &[0], vec![state(vec![], complete(0))]);
    rejected(
        &fixture,
        "completion payload does not match function result",
    );
}

#[test]
fn native_task_frame_ir_copy_is_scalar_and_move_transfers_owned_initialization() {
    let scalar = program(
        vec![IrType::Int, IrType::Int, result(IrType::Null)],
        &[],
        vec![state(
            vec![
                init(0, TaskConstant::Int(7)),
                TaskOp::Copy {
                    dst: SlotId(1),
                    src: SlotId(0),
                },
                read(0),
                read(1),
                init(2, TaskConstant::Ok(Box::new(TaskConstant::Null))),
            ],
            complete(2),
        )],
    );
    accepted(&scalar);
    let mut fixture = program(
        vec![IrType::Str, IrType::Str, result(IrType::Null)],
        &[],
        vec![state(
            vec![
                init(0, TaskConstant::Str("owned".into())),
                TaskOp::Move {
                    dst: SlotId(1),
                    src: SlotId(0),
                },
                read(1),
                drop_slot(1),
                init(2, TaskConstant::Ok(Box::new(TaskConstant::Null))),
            ],
            complete(2),
        )],
    );
    accepted(&fixture);
    fixture.functions[0].states[0].operations.insert(2, read(0));
    rejected(&fixture, "moved source is not readable");
    fixture.functions[0].states[0].operations.remove(2);
    fixture.functions[0].states[0].operations[1] = TaskOp::Copy {
        dst: SlotId(1),
        src: SlotId(0),
    };
    fixture.functions[0].states[0]
        .operations
        .insert(2, drop_slot(0));
    rejected(&fixture, "owned copy cannot duplicate allocation");
}

fn branch_owned() -> TaskProgram {
    program(
        vec![IrType::Bool, IrType::Str, IrType::Str, result(IrType::Null)],
        &[0],
        vec![
            state(
                vec![init(1, TaskConstant::Str("owned".into()))],
                branch(0, 1, 2),
            ),
            state(
                vec![TaskOp::Move {
                    dst: SlotId(2),
                    src: SlotId(1),
                }],
                jump(3),
            ),
            state(vec![], jump(3)),
            state(
                vec![
                    drop_if(1),
                    drop_if(2),
                    init(3, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(3),
            ),
        ],
    )
}

#[test]
fn native_task_frame_ir_branch_join_keeps_may_and_must_initialization_distinct() {
    let mut fixture = branch_owned();
    accepted(&fixture);
    fixture.functions[0].states[3].operations.insert(0, read(1));
    rejected(&fixture, "source moved on only one incoming path");
    fixture.functions[0].states[3].operations[0] = read(2);
    rejected(
        &fixture,
        "destination initialized on only one incoming path",
    );
    fixture.functions[0].states[3].operations[0] = drop_slot(1);
    rejected(&fixture, "unconditional drop of maybe-initialized owner");
}

#[test]
fn native_task_frame_ir_reinitialization_cannot_overwrite_a_maybe_owned_payload() {
    let mut fixture = branch_owned();
    fixture.functions[0].states[3]
        .operations
        .insert(0, init(1, TaskConstant::Str("replacement".into())));
    rejected(&fixture, "overwriting owner still live on another path");
    fixture.functions[0].states[3]
        .operations
        .insert(0, drop_if(1));
    accepted(&fixture);
}

fn suspended_loop(reinitialize: bool) -> TaskProgram {
    let mut again = vec![
        TaskOp::Move {
            dst: SlotId(1),
            src: SlotId(0),
        },
        drop_slot(1),
    ];
    if reinitialize {
        again.push(init(0, TaskConstant::Str("next iteration".into())));
    }
    program(
        vec![IrType::Str, IrType::Str],
        &[0],
        vec![
            state(vec![read(0)], suspend(1, 2)),
            state(again, jump(0)),
            state(vec![drop_if(0), drop_if(1)], TaskTerminator::Terminate),
        ],
    )
}

#[test]
fn native_task_frame_ir_loop_fixpoint_rejects_second_iteration_use_after_move() {
    accepted(&suspended_loop(true));
    rejected(
        &suspended_loop(false),
        "backedge reaches read after previous iteration moved owner",
    );
}

#[test]
fn native_task_frame_ir_requires_suspend_on_every_normal_cycle() {
    rejected(
        &program(vec![], &[], vec![state(vec![], jump(0))]),
        "unsuspendable self loop",
    );
    let fixture = program(
        vec![IrType::Bool, result(IrType::Null)],
        &[0],
        vec![
            state(vec![], suspend(1, 4)),
            state(vec![], branch(0, 2, 3)),
            state(vec![], jump(1)),
            state(
                vec![init(1, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
                complete(1),
            ),
            state(vec![], TaskTerminator::Terminate),
        ],
    );
    rejected(
        &fixture,
        "an earlier Suspend does not guard a later inner cycle",
    );
}

fn live_across_suspend() -> TaskProgram {
    program(
        vec![IrType::Str, IrType::Int, IrType::Int, result(IrType::Null)],
        &[],
        vec![
            state(
                vec![
                    init(0, TaskConstant::Str("cleanup owner".into())),
                    init(1, TaskConstant::Int(7)),
                    init(2, TaskConstant::Int(99)),
                ],
                suspend(1, 2),
            ),
            state(
                vec![
                    drop_slot(0),
                    init(3, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(3),
            ),
            state(
                vec![read(0), read(1), drop_slot(0)],
                TaskTerminator::Terminate,
            ),
        ],
    )
}

#[test]
fn native_task_frame_ir_cleanup_liveness_is_included_and_dead_copy_is_not_spilled() {
    let fixture = live_across_suspend();
    let first = accepted(&fixture);
    let second = accepted(&fixture);
    assert_eq!(first.functions[0].function, TaskFunctionId(0));
    assert_eq!(first.functions[0].slots, vec![SlotId(0), SlotId(1)]);
    assert_eq!(first.functions[0].slots, second.functions[0].slots);
    assert_eq!(first.functions[0].suspensions.len(), 1);
    assert_eq!(first.functions[0].suspensions[0].state, StateId(0));
    assert_eq!(
        first.functions[0].suspensions[0].slots,
        vec![SlotId(0), SlotId(1)]
    );
}

#[test]
fn native_task_frame_ir_dead_owned_values_require_explicit_cleanup() {
    let mut fixture = live_across_suspend();
    fixture.functions[0].states[1].operations.remove(0);
    fixture.functions[0].states[2].operations.clear();
    rejected(
        &fixture,
        "initialized owned payload would disappear at suspension",
    );
    fixture.functions[0].states[0].operations.push(drop_slot(0));
    let plan = accepted(&fixture);
    assert!(plan.functions[0].slots.is_empty());
}

#[test]
fn native_task_frame_ir_borrowed_projections_cannot_become_frame_fields() {
    let mut fixture = program(
        vec![IrType::Int, result(IrType::Null)],
        &[],
        vec![
            state(vec![init(0, TaskConstant::Int(7))], suspend(1, 2)),
            state(
                vec![
                    read(0),
                    init(1, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(1),
            ),
            state(vec![], TaskTerminator::Terminate),
        ],
    );
    accepted(&fixture);
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: true,
    };
    rejected(&fixture, "borrowed Copy slot itself crosses Suspend");
    fixture.functions[0].states[0].operations.push(drop_slot(0));
    rejected(&fixture, "borrowed storage cannot be explicitly dropped");
}

#[test]
fn native_task_frame_ir_cleanup_cannot_complete_suspend_reenter_normal_or_loop() {
    let mut fixture = live_across_suspend();
    accepted(&fixture);
    fixture.functions[0].states[2]
        .operations
        .push(init(3, TaskConstant::Ok(Box::new(TaskConstant::Null))));
    fixture.functions[0].states[2].terminator = complete(3);
    rejected(&fixture, "cleanup replaces cancellation with success");
    fixture = live_across_suspend();
    fixture.functions[0].states[2].terminator = suspend(1, 2);
    rejected(&fixture, "cleanup starts another suspension");
    fixture = live_across_suspend();
    fixture.functions[0].states[2].terminator = jump(1);
    rejected(&fixture, "cleanup joins normal continuation");
    fixture = live_across_suspend();
    fixture.functions[0].states[2].operations.clear();
    fixture.functions[0].states[2].terminator = jump(2);
    rejected(&fixture, "cleanup has an unbounded cycle");
    rejected(
        &program(vec![], &[], vec![state(vec![], TaskTerminator::Terminate)]),
        "normal execution cannot invent a cancellation context",
    );
}

#[test]
fn native_task_frame_ir_shared_cleanup_preserves_partial_initialization() {
    let mut fixture = program(
        vec![IrType::Bool, IrType::Str, result(IrType::Null)],
        &[0],
        vec![
            state(vec![], branch(0, 1, 2)),
            state(
                vec![init(1, TaskConstant::Str("branch-owned".into()))],
                suspend(3, 4),
            ),
            state(vec![], suspend(3, 4)),
            state(
                vec![
                    drop_if(1),
                    init(2, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(2),
            ),
            state(vec![drop_if(1)], TaskTerminator::Terminate),
        ],
    );
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].slots, vec![SlotId(0), SlotId(1)]);
    assert_eq!(plan.functions[0].suspensions[0].slots, vec![SlotId(1)]);
    assert!(plan.functions[0].suspensions[1].slots.is_empty());
    fixture.functions[0].states[4].operations[0] = drop_slot(1);
    rejected(&fixture, "cleanup owner exists at only one suspension");
    fixture.functions[0].states[4].operations = vec![drop_if(1), drop_slot(1)];
    rejected(&fixture, "cleanup cannot drop the same payload twice");
}

#[test]
fn native_task_frame_ir_borrowed_copy_must_be_materialized_before_suspension() {
    let mut fixture = program(
        vec![IrType::Int, IrType::Int, result(IrType::Null)],
        &[],
        vec![
            state(
                vec![
                    init(0, TaskConstant::Int(7)),
                    TaskOp::Copy {
                        dst: SlotId(1),
                        src: SlotId(0),
                    },
                ],
                suspend(1, 2),
            ),
            state(
                vec![
                    read(1),
                    init(2, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(2),
            ),
            state(vec![], TaskTerminator::Terminate),
        ],
    );
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: true,
    };
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].slots, vec![SlotId(1)]);
    fixture.functions[0].states[1].operations[0] = read(0);
    rejected(
        &fixture,
        "reading original view still requires an invalid frame borrow",
    );
}

#[test]
fn native_task_frame_ir_budget_boundaries_fail_closed() {
    let fixture = minimal();
    for limits in [
        TaskLimits {
            max_functions: 0,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_states: 0,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_slots: 0,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_operations: 0,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_analysis_work: 0,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_states: 257,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_slots: 65,
            ..TaskLimits::default()
        },
        TaskLimits {
            max_operations: 4097,
            ..TaskLimits::default()
        },
    ] {
        assert!(
            verify_and_plan(&fixture, limits).is_err(),
            "invalid/tight resource limit must reject"
        );
    }
    verify_and_plan(
        &fixture,
        TaskLimits {
            max_functions: 1,
            max_states: 1,
            max_slots: 1,
            max_operations: 1,
            ..TaskLimits::default()
        },
    )
    .unwrap();
}

#[test]
fn native_task_frame_ir_literal_budget_counts_utf8_bytes_and_all_error_fields() {
    let mut fixture = program(
        vec![result(IrType::Str)],
        &[],
        vec![state(
            vec![init(
                0,
                TaskConstant::Ok(Box::new(TaskConstant::Str("中😀".into()))),
            )],
            complete(0),
        )],
    );
    fixture.functions[0].result = result(IrType::Str);
    let name_bytes = fixture.functions[0].name.len();
    verify_and_plan(
        &fixture,
        TaskLimits {
            max_literal_bytes: name_bytes + 7,
            ..TaskLimits::default()
        },
    )
    .unwrap();
    assert!(verify_and_plan(
        &fixture,
        TaskLimits {
            max_literal_bytes: name_bytes + 6,
            ..TaskLimits::default()
        }
    )
    .is_err());
    fixture.functions[0].states[0].operations = vec![init(
        0,
        TaskConstant::Err {
            result: result(IrType::Str),
            domain: "中".into(),
            code: "x".into(),
            message: "😀".into(),
        },
    )];
    verify_and_plan(
        &fixture,
        TaskLimits {
            max_literal_bytes: name_bytes + 8,
            ..TaskLimits::default()
        },
    )
    .unwrap();
    assert!(verify_and_plan(
        &fixture,
        TaskLimits {
            max_literal_bytes: name_bytes + 7,
            ..TaskLimits::default()
        }
    )
    .is_err());
}

#[test]
fn native_task_frame_ir_dense_slot_and_state_hard_limits_are_inclusive() {
    let mut types = vec![IrType::Int; 63];
    types.push(result(IrType::Null));
    let mut fixture = program(types, &[63], vec![state(vec![], complete(63))]);
    assert_eq!(accepted(&fixture).functions[0].slots, vec![SlotId(63)]);
    fixture.functions[0].slots.push(TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Int,
            borrowed: false,
        },
    });
    rejected(&fixture, "slot 64 exceeds the fixed 64-bit analysis domain");

    let mut states: Vec<_> = (0..255).map(|id| state(vec![], jump(id + 1))).collect();
    states.push(state(
        vec![init(0, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
        complete(0),
    ));
    fixture = program(vec![result(IrType::Null)], &[], states);
    accepted(&fixture);
    fixture.functions[0].states[255].terminator = jump(256);
    fixture.functions[0].states.push(state(vec![], complete(0)));
    rejected(&fixture, "state 256 exceeds the fixed state budget");
}

#[test]
fn native_task_frame_ir_operation_budget_is_aggregate_across_functions() {
    let mut fixture = minimal();
    let mut second = fixture.functions[0].clone();
    second.id = TaskFunctionId(7);
    second.name = "Second".into();
    fixture.functions.push(second);
    verify_and_plan(
        &fixture,
        TaskLimits {
            max_operations: 2,
            ..TaskLimits::default()
        },
    )
    .unwrap();
    assert!(verify_and_plan(
        &fixture,
        TaskLimits {
            max_operations: 1,
            ..TaskLimits::default()
        }
    )
    .is_err());
}
