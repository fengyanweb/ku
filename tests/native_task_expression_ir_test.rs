//! R5e raw typed IR gates. These tests do not substitute for native execution.
use ku::ir::{task::*, IrType};

const BINARY_OPS: [TaskBinaryOp; 11] = [
    TaskBinaryOp::Add,
    TaskBinaryOp::Subtract,
    TaskBinaryOp::Multiply,
    TaskBinaryOp::Divide,
    TaskBinaryOp::Remainder,
    TaskBinaryOp::Equal,
    TaskBinaryOp::NotEqual,
    TaskBinaryOp::Less,
    TaskBinaryOp::LessEqual,
    TaskBinaryOp::Greater,
    TaskBinaryOp::GreaterEqual,
];

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
fn null_result(dst: usize) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
    }
}
fn unary(op: TaskUnaryOp, input: IrType, output: IrType) -> TaskProgram {
    TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "UnaryFixture".into(),
            slots: vec![value(input), value(output), value(result(IrType::Null))],
            parameters: vec![SlotId(0)],
            entry: StateId(0),
            states: vec![state(
                vec![
                    TaskOp::Unary {
                        dst: SlotId(1),
                        op,
                        src: SlotId(0),
                    },
                    read(0),
                    read(1),
                    null_result(2),
                ],
                done(2),
            )],
            result: result(IrType::Null),
        }],
    }
}
fn binary(op: TaskBinaryOp, left: IrType, right: IrType, output: IrType) -> TaskProgram {
    TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "BinaryFixture".into(),
            slots: vec![
                value(left),
                value(right),
                value(output),
                value(result(IrType::Null)),
            ],
            parameters: vec![SlotId(0), SlotId(1)],
            entry: StateId(0),
            states: vec![state(
                vec![
                    TaskOp::Binary {
                        dst: SlotId(2),
                        op,
                        left: SlotId(0),
                        right: SlotId(1),
                    },
                    read(0),
                    read(1),
                    read(2),
                    null_result(3),
                ],
                done(3),
            )],
            result: result(IrType::Null),
        }],
    }
}
fn accepted(program: &TaskProgram) -> TaskFramePlan {
    verify_and_plan(program, TaskLimits::default())
        .unwrap_or_else(|error| panic!("valid expression IR rejected: {error}\n{program:?}"))
}
fn rejected(program: &TaskProgram, message: &str) {
    let error = verify_and_plan(program, TaskLimits::default()).unwrap_err();
    assert!(error.message.contains(message), "{error}\n{program:?}");
}
fn type_cases() -> [IrType; 5] {
    [
        IrType::Int,
        IrType::Bool,
        IrType::Null,
        IrType::Str,
        result(IrType::Int),
    ]
}
fn binary_types_match(op: TaskBinaryOp, left: &IrType, right: &IrType, output: &IrType) -> bool {
    match op {
        TaskBinaryOp::Add
        | TaskBinaryOp::Subtract
        | TaskBinaryOp::Multiply
        | TaskBinaryOp::Divide
        | TaskBinaryOp::Remainder => {
            left == &IrType::Int && right == &IrType::Int && output == &IrType::Int
        }
        TaskBinaryOp::Equal | TaskBinaryOp::NotEqual => {
            matches!(left, IrType::Int | IrType::Bool) && left == right && output == &IrType::Bool
        }
        TaskBinaryOp::Less
        | TaskBinaryOp::LessEqual
        | TaskBinaryOp::Greater
        | TaskBinaryOp::GreaterEqual => {
            left == &IrType::Int && right == &IrType::Int && output == &IrType::Bool
        }
    }
}

#[test]
fn native_task_expression_ir_unary_exact_type_matrix() {
    for op in [TaskUnaryOp::Negate, TaskUnaryOp::Not] {
        for input in type_cases() {
            for output in type_cases() {
                let expected = match op {
                    TaskUnaryOp::Negate => input == IrType::Int && output == IrType::Int,
                    TaskUnaryOp::Not => input == IrType::Bool && output == IrType::Bool,
                };
                let program = unary(op, input.clone(), output.clone());
                assert_eq!(
                    verify_and_plan(&program, TaskLimits::default()).is_ok(),
                    expected,
                    "{op:?} {input:?} -> {output:?}"
                );
            }
        }
    }
}

#[test]
fn native_task_expression_ir_binary_exact_type_matrix() {
    for op in BINARY_OPS {
        for left in type_cases() {
            for right in type_cases() {
                for output in type_cases() {
                    let expected = binary_types_match(op, &left, &right, &output);
                    let program = binary(op, left.clone(), right.clone(), output.clone());
                    assert_eq!(
                        verify_and_plan(&program, TaskLimits::default()).is_ok(),
                        expected,
                        "{left:?} {op:?} {right:?} -> {output:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn native_task_expression_ir_rejects_task_payloads_and_borrowed_destinations() {
    for index in 0..2 {
        let mut program = unary(TaskUnaryOp::Not, IrType::Bool, IrType::Bool);
        program.functions[0].slots[index].ty = TaskSlotType::Task {
            result: result(IrType::Bool),
        };
        if index == 0 {
            // A Task parameter is separately illegal. Keep this case aimed at
            // the expression shape rather than that earlier parameter gate.
            program.functions[0].parameters.clear();
        }
        rejected(&program, "operation requires a Value slot");
    }
    for index in 0..3 {
        let mut program = binary(TaskBinaryOp::Equal, IrType::Int, IrType::Int, IrType::Bool);
        program.functions[0].slots[index].ty = TaskSlotType::Task {
            result: result(IrType::Int),
        };
        program.functions[0]
            .parameters
            .retain(|slot| slot.0 != index);
        rejected(&program, "operation requires a Value slot");
    }
    let mut unary = unary(TaskUnaryOp::Not, IrType::Bool, IrType::Bool);
    unary.functions[0].slots[1].ty = TaskSlotType::Value {
        ty: IrType::Bool,
        borrowed: true,
    };
    rejected(&unary, "Unary requires");
    let mut binary = binary(TaskBinaryOp::Add, IrType::Int, IrType::Int, IrType::Int);
    binary.functions[0].slots[2].ty = TaskSlotType::Value {
        ty: IrType::Int,
        borrowed: true,
    };
    rejected(&binary, "Binary requires");
}

#[test]
fn native_task_expression_ir_rejects_unknown_and_destination_alias_slots() {
    for operand in 0..2 {
        let mut program = unary(TaskUnaryOp::Negate, IrType::Int, IrType::Int);
        let TaskOp::Unary { dst, src, .. } = &mut program.functions[0].states[0].operations[0]
        else {
            unreachable!()
        };
        *if operand == 0 { dst } else { src } = SlotId(usize::MAX);
        rejected(&program, "unknown slot");
    }
    for operand in 0..3 {
        let mut program = binary(TaskBinaryOp::Add, IrType::Int, IrType::Int, IrType::Int);
        let TaskOp::Binary {
            dst, left, right, ..
        } = &mut program.functions[0].states[0].operations[0]
        else {
            unreachable!()
        };
        *match operand {
            0 => dst,
            1 => left,
            _ => right,
        } = SlotId(usize::MAX);
        rejected(&program, "unknown slot");
    }
    let mut program = unary(TaskUnaryOp::Negate, IrType::Int, IrType::Int);
    let TaskOp::Unary { dst, .. } = &mut program.functions[0].states[0].operations[0] else {
        unreachable!()
    };
    *dst = SlotId(0);
    rejected(&program, "Unary requires");
    for source in [0, 1] {
        let mut program = binary(TaskBinaryOp::Add, IrType::Int, IrType::Int, IrType::Int);
        let TaskOp::Binary { dst, .. } = &mut program.functions[0].states[0].operations[0] else {
            unreachable!()
        };
        *dst = SlotId(source);
        rejected(&program, "Binary requires");
    }
}

#[test]
fn native_task_expression_ir_reads_each_input_and_does_not_consume_copy_values() {
    let unary = unary(TaskUnaryOp::Not, IrType::Bool, IrType::Bool);
    accepted(&unary); // Read(source) and Read(destination) after the operation.
    let mut missing = unary.clone();
    missing.functions[0].parameters.clear();
    rejected(&missing, "not definitely initialized");
    for source in [0, 1] {
        let mut program = binary(TaskBinaryOp::Add, IrType::Int, IrType::Int, IrType::Int);
        program.functions[0]
            .parameters
            .retain(|slot| slot.0 != source);
        rejected(
            &program,
            "Binary reads a slot that is not definitely initialized",
        );
    }
    let mut repeated = binary(TaskBinaryOp::Add, IrType::Int, IrType::Int, IrType::Int);
    let TaskOp::Binary { right, .. } = &mut repeated.functions[0].states[0].operations[0] else {
        unreachable!()
    };
    *right = SlotId(0);
    accepted(&repeated); // x+x is legal; only destination/input alias is barred.
    let operation = repeated.functions[0].states[0].operations[0].clone();
    repeated.functions[0].states[0]
        .operations
        .insert(1, operation);
    accepted(&repeated); // Explicitly initialized Copy destination may be replaced.
}

fn cleanup_program(mut program: TaskProgram) -> TaskProgram {
    let operation = program.functions[0].states[0].operations[0].clone();
    let result = program.functions[0].slots.len() - 1;
    program.functions[0].states = vec![
        state(
            vec![],
            TaskTerminator::Suspend {
                resume: StateId(1),
                cleanup: StateId(2),
            },
        ),
        state(vec![null_result(result)], done(result)),
        state(vec![operation], TaskTerminator::Terminate),
    ];
    program
}

#[test]
fn native_task_expression_ir_cleanup_rejects_all_trapping_arithmetic() {
    rejected(
        &cleanup_program(unary(TaskUnaryOp::Negate, IrType::Int, IrType::Int)),
        "cleanup cannot perform trapping arithmetic",
    );
    for op in [
        TaskBinaryOp::Add,
        TaskBinaryOp::Subtract,
        TaskBinaryOp::Multiply,
        TaskBinaryOp::Divide,
        TaskBinaryOp::Remainder,
    ] {
        rejected(
            &cleanup_program(binary(op, IrType::Int, IrType::Int, IrType::Int)),
            "cleanup cannot perform trapping arithmetic",
        );
    }
}

#[test]
fn native_task_expression_ir_cleanup_allows_total_copy_operations() {
    accepted(&cleanup_program(unary(
        TaskUnaryOp::Not,
        IrType::Bool,
        IrType::Bool,
    )));
    for op in [
        TaskBinaryOp::Equal,
        TaskBinaryOp::NotEqual,
        TaskBinaryOp::Less,
        TaskBinaryOp::LessEqual,
        TaskBinaryOp::Greater,
        TaskBinaryOp::GreaterEqual,
    ] {
        accepted(&cleanup_program(binary(
            op,
            IrType::Int,
            IrType::Int,
            IrType::Bool,
        )));
    }
    for op in [TaskBinaryOp::Equal, TaskBinaryOp::NotEqual] {
        accepted(&cleanup_program(binary(
            op,
            IrType::Bool,
            IrType::Bool,
            IrType::Bool,
        )));
    }
}

#[test]
fn native_task_expression_ir_operations_initialize_both_join_paths() {
    let mut program = unary(TaskUnaryOp::Not, IrType::Bool, IrType::Bool);
    program.functions[0].slots[2] = value(result(IrType::Bool));
    program.functions[0].result = result(IrType::Bool);
    program.functions[0].states = vec![
        state(
            vec![],
            TaskTerminator::Branch {
                condition: SlotId(0),
                then_state: StateId(1),
                else_state: StateId(2),
            },
        ),
        state(
            vec![TaskOp::Unary {
                dst: SlotId(1),
                op: TaskUnaryOp::Not,
                src: SlotId(0),
            }],
            jump(3),
        ),
        state(
            vec![TaskOp::Binary {
                dst: SlotId(1),
                op: TaskBinaryOp::Equal,
                left: SlotId(0),
                right: SlotId(0),
            }],
            jump(3),
        ),
        state(
            vec![TaskOp::WrapOk {
                dst: SlotId(2),
                src: SlotId(1),
            }],
            done(2),
        ),
    ];
    accepted(&program);
    program.functions[0].states[2].operations.clear();
    rejected(&program, "not definitely initialized");
}

#[test]
fn native_task_expression_ir_unary_and_binary_liveness_save_copy_inputs() {
    let mut program = binary(TaskBinaryOp::Add, IrType::Int, IrType::Int, IrType::Int);
    program.functions[0].slots.push(value(IrType::Int));
    program.functions[0].result = result(IrType::Int);
    program.functions[0].slots[3] = value(result(IrType::Int));
    program.functions[0].states = vec![
        state(
            vec![TaskOp::Binary {
                dst: SlotId(2),
                op: TaskBinaryOp::Add,
                left: SlotId(0),
                right: SlotId(1),
            }],
            TaskTerminator::Suspend {
                resume: StateId(1),
                cleanup: StateId(2),
            },
        ),
        state(
            vec![
                TaskOp::Unary {
                    dst: SlotId(4),
                    op: TaskUnaryOp::Negate,
                    src: SlotId(2),
                },
                TaskOp::WrapOk {
                    dst: SlotId(3),
                    src: SlotId(4),
                },
            ],
            done(3),
        ),
        state(vec![], TaskTerminator::Terminate),
    ];
    let plan = accepted(&program);
    let frame = &plan.functions[0];
    assert!(!frame.hosted);
    assert!(frame.slots.contains(&SlotId(2)));
    assert!(!frame.slots.contains(&SlotId(4)));
    assert_eq!(frame.suspensions[0].slots, vec![SlotId(2)]);
}

fn two_await_program() -> TaskProgram {
    let task_slot = || TaskSlot {
        ty: TaskSlotType::Task {
            result: result(IrType::Int),
        },
    };
    let drop_if = |slot| TaskOp::DropIfInit { slot: SlotId(slot) };
    let start = |dst| TaskOp::Start {
        dst: SlotId(dst),
        function: TaskFunctionId(9),
        arguments: vec![SlotId(0)],
    };
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "TwoAwait".into(),
                slots: vec![
                    value(IrType::Int),
                    value(IrType::Int),
                    task_slot(),
                    value(result(IrType::Int)),
                    value(IrType::Int),
                    task_slot(),
                    value(result(IrType::Int)),
                    value(IrType::Int),
                    value(IrType::Int),
                    value(result(IrType::Int)),
                    value(result(IrType::Int)),
                ],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Int),
                states: vec![
                    state(vec![start(2)], jump(1)),
                    state(
                        vec![],
                        TaskTerminator::Await {
                            task: SlotId(2),
                            dst: SlotId(3),
                            ready: StateId(2),
                            cleanup: StateId(7),
                        },
                    ),
                    state(
                        vec![],
                        TaskTerminator::TryResult {
                            src: SlotId(3),
                            ok_value: SlotId(4),
                            err_result: SlotId(10),
                            ok: StateId(3),
                            err: StateId(8),
                        },
                    ),
                    state(
                        vec![
                            TaskOp::Unary {
                                dst: SlotId(1),
                                op: TaskUnaryOp::Negate,
                                src: SlotId(4),
                            },
                            start(5),
                        ],
                        jump(4),
                    ),
                    state(
                        vec![],
                        TaskTerminator::Await {
                            task: SlotId(5),
                            dst: SlotId(6),
                            ready: StateId(5),
                            cleanup: StateId(7),
                        },
                    ),
                    state(
                        vec![],
                        TaskTerminator::TryResult {
                            src: SlotId(6),
                            ok_value: SlotId(7),
                            err_result: SlotId(10),
                            ok: StateId(6),
                            err: StateId(8),
                        },
                    ),
                    state(
                        vec![
                            TaskOp::Binary {
                                dst: SlotId(8),
                                op: TaskBinaryOp::Add,
                                left: SlotId(1),
                                right: SlotId(7),
                            },
                            TaskOp::WrapOk {
                                dst: SlotId(9),
                                src: SlotId(8),
                            },
                        ],
                        done(9),
                    ),
                    state(
                        vec![drop_if(10), drop_if(9), drop_if(6), drop_if(3)],
                        TaskTerminator::Terminate,
                    ),
                    state(vec![drop_if(9), drop_if(6), drop_if(3)], done(10)),
                ],
            },
            TaskFunction {
                id: TaskFunctionId(9),
                name: "Leaf".into(),
                slots: vec![value(IrType::Int), value(result(IrType::Int))],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                states: vec![state(
                    vec![TaskOp::WrapOk {
                        dst: SlotId(1),
                        src: SlotId(0),
                    }],
                    done(1),
                )],
                result: result(IrType::Int),
            },
        ],
    }
}

#[test]
fn native_task_expression_ir_left_snapshot_survives_second_await() {
    let plan = accepted(&two_await_program());
    let frame = &plan.functions[0];
    assert!(frame.hosted);
    assert_eq!(frame.scope_task_mask, (1u64 << 2) | (1u64 << 5));
    assert_eq!(frame.suspensions.len(), 2);
    let second = frame
        .suspensions
        .iter()
        .find(|suspension| suspension.state == StateId(4))
        .unwrap();
    assert!(second.slots.contains(&SlotId(1)));
    assert!(frame.slots.contains(&SlotId(1)));
    assert!(!frame.slots.contains(&SlotId(8)));
}

#[test]
fn native_task_expression_ir_only_copy_operations_do_not_require_a_host() {
    for program in [
        unary(TaskUnaryOp::Negate, IrType::Int, IrType::Int),
        binary(
            TaskBinaryOp::Equal,
            IrType::Bool,
            IrType::Bool,
            IrType::Bool,
        ),
    ] {
        let plan = accepted(&program);
        assert!(!plan.functions[0].hosted);
        assert_eq!(plan.functions[0].scope_task_mask, 0);
        assert!(plan.functions[0].suspensions.is_empty());
    }
}

#[test]
fn native_task_expression_ir_slot_63_is_valid_but_slot_65_is_not() {
    let mut program = binary(TaskBinaryOp::Less, IrType::Int, IrType::Int, IrType::Bool);
    program.functions[0].slots = (0..64).map(|_| value(IrType::Int)).collect();
    program.functions[0].slots[2] = value(result(IrType::Bool));
    program.functions[0].slots[62] = value(IrType::Bool);
    program.functions[0].slots[63] = value(IrType::Bool);
    program.functions[0].result = result(IrType::Bool);
    program.functions[0].states = vec![state(
        vec![
            TaskOp::Binary {
                dst: SlotId(63),
                op: TaskBinaryOp::Less,
                left: SlotId(0),
                right: SlotId(1),
            },
            TaskOp::Unary {
                dst: SlotId(62),
                op: TaskUnaryOp::Not,
                src: SlotId(63),
            },
            TaskOp::WrapOk {
                dst: SlotId(2),
                src: SlotId(62),
            },
        ],
        done(2),
    )];
    accepted(&program);
    program.functions[0].slots.push(value(IrType::Int));
    rejected(&program, "per-function state or slot limit exceeded");
}

#[test]
fn native_task_expression_ir_instruction_budget_is_not_relaxed() {
    let program = binary(
        TaskBinaryOp::Multiply,
        IrType::Int,
        IrType::Int,
        IrType::Int,
    );
    let count = program.functions[0].states[0].operations.len();
    let limits = TaskLimits {
        max_operations: count,
        ..TaskLimits::default()
    };
    verify_and_plan(&program, limits).unwrap();
    let error = verify_and_plan(
        &program,
        TaskLimits {
            max_operations: count - 1,
            ..limits
        },
    )
    .unwrap_err();
    assert!(
        error.message.contains("operation limit exceeded"),
        "{error}"
    );
}

fn minimum_analysis_budget(program: &TaskProgram) -> usize {
    // Small, bounded fixture search. It is a verifier work-budget probe, not a
    // wall-clock performance assertion or an unbounded convergence poll.
    (0..=512)
        .find(|work| {
            verify_and_plan(
                program,
                TaskLimits {
                    max_analysis_work: *work,
                    ..TaskLimits::default()
                },
            )
            .is_ok()
        })
        .expect("small expression fixture should fit within 512 analysis steps")
}

#[test]
fn native_task_expression_ir_analysis_budget_charges_both_binary_inputs() {
    let binary = binary(
        TaskBinaryOp::Equal,
        IrType::Bool,
        IrType::Bool,
        IrType::Bool,
    );
    let mut unary = binary.clone();
    unary.functions[0].states[0].operations[0] = TaskOp::Unary {
        dst: SlotId(2),
        op: TaskUnaryOp::Not,
        src: SlotId(0),
    };
    let binary_min = minimum_analysis_budget(&binary);
    let unary_min = minimum_analysis_budget(&unary);
    assert!(
        binary_min > unary_min,
        "binary={binary_min}, unary={unary_min}"
    );
    for (program, minimum) in [(&binary, binary_min), (&unary, unary_min)] {
        verify_and_plan(
            program,
            TaskLimits {
                max_analysis_work: minimum,
                ..TaskLimits::default()
            },
        )
        .unwrap();
        let error = verify_and_plan(
            program,
            TaskLimits {
                max_analysis_work: minimum - 1,
                ..TaskLimits::default()
            },
        )
        .unwrap_err();
        assert!(
            error.message.contains("analysis work limit exceeded"),
            "{error}"
        );
    }
}
