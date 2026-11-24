use super::*;
use std::collections::HashSet;

/// A nullable string projection is a shallow local view. It may be read only
/// in its creating state while the wrapper remains unchanged; owning uses are
/// lowered to NullableTake instead. This also checks hand-authored Task IR.
pub(super) fn validate_views(function: &TaskFunction, budget: &mut Budget) -> KuResult<()> {
    let projected: HashSet<_> = function
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|operation| match operation {
            TaskOp::Collection {
                dst,
                operation: TaskCollectionOperation::NullableView,
                ..
            } if matches!(
                function.slots.get(dst.0).map(|slot| &slot.ty),
                Some(TaskSlotType::Value { borrowed: true, .. })
            ) =>
            {
                Some(*dst)
            }
            _ => None,
        })
        .collect();
    if projected.is_empty() {
        return Ok(());
    }
    for state in &function.states {
        let mut live_views: HashMap<SlotId, SlotId> = HashMap::new();
        for operation in &state.operations {
            let destination = operation_destination(operation);
            let mut copy = operation.clone();
            let mut inputs = Vec::new();
            source_op_slots(&mut copy, &mut |slot| {
                if Some(*slot) != destination {
                    inputs.push(*slot);
                }
            });
            budget.spend_many(inputs.len().saturating_add(1))?;
            if inputs
                .iter()
                .any(|slot| projected.contains(slot) && !live_views.contains_key(slot))
            {
                return Err(invalid(
                    "nullable view outlives its creating state or unchanged owner",
                ));
            }
            if let Some(destination) = destination {
                live_views.retain(|view, owner| *view != destination && *owner != destination);
            }
            let readonly = matches!(
                operation,
                TaskOp::ArrayClone { .. }
                    | TaskOp::ArrayLen { .. }
                    | TaskOp::StringLen { .. }
                    | TaskOp::StringClone { .. }
                    | TaskOp::PrimitiveString { .. }
                    | TaskOp::StringConcat { .. }
                    | TaskOp::ArrayTryGet { .. }
                    | TaskOp::Unary { .. }
                    | TaskOp::Binary { .. }
                    | TaskOp::Read { .. }
                    | TaskOp::Print { .. }
                    | TaskOp::Collection {
                        operation: TaskCollectionOperation::Concat
                            | TaskCollectionOperation::First
                            | TaskCollectionOperation::Last
                            | TaskCollectionOperation::NullableClone
                            | TaskCollectionOperation::NullableView
                            | TaskCollectionOperation::NullableIsNull,
                        ..
                    }
            );
            if !readonly {
                live_views.retain(|_, owner| !inputs.contains(owner));
            }
            if let TaskOp::Collection {
                dst,
                operation: TaskCollectionOperation::NullableViewEnd,
                ..
            } = operation
            {
                live_views.remove(dst);
            }
            if let TaskOp::Collection {
                dst,
                source: Some(owner),
                operation: TaskCollectionOperation::NullableView,
                ..
            } = operation
            {
                if projected.contains(dst) {
                    live_views.insert(*dst, *owner);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(operations: Vec<TaskOp>) -> TaskFunction {
        TaskFunction {
            id: TaskFunctionId(0),
            name: "view".into(),
            parameters: vec![],
            entry: StateId(0),
            slots: vec![
                TaskSlot {
                    ty: TaskSlotType::Value {
                        ty: IrType::Nullable(Box::new(IrType::Str)),
                        borrowed: false,
                    },
                },
                TaskSlot {
                    ty: TaskSlotType::Value {
                        ty: IrType::Str,
                        borrowed: true,
                    },
                },
                TaskSlot {
                    ty: TaskSlotType::Value {
                        ty: IrType::Int,
                        borrowed: false,
                    },
                },
            ],
            states: vec![TaskState {
                operations,
                terminator: TaskTerminator::Terminate,
            }],
            result: IrType::Result(Box::new(IrType::Null)),
        }
    }

    #[test]
    fn nullable_view_cannot_survive_owner_retirement_or_a_state_edge() {
        let view = TaskOp::Collection {
            dst: SlotId(1),
            source: Some(SlotId(0)),
            other: None,
            operation: TaskCollectionOperation::NullableView,
        };
        let read = TaskOp::StringLen {
            dst: SlotId(2),
            src: SlotId(1),
            bytes: false,
        };
        let drop = TaskOp::Drop { slot: SlotId(0) };
        let valid = fixture(vec![view.clone(), read.clone(), drop.clone()]);
        validate_views(&valid, &mut Budget { remaining: 1000 })
            .expect("read ends before owner retirement");
        let retired = fixture(vec![view.clone(), drop, read.clone()]);
        assert!(validate_views(&retired, &mut Budget { remaining: 1000 }).is_err());
        let mut edge = fixture(vec![view]);
        edge.states[0].terminator = TaskTerminator::Jump { target: StateId(1) };
        edge.states.push(TaskState {
            operations: vec![read],
            terminator: TaskTerminator::Terminate,
        });
        assert!(validate_views(&edge, &mut Budget { remaining: 1000 }).is_err());
    }
}
