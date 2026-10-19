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
fn native_task_frame_ir_owns_bare_catch_errors_and_validates_projections() {
    let error = IrType::Named("__ku_error_type".into());
    let fixture = program(
        vec![IrType::Str, error, IrType::Str, result(IrType::Null)],
        &[],
        vec![state(
            vec![
                init(0, TaskConstant::Str("中文😀".into())),
                TaskOp::MakeError {
                    dst: SlotId(1),
                    message: SlotId(0),
                },
                TaskOp::ErrorField {
                    dst: SlotId(2),
                    src: SlotId(1),
                    field: "message".into(),
                },
                drop_slot(2),
                TaskOp::WrapError {
                    dst: SlotId(3),
                    src: SlotId(1),
                },
            ],
            TaskTerminator::Exit { value: SlotId(3) },
        )],
    );
    let plan = accepted(&fixture);
    assert!(plan.functions[0].slots.contains(&SlotId(1)));
    assert!(plan.functions[0].slots.contains(&SlotId(2)));

    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations[2] = TaskOp::ErrorField {
        dst: SlotId(2),
        src: SlotId(1),
        field: "unknown".into(),
    };
    rejected(&bad, "unknown Error field cannot reach C emission");
    let mut bad = fixture.clone();
    bad.functions[0].slots[2].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: false,
    };
    rejected(&bad, "Error field destination must own a string");
    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations[4] = TaskOp::WrapError {
        dst: SlotId(3),
        src: SlotId(0),
    };
    rejected(&bad, "a string cannot replace the bare Error input");
    let mut bad = fixture.clone();
    bad.functions[0].states[0]
        .operations
        .push(TaskOp::ErrorField {
            dst: SlotId(2),
            src: SlotId(1),
            field: "message".into(),
        });
    rejected(
        &bad,
        "projection cannot read an Error after it moved into Result",
    );
    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations.insert(
        2,
        TaskOp::MakeError {
            dst: SlotId(1),
            message: SlotId(0),
        },
    );
    rejected(
        &bad,
        "a consumed message and initialized Error cannot be reused",
    );
    let mut bad = fixture.clone();
    bad.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Str,
        borrowed: true,
    };
    rejected(&bad, "MakeError cannot consume a borrowed message");
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
        let plan = accepted(&fixture);
        assert!(!plan.functions[0].hosted);
        assert_eq!(plan.functions[0].scope_task_mask, 0);
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
fn native_task_frame_ir_wrap_ok_copies_scalars_and_moves_owned_strings_and_arrays() {
    for ty in [
        IrType::Int,
        IrType::Bool,
        IrType::Null,
        IrType::Str,
        IrType::Array(Box::new(IrType::Int)),
        IrType::Array(Box::new(IrType::Bool)),
        IrType::Array(Box::new(IrType::Str)),
    ] {
        let mut fixture = program(
            vec![ty.clone(), result(ty.clone())],
            &[0],
            vec![state(
                vec![TaskOp::WrapOk {
                    dst: SlotId(1),
                    src: SlotId(0),
                }],
                complete(1),
            )],
        );
        fixture.functions[0].result = result(ty.clone());
        accepted(&fixture);
        fixture.functions[0].states[0].operations.push(read(0));
        if matches!(ty, IrType::Str | IrType::Array(_)) {
            rejected(&fixture, "owned ok payload has moved from its source");
        } else {
            accepted(&fixture);
        }
        fixture.functions[0].states[0].operations.pop();
        fixture.functions[0].parameters.clear();
        rejected(&fixture, "ok cannot read an uninitialized payload");
        if matches!(ty, IrType::Array(_)) {
            fixture.functions[0].states[0].operations = vec![init(
                1,
                TaskConstant::Err {
                    result: result(ty.clone()),
                    domain: "array".into(),
                    code: "empty".into(),
                    message: "no array payload".into(),
                },
            )];
            accepted(&fixture);
            fixture.functions[0].states[0].operations.clear();
            fixture.functions[0].parameters.push(SlotId(1));
            accepted(&fixture);
            if let TaskSlotType::Value { borrowed, .. } = &mut fixture.functions[0].slots[1].ty {
                *borrowed = true;
            }
            rejected(
                &fixture,
                "Result<Array> parameters must own their active payload",
            );
        }
    }
}

#[test]
fn native_task_frame_ir_wrap_ok_rejects_type_borrow_and_overwrite_errors() {
    let mut fixture = program(
        vec![IrType::Str, result(IrType::Str)],
        &[0],
        vec![state(
            vec![TaskOp::WrapOk {
                dst: SlotId(1),
                src: SlotId(0),
            }],
            complete(1),
        )],
    );
    fixture.functions[0].result = result(IrType::Str);
    accepted(&fixture);
    fixture.functions[0].parameters.push(SlotId(1));
    rejected(&fixture, "ok must not overwrite an owned Result");
    fixture.functions[0].parameters.pop();
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: false,
    };
    rejected(&fixture, "ok payload type must match Result");
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: result(IrType::Str),
        borrowed: false,
    };
    rejected(&fixture, "ok must not add a nested Result layer");
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Str,
        borrowed: true,
    };
    fixture.functions[0].parameters.clear();
    fixture.functions[0].states[0]
        .operations
        .insert(0, init(0, TaskConstant::Str("borrow".into())));
    rejected(
        &fixture,
        "ok must not transfer borrowed owned storage to an owned Result",
    );
    fixture.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Str,
        borrowed: false,
    };
    fixture.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: result(IrType::Str),
        borrowed: true,
    };
    rejected(&fixture, "ok cannot construct into a borrowed destination");
    fixture.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: result(IrType::Str),
        borrowed: false,
    };
    fixture.functions[0].states[0].operations[1] = TaskOp::WrapOk {
        dst: SlotId(usize::MAX),
        src: SlotId(0),
    };
    rejected(&fixture, "ok unknown destination");
    fixture.functions[0].states[0].operations[1] = TaskOp::WrapOk {
        dst: SlotId(1),
        src: SlotId(usize::MAX),
    };
    rejected(&fixture, "ok unknown source");
}

#[test]
fn native_task_frame_ir_wrap_ok_updates_suspension_liveness_and_cleanup() {
    for ty in [
        IrType::Str,
        IrType::Array(Box::new(IrType::Int)),
        IrType::Array(Box::new(IrType::Bool)),
        IrType::Array(Box::new(IrType::Str)),
    ] {
        for wrap_before in [false, true] {
            let wrap = TaskOp::WrapOk {
                dst: SlotId(1),
                src: SlotId(0),
            };
            let mut fixture = program(
                vec![ty.clone(), result(ty.clone())],
                &[0],
                vec![
                    state(
                        if wrap_before {
                            vec![wrap.clone()]
                        } else {
                            vec![]
                        },
                        suspend(1, 2),
                    ),
                    state(if wrap_before { vec![] } else { vec![wrap] }, complete(1)),
                    state(vec![drop_if(1), drop_if(0)], TaskTerminator::Terminate),
                ],
            );
            fixture.functions[0].result = result(ty.clone());
            let plan = accepted(&fixture);
            assert_eq!(
                plan.functions[0].suspensions[0].slots,
                vec![SlotId(usize::from(wrap_before))]
            );
            fixture.functions[0].states[2].operations.clear();
            rejected(
                &fixture,
                "the active payload still needs cancellation cleanup",
            );
        }
    }
}

#[test]
fn native_task_frame_ir_owned_replacement_hosts_plain_complete_and_preserves_owned_slots() {
    let mut fixture = program(
        vec![IrType::Str, IrType::Str, result(IrType::Str)],
        &[0, 1],
        vec![state(
            vec![
                drop_if(0),
                TaskOp::Move {
                    dst: SlotId(0),
                    src: SlotId(1),
                },
                TaskOp::WrapOk {
                    dst: SlotId(2),
                    src: SlotId(0),
                },
            ],
            complete(2),
        )],
    );
    fixture.functions[0].result = result(IrType::Str);
    let plan = accepted(&fixture);
    assert!(plan.functions[0].hosted);
    assert_eq!(
        plan.functions[0].slots,
        vec![SlotId(0), SlotId(1), SlotId(2)],
        "replacement accounting needs every Owned field even without suspension or Exit"
    );
    fixture.functions[0].states[0].operations.remove(0);
    let error = verify_and_plan(&fixture, TaskLimits::default()).unwrap_err();
    assert!(
        error
            .message
            .contains("overwriting a possibly initialized owned slot without drop"),
        "{error}"
    );
}

#[test]
fn native_task_frame_ir_string_clone_hosts_plain_complete_and_preserves_owned_slots() {
    let mut fixture = program(
        vec![IrType::Str, IrType::Str, result(IrType::Str)],
        &[0],
        vec![state(
            vec![
                TaskOp::StringClone {
                    dst: SlotId(1),
                    src: SlotId(0),
                },
                read(0),
                drop_slot(0),
                TaskOp::WrapOk {
                    dst: SlotId(2),
                    src: SlotId(1),
                },
            ],
            complete(2),
        )],
    );
    fixture.functions[0].result = result(IrType::Str);
    let plan = accepted(&fixture);
    assert!(plan.functions[0].hosted && !plan.functions[0].exit_bridge);
    assert_eq!(plan.functions[0].scope_task_mask, 0);
    assert_eq!(
        plan.functions[0].slots,
        vec![SlotId(0), SlotId(1), SlotId(2)]
    );
    assert!(plan.functions[0].suspensions.is_empty());
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
        IrType::Array(Box::new(IrType::Float)),
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
    rejected(&fixture, "function result must be a supported Result");
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

#[test]
fn native_task_frame_ir_wrap_ok_checks_must_and_may_at_branch_joins() {
    let wrap = TaskOp::WrapOk {
        dst: SlotId(1),
        src: SlotId(0),
    };
    let mut moved = program(
        vec![IrType::Str, result(IrType::Str), IrType::Bool],
        &[0, 2],
        vec![
            state(vec![], branch(2, 1, 2)),
            state(vec![wrap.clone(), drop_slot(1)], jump(3)),
            state(vec![], jump(3)),
            state(
                vec![
                    drop_if(0),
                    init(
                        1,
                        TaskConstant::Ok(Box::new(TaskConstant::Str("joined".into()))),
                    ),
                ],
                complete(1),
            ),
        ],
    );
    moved.functions[0].result = result(IrType::Str);
    accepted(&moved);
    moved.functions[0].states[3].operations = vec![wrap.clone()];
    let error = verify_and_plan(&moved, TaskLimits::default()).unwrap_err();
    assert!(
        error.message.contains("not definitely initialized"),
        "wrapping an owned source moved on one branch must fail its must check: {error}"
    );

    let mut overwritten = program(
        vec![IrType::Int, result(IrType::Int), IrType::Bool],
        &[0, 2],
        vec![
            state(vec![], branch(2, 1, 2)),
            state(vec![wrap.clone()], jump(3)),
            state(vec![], jump(3)),
            state(vec![drop_if(1), wrap.clone()], complete(1)),
        ],
    );
    overwritten.functions[0].result = result(IrType::Int);
    accepted(&overwritten);
    overwritten.functions[0].states[3].operations = vec![wrap];
    let error = verify_and_plan(&overwritten, TaskLimits::default()).unwrap_err();
    assert!(
        error.message.contains("possibly initialized owned slot"),
        "a Result initialized on only one branch must still fail its may-overwrite check: {error}"
    );
}

#[test]
fn native_task_frame_ir_wrap_ok_copies_local_borrows_without_spilling_them() {
    for (ty, value) in [
        (IrType::Int, TaskConstant::Int(37)),
        (IrType::Bool, TaskConstant::Bool(true)),
        (IrType::Null, TaskConstant::Null),
    ] {
        let mut fixture = program(
            vec![ty.clone(), result(ty.clone())],
            &[],
            vec![
                state(
                    vec![
                        init(0, value),
                        TaskOp::WrapOk {
                            dst: SlotId(1),
                            src: SlotId(0),
                        },
                        read(0),
                    ],
                    suspend(1, 2),
                ),
                state(vec![], complete(1)),
                state(vec![drop_slot(1)], TaskTerminator::Terminate),
            ],
        );
        fixture.functions[0].slots[0].ty = TaskSlotType::Value {
            ty: ty.clone(),
            borrowed: true,
        };
        fixture.functions[0].result = result(ty);
        let plan = accepted(&fixture);
        assert_eq!(plan.functions[0].slots, vec![SlotId(1)]);
        assert_eq!(plan.functions[0].suspensions[0].slots, vec![SlotId(1)]);

        fixture.functions[0].states[1].operations.push(read(0));
        let error = verify_and_plan(&fixture, TaskLimits::default()).unwrap_err();
        assert!(
            error
                .message
                .contains("borrowed value cannot cross suspension"),
            "copying into Result must not permit a later use of the original borrow: {error}"
        );
    }
}

#[test]
fn native_task_frame_ir_wrap_ok_moves_stack_local_strings_before_suspension() {
    let mut fixture = program(
        vec![IrType::Str, IrType::Str, result(IrType::Str)],
        &[0],
        vec![
            state(
                vec![
                    TaskOp::Move {
                        dst: SlotId(1),
                        src: SlotId(0),
                    },
                    TaskOp::WrapOk {
                        dst: SlotId(2),
                        src: SlotId(1),
                    },
                ],
                suspend(1, 2),
            ),
            state(vec![], complete(2)),
            state(
                vec![drop_if(2), drop_if(1), drop_if(0)],
                TaskTerminator::Terminate,
            ),
        ],
    );
    fixture.functions[0].result = result(IrType::Str);
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].slots, vec![SlotId(0), SlotId(2)]);
    assert_eq!(plan.functions[0].suspensions[0].slots, vec![SlotId(2)]);
}

#[test]
fn native_task_timer_ir_persists_argument_and_initializes_only_ready_result() {
    let fixture = program(
        vec![IrType::Int, result(IrType::Null)],
        &[],
        vec![
            state(vec![init(0, TaskConstant::Int(1))], jump(1)),
            state(
                vec![],
                TaskTerminator::Timer {
                    millis: SlotId(0),
                    dst: SlotId(1),
                    ready: StateId(2),
                    cleanup: StateId(3),
                },
            ),
            state(vec![], complete(1)),
            state(vec![drop_if(1)], TaskTerminator::Terminate),
        ],
    );
    let plan = accepted(&fixture);
    assert!(plan.functions[0].hosted);
    assert!(
        plan.functions[0].slots.contains(&SlotId(0)),
        "Pending must retain the once-evaluated argument"
    );
    assert!(plan.functions[0].slots.contains(&SlotId(1)));
    assert!(plan.functions[0]
        .suspensions
        .iter()
        .any(|boundary| boundary.state == StateId(1)));

    let mut bad = fixture.clone();
    bad.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Bool,
        borrowed: false,
    };
    bad.functions[0].states[0].operations[0] = init(0, TaskConstant::Bool(true));
    rejected(&bad, "non-int milliseconds");
    bad = fixture.clone();
    bad.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: result(IrType::Int),
        borrowed: false,
    };
    rejected(&bad, "non-null Result destination");
    bad = fixture.clone();
    bad.functions[0].states[0].operations.clear();
    rejected(&bad, "uninitialized milliseconds");
    bad = fixture.clone();
    bad.functions[0].states[0]
        .operations
        .push(init(1, TaskConstant::Ok(Box::new(TaskConstant::Null))));
    rejected(&bad, "overwriting an existing Result");
    bad = fixture.clone();
    bad.functions[0].slots[0].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: true,
    };
    rejected(&bad, "borrowed milliseconds");
    bad = fixture.clone();
    bad.functions[0].states[2] = state(vec![drop_slot(1)], jump(1));
    rejected(&bad, "zero timer is not guaranteed progress in a raw cycle");
    bad = fixture.clone();
    bad.functions[0].states[3].terminator = TaskTerminator::Timer {
        millis: SlotId(0),
        dst: SlotId(1),
        ready: StateId(2),
        cleanup: StateId(3),
    };
    rejected(&bad, "cleanup cannot start another timer");
    bad = fixture.clone();
    bad.functions[0].states[1].terminator = TaskTerminator::Timer {
        millis: SlotId(0),
        dst: SlotId(1),
        ready: StateId(99),
        cleanup: StateId(3),
    };
    rejected(&bad, "missing ready state");
}

fn callable_fixture() -> TaskProgram {
    let value = |ty| TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    };
    let callable = || TaskSlot {
        ty: TaskSlotType::Callable {
            parameters: vec![IrType::Int],
            result: result(IrType::Int),
        },
    };
    let child = |id| TaskFunction {
        id: TaskFunctionId(id),
        name: format!("Child{id}"),
        slots: vec![value(IrType::Int), value(result(IrType::Int))],
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        states: vec![state(
            vec![TaskOp::WrapOk {
                dst: SlotId(1),
                src: SlotId(0),
            }],
            complete(1),
        )],
        result: result(IrType::Int),
    };
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "Caller".into(),
                slots: vec![
                    callable(),
                    callable(),
                    value(IrType::Int),
                    TaskSlot {
                        ty: TaskSlotType::Task {
                            result: result(IrType::Int),
                        },
                    },
                    value(result(IrType::Int)),
                    value(result(IrType::Null)),
                    value(IrType::Bool),
                ],
                parameters: vec![SlotId(6)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![
                    state(
                        vec![
                            TaskOp::InitCallable {
                                dst: SlotId(0),
                                function: TaskFunctionId(1),
                            },
                            TaskOp::CloneCallable {
                                dst: SlotId(1),
                                src: SlotId(0),
                            },
                            drop_slot(0),
                            init(2, TaskConstant::Int(7)),
                        ],
                        branch(6, 1, 2),
                    ),
                    state(
                        vec![
                            drop_slot(1),
                            TaskOp::InitCallable {
                                dst: SlotId(1),
                                function: TaskFunctionId(1),
                            },
                        ],
                        jump(3),
                    ),
                    state(
                        vec![
                            drop_slot(1),
                            TaskOp::InitCallable {
                                dst: SlotId(1),
                                function: TaskFunctionId(2),
                            },
                        ],
                        jump(3),
                    ),
                    state(
                        vec![
                            TaskOp::StartIndirect {
                                dst: SlotId(3),
                                callee: SlotId(1),
                                arguments: vec![SlotId(2)],
                            },
                            drop_slot(1),
                        ],
                        TaskTerminator::Await {
                            task: SlotId(3),
                            dst: SlotId(4),
                            ready: StateId(4),
                            cleanup: StateId(5),
                        },
                    ),
                    state(
                        vec![
                            drop_slot(4),
                            init(5, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                        ],
                        complete(5),
                    ),
                    state(
                        vec![drop_if(0), drop_if(1), drop_if(4), drop_if(5)],
                        TaskTerminator::Terminate,
                    ),
                ],
            },
            child(1),
            child(2),
        ],
    }
}

fn indirect_child(id: usize, target: usize) -> TaskFunction {
    let value = |ty| TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    };
    TaskFunction {
        id: TaskFunctionId(id),
        name: format!("Indirect{id}"),
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        result: result(IrType::Int),
        slots: vec![
            value(IrType::Int),
            TaskSlot {
                ty: TaskSlotType::Callable {
                    parameters: vec![IrType::Int],
                    result: result(IrType::Int),
                },
            },
            TaskSlot {
                ty: TaskSlotType::Task {
                    result: result(IrType::Int),
                },
            },
            value(result(IrType::Int)),
        ],
        states: vec![
            state(
                vec![
                    TaskOp::InitCallable {
                        dst: SlotId(1),
                        function: TaskFunctionId(target),
                    },
                    TaskOp::StartIndirect {
                        dst: SlotId(2),
                        callee: SlotId(1),
                        arguments: vec![SlotId(0)],
                    },
                    drop_slot(1),
                ],
                TaskTerminator::Await {
                    task: SlotId(2),
                    dst: SlotId(3),
                    ready: StateId(1),
                    cleanup: StateId(2),
                },
            ),
            state(vec![], complete(3)),
            state(vec![drop_if(1), drop_if(3)], TaskTerminator::Terminate),
        ],
    }
}

#[test]
fn native_task_callable_ir_tracks_exact_targets_and_rejects_invalid_ownership() {
    let fixture = callable_fixture();
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].indirect_calls.len(), 1);
    let call = &plan.functions[0].indirect_calls[0];
    assert_eq!((call.state, call.operation), (StateId(3), 0));
    assert_eq!(call.targets, [TaskFunctionId(1), TaskFunctionId(2)]);
    assert!(plan.functions[0].slots.contains(&SlotId(0)));
    assert!(plan.functions[0].slots.contains(&SlotId(1)));

    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations[0] = TaskOp::InitCallable {
        dst: SlotId(0),
        function: TaskFunctionId(99),
    };
    rejected(&bad, "unknown callable identity");
    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations[1] = TaskOp::Copy {
        dst: SlotId(1),
        src: SlotId(0),
    };
    rejected(&bad, "implicit callable copy");
    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations.remove(0);
    rejected(&bad, "clone of uninitialized callable");
    let mut bad = fixture.clone();
    bad.functions[0].states[0].operations.insert(
        2,
        TaskOp::CloneCallable {
            dst: SlotId(1),
            src: SlotId(0),
        },
    );
    rejected(&bad, "clone overwrites live callable owner");
    let mut bad = fixture.clone();
    bad.functions[0].states[3].operations[0] = TaskOp::StartIndirect {
        dst: SlotId(3),
        callee: SlotId(0),
        arguments: vec![SlotId(2)],
    };
    rejected(&bad, "callable has already been dropped");
    let mut bad = fixture.clone();
    bad.functions[0].parameters.push(SlotId(0));
    rejected(
        &bad,
        "callable parameter cannot be initialized over its live owner",
    );
    let mut bad = fixture.clone();
    bad.functions[0].slots[0].ty = TaskSlotType::Callable {
        parameters: vec![IrType::Bool],
        result: result(IrType::Int),
    };
    rejected(&bad, "complete signature includes argument types");
    let mut bad = fixture.clone();
    bad.functions[0].states[3].operations[0] = TaskOp::StartIndirect {
        dst: SlotId(3),
        callee: SlotId(1),
        arguments: vec![SlotId(6)],
    };
    rejected(&bad, "indirect argument type differs");
    let mut bad = fixture.clone();
    bad.functions[0].states[5]
        .operations
        .push(TaskOp::StartIndirect {
            dst: SlotId(3),
            callee: SlotId(1),
            arguments: vec![SlotId(2)],
        });
    rejected(&bad, "cleanup cannot start an indirect task");
    let mut chain = fixture.clone();
    chain.functions[1] = indirect_child(1, 2);
    accepted(&chain);
    chain.functions[2] = indirect_child(2, 1);
    rejected(
        &chain,
        "indirect edges participate in recursive Start detection",
    );
    let limits = TaskLimits {
        max_analysis_work: 1,
        ..TaskLimits::default()
    };
    assert!(verify_and_plan(&fixture, limits)
        .unwrap_err()
        .to_string()
        .contains("analysis work limit exceeded"));
}

#[test]
fn native_task_finally_ir_checks_dispatch_live_values_markers_and_unwind_pairs() {
    let fixture = program(
        vec![IrType::Int, result(IrType::Null)],
        &[],
        vec![
            state(vec![init(0, TaskConstant::Int(37))], jump(1)),
            state(
                vec![TaskOp::FinallyEnter {
                    cleanup: StateId(5),
                }],
                jump(2),
            ),
            state(vec![], suspend(3, 5)),
            state(
                vec![TaskOp::FinallyLeave {
                    cleanup: StateId(5),
                }],
                jump(4),
            ),
            state(
                vec![init(1, TaskConstant::Ok(Box::new(TaskConstant::Null)))],
                TaskTerminator::Exit { value: SlotId(1) },
            ),
            state(vec![read(0), drop_if(1)], TaskTerminator::Terminate),
        ],
    );
    let plan = accepted(&fixture);
    assert_eq!(plan.functions[0].finally.state_cleanup[2], Some(StateId(5)));
    assert!(plan.functions[0].slots.contains(&SlotId(0)));
    let mut wrong_leave = fixture.clone();
    wrong_leave.functions[0].states[3].operations[0] = TaskOp::FinallyLeave {
        cleanup: StateId(4),
    };
    rejected(&wrong_leave, "Leave must match the active finally");
    let mut early_read = fixture.clone();
    early_read.functions[0].states[0].operations.clear();
    early_read.functions[0].states[2]
        .operations
        .push(init(0, TaskConstant::Int(37)));
    rejected(
        &early_read,
        "cancellation before state operations still reads initialized values",
    );
    let mut unmatched_end = fixture.clone();
    unmatched_end.functions[0].states[0]
        .operations
        .push(TaskOp::FinallyUnwindEnd);
    rejected(&unmatched_end, "ordinary unwind End requires Begin");
    let mut nested_begin = fixture.clone();
    nested_begin.functions[0].states[0]
        .operations
        .extend([TaskOp::FinallyUnwindBegin, TaskOp::FinallyUnwindBegin]);
    rejected(&nested_begin, "ordinary unwind Begin cannot nest");
    let mut no_entry = minimal();
    no_entry.functions[0].states[0]
        .operations
        .push(TaskOp::FinallyUnwindBegin);
    no_entry.functions[0].states[0].terminator = TaskTerminator::Exit { value: SlotId(0) };
    rejected(
        &no_entry,
        "ordinary unwind cannot manufacture a finally declaration",
    );
    let mut protected_unwind = fixture.clone();
    protected_unwind.functions[0].states[5]
        .operations
        .insert(0, TaskOp::FinallyUnwindBegin);
    rejected(
        &protected_unwind,
        "protected cleanup cannot replace its original deadline",
    );

    let mut projected = fixture.clone();
    let function = &mut projected.functions[0];
    function.slots = vec![
        TaskSlot {
            ty: TaskSlotType::Value {
                ty: result(IrType::Str),
                borrowed: false,
            },
        },
        TaskSlot {
            ty: TaskSlotType::Value {
                ty: IrType::Str,
                borrowed: false,
            },
        },
        TaskSlot {
            ty: TaskSlotType::Value {
                ty: result(IrType::Null),
                borrowed: false,
            },
        },
        TaskSlot {
            ty: TaskSlotType::Value {
                ty: IrType::Named("__ku_error_type".into()),
                borrowed: false,
            },
        },
    ];
    function.parameters = vec![SlotId(0)];
    function.states[0].operations.clear();
    function.states[4].operations = vec![init(2, TaskConstant::Ok(Box::new(TaskConstant::Null)))];
    function.states[4].terminator = TaskTerminator::Exit { value: SlotId(2) };
    function.states[5].operations.clear();
    function.states[5].terminator = TaskTerminator::TryResult {
        src: SlotId(0),
        ok_value: SlotId(1),
        err_result: SlotId(3),
        ok: StateId(6),
        err: StateId(7),
    };
    for _ in 0..2 {
        function.states.push(state(
            vec![drop_if(0), drop_if(1), drop_if(2), drop_if(3)],
            TaskTerminator::Terminate,
        ));
    }
    let plan = accepted(&projected);
    assert_eq!(
        plan.functions[0].finally.region_slots[5],
        (1u64 << 1) | (1u64 << 3)
    );
}

#[test]
fn blocking_write_preserves_both_view_sources_and_full_function_slot_limits() {
    let mut types = vec![IrType::Int; 64];
    types[61] = IrType::Str;
    types[62] = result(IrType::Null);
    types[63] = IrType::Str;
    let fixture = program(
        types,
        &[63, 61],
        vec![
            state(
                vec![],
                TaskTerminator::BlockingWrite {
                    path: SlotId(63),
                    content: SlotId(61),
                    dst: SlotId(62),
                    ready: StateId(1),
                    cleanup: StateId(2),
                },
            ),
            state(
                vec![read(63), read(61), drop_slot(63), drop_slot(61)],
                complete(62),
            ),
            state(
                vec![drop_if(63), drop_if(61), drop_if(62)],
                TaskTerminator::Terminate,
            ),
        ],
    );
    let plan = verify_and_plan(&fixture, TaskLimits::default()).unwrap();
    assert!(plan.functions[0].hosted);
    assert!(plan.functions[0].slots.contains(&SlotId(63)));
    assert!(plan.functions[0].slots.contains(&SlotId(61)));
    let mut full = fixture.clone();
    for index in 1..64 {
        let mut function = fixture.functions[0].clone();
        function.id = TaskFunctionId(index);
        function.name = format!("Write{index}");
        full.functions.push(function);
    }
    assert_eq!(
        verify_and_plan(&full, TaskLimits::default())
            .unwrap()
            .functions
            .len(),
        64
    );
    for mode in 0..4 {
        let mut invalid = fixture.clone();
        match mode {
            0 => {
                invalid.functions[0].slots[63].ty = TaskSlotType::Value {
                    ty: IrType::Int,
                    borrowed: false,
                }
            }
            1 => {
                invalid.functions[0].slots[61].ty = TaskSlotType::Value {
                    ty: IrType::Str,
                    borrowed: true,
                }
            }
            2 => invalid.functions[0].parameters.pop().map(|_| ()).unwrap(),
            _ => invalid.functions[0].parameters.push(SlotId(62)),
        }
        assert!(
            verify_and_plan(&invalid, TaskLimits::default()).is_err(),
            "invalid blocking source {mode}"
        );
    }
}

#[test]
fn blocking_read_preserves_view_and_initializes_only_ready_string_result() {
    let mut fixture = program(
        vec![IrType::Str, result(IrType::Str)],
        &[0],
        vec![
            state(
                vec![],
                TaskTerminator::BlockingRead {
                    path: SlotId(0),
                    dst: SlotId(1),
                    ready: StateId(1),
                    cleanup: StateId(2),
                },
            ),
            state(vec![read(0), drop_slot(0)], complete(1)),
            state(vec![drop_if(0), drop_if(1)], TaskTerminator::Terminate),
        ],
    );
    fixture.functions[0].result = result(IrType::Str);
    let plan = verify_and_plan(&fixture, TaskLimits::default()).unwrap();
    assert!(plan.functions[0].hosted);
    assert!(plan.functions[0].slots.contains(&SlotId(0)));
    fixture.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: result(IrType::Null),
        borrowed: false,
    };
    assert!(verify_and_plan(&fixture, TaskLimits::default()).is_err());
}

#[test]
fn blocking_read_dir_preserves_view_and_initializes_only_ready_array_result() {
    let mut fixture = program(
        vec![IrType::Str, result(IrType::Array(Box::new(IrType::Str)))],
        &[0],
        vec![
            state(
                vec![],
                TaskTerminator::BlockingReadDir {
                    path: SlotId(0),
                    dst: SlotId(1),
                    ready: StateId(1),
                    cleanup: StateId(2),
                },
            ),
            state(vec![read(0), drop_slot(0)], complete(1)),
            state(vec![drop_if(0), drop_if(1)], TaskTerminator::Terminate),
        ],
    );
    fixture.functions[0].result = result(IrType::Array(Box::new(IrType::Str)));
    let plan = verify_and_plan(&fixture, TaskLimits::default()).unwrap();
    assert!(plan.functions[0].hosted);
    assert!(plan.functions[0].slots.contains(&SlotId(0)));
    fixture.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: result(IrType::Null),
        borrowed: false,
    };
    assert!(verify_and_plan(&fixture, TaskLimits::default()).is_err());
}

#[test]
fn blocking_exists_preserves_view_and_initializes_only_ready_bool_at_slot_limit() {
    let mut types = vec![IrType::Int; 64];
    types[61] = result(IrType::Bool);
    types[62] = IrType::Bool;
    types[63] = IrType::Str;
    let mut fixture = program(
        types,
        &[63],
        vec![
            state(
                vec![],
                TaskTerminator::BlockingExists {
                    path: SlotId(63),
                    dst: SlotId(62),
                    ready: StateId(1),
                    cleanup: StateId(2),
                },
            ),
            state(
                vec![
                    read(63),
                    read(62),
                    drop_slot(63),
                    TaskOp::WrapOk {
                        dst: SlotId(61),
                        src: SlotId(62),
                    },
                ],
                complete(61),
            ),
            state(
                vec![read(63), drop_if(63), drop_if(62), drop_if(61)],
                TaskTerminator::Terminate,
            ),
        ],
    );
    fixture.functions[0].result = result(IrType::Bool);
    let plan = accepted(&fixture);
    assert!(plan.functions[0].hosted && plan.functions[0].slots.contains(&SlotId(63)));
    for invalid_type in [IrType::Int, IrType::Null, result(IrType::Bool)] {
        let mut invalid = fixture.clone();
        invalid.functions[0].slots[62].ty = TaskSlotType::Value {
            ty: invalid_type,
            borrowed: false,
        };
        rejected(&invalid, "exists destination must be the Copy bool");
    }
    let mut invalid = fixture.clone();
    invalid.functions[0].states[2]
        .operations
        .insert(0, read(62));
    rejected(&invalid, "cleanup never initializes the exists destination");
    let mut invalid = fixture.clone();
    invalid.functions[0].parameters.clear();
    rejected(&invalid, "exists must not read an uninitialized path");
    let mut invalid = fixture.clone();
    invalid.functions[0].slots[63].ty = TaskSlotType::Value {
        ty: IrType::Str,
        borrowed: true,
    };
    rejected(&invalid, "a borrowed path cannot persist in a task frame");
    let mut invalid = fixture.clone();
    invalid.functions[0].slots[63].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: false,
    };
    rejected(&invalid, "exists requires a string path");
    let mut invalid = fixture.clone();
    invalid.functions[0].parameters.push(SlotId(62));
    rejected(
        &invalid,
        "exists cannot overwrite an initialized destination",
    );
    let mut invalid = fixture;
    invalid.functions[0].slots.push(TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Int,
            borrowed: false,
        },
    });
    rejected(&invalid, "exists preserves the 64-slot hard limit");
}

#[test]
fn native_task_flat_array_ir_validates_owned_elements_and_parameter_boundary() {
    let fixture = program(
        vec![
            IrType::Str,
            IrType::Array(Box::new(IrType::Str)),
            result(IrType::Null),
        ],
        &[0],
        vec![state(
            vec![
                TaskOp::ArrayMake {
                    dst: SlotId(1),
                    elements: vec![SlotId(0)],
                },
                drop_slot(1),
                init(2, TaskConstant::Ok(Box::new(TaskConstant::Null))),
            ],
            complete(2),
        )],
    );
    let plan = verify_and_plan(&fixture, TaskLimits::default()).unwrap();
    assert!(plan.functions[0].hosted);
    assert!(plan.functions[0].slots.contains(&SlotId(1)));
    let mut duplicate = fixture.clone();
    if let TaskOp::ArrayMake { elements, .. } = &mut duplicate.functions[0].states[0].operations[0]
    {
        elements.push(SlotId(0));
    }
    assert!(verify_and_plan(&duplicate, TaskLimits::default()).is_err());
    let mut parameter = fixture.clone();
    parameter.functions[0].parameters.push(SlotId(1));
    let error = verify_and_plan(&parameter, TaskLimits::default()).unwrap_err();
    assert!(
        error
            .message
            .contains("overwriting a possibly initialized owned slot without drop"),
        "{error}"
    );
    for element in [IrType::Int, IrType::Bool, IrType::Str] {
        let array = IrType::Array(Box::new(element));
        let valid = program(
            vec![array.clone(), result(IrType::Null)],
            &[0],
            vec![state(
                vec![
                    drop_slot(0),
                    init(1, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(1),
            )],
        );
        let plan = verify_and_plan(&valid, TaskLimits::default()).unwrap();
        assert!(plan.functions[0].hosted && plan.functions[0].slots.contains(&SlotId(0)));
        let twice = program(
            vec![array.clone(), array.clone(), array, result(IrType::Null)],
            &[0],
            vec![state(
                vec![
                    TaskOp::Move {
                        dst: SlotId(1),
                        src: SlotId(0),
                    },
                    TaskOp::Move {
                        dst: SlotId(2),
                        src: SlotId(0),
                    },
                    drop_slot(1),
                    drop_slot(2),
                    init(3, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                ],
                complete(3),
            )],
        );
        assert!(verify_and_plan(&twice, TaskLimits::default()).is_err());
    }
    let mut nested = fixture.clone();
    nested.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: IrType::Array(Box::new(IrType::Array(Box::new(IrType::Str)))),
        borrowed: false,
    };
    assert!(verify_and_plan(&nested, TaskLimits::default()).is_err());
}

#[test]
fn native_task_array_try_get_ir_preserves_inputs_and_owns_only_the_result() {
    for element in [IrType::Int, IrType::Bool, IrType::Str] {
        let array = IrType::Array(Box::new(element.clone()));
        let fixture = program(
            vec![
                array.clone(),
                IrType::Int,
                result(element),
                result(IrType::Null),
            ],
            &[0, 1],
            vec![
                state(
                    vec![TaskOp::ArrayTryGet {
                        dst: SlotId(2),
                        array: SlotId(0),
                        index: SlotId(1),
                    }],
                    suspend(1, 2),
                ),
                state(
                    vec![
                        read(0),
                        read(1),
                        read(2),
                        drop_slot(2),
                        drop_slot(0),
                        init(3, TaskConstant::Ok(Box::new(TaskConstant::Null))),
                    ],
                    complete(3),
                ),
                state(vec![drop_if(2), drop_if(0)], TaskTerminator::Terminate),
            ],
        );
        let plan = accepted(&fixture);
        let frame = &plan.functions[0];
        assert!(frame.hosted);
        for slot in [SlotId(0), SlotId(1), SlotId(2)] {
            assert!(frame.slots.contains(&slot));
            assert!(frame
                .suspensions
                .iter()
                .any(|suspension| suspension.slots.contains(&slot)));
        }
        let mut invalid = fixture.clone();
        invalid.functions[0].parameters.push(SlotId(2));
        rejected(&invalid, "try_get must not overwrite an initialized Result");
        for missing in [SlotId(0), SlotId(1)] {
            let mut invalid = fixture.clone();
            invalid.functions[0]
                .parameters
                .retain(|slot| *slot != missing);
            rejected(
                &invalid,
                "both try_get inputs must be definitely initialized",
            );
        }
        for slot in [0usize, 1, 2] {
            let mut invalid = fixture.clone();
            invalid.functions[0].slots[slot].ty = TaskSlotType::Value {
                ty: IrType::Bool,
                borrowed: false,
            };
            rejected(
                &invalid,
                "try_get source, index and Result types must match",
            );
            let mut invalid = fixture.clone();
            if let TaskSlotType::Value { borrowed, .. } = &mut invalid.functions[0].slots[slot].ty {
                *borrowed = true;
            }
            rejected(
                &invalid,
                "try_get cannot retain borrowed input or Result slots",
            );
        }
        let mut invalid = fixture.clone();
        invalid.functions[0].states[0]
            .operations
            .push(TaskOp::ArrayTryGet {
                dst: SlotId(2),
                array: SlotId(0),
                index: SlotId(1),
            });
        rejected(
            &invalid,
            "a second try_get cannot overwrite the first Owned Result",
        );
        let mut invalid = fixture.clone();
        invalid.functions[0].states[0]
            .operations
            .insert(0, drop_slot(0));
        rejected(&invalid, "try_get cannot read an already dropped source");
        let mut invalid = fixture;
        invalid.functions[0].slots[0].ty = TaskSlotType::Value {
            ty: IrType::Array(Box::new(array)),
            borrowed: false,
        };
        rejected(&invalid, "try_get does not open nested arrays");
    }
}
