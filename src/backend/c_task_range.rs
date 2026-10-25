//! Optional, bounded numeric proof over already verified typed Task states.
//!
//! This is not a loop invariant or an AST analysis. Each invocation is guarded
//! afresh and executes at most COMPUTE_PROOF_STATES original state bodies.
//! This horizon includes every bank a private hosted invocation may borrow.
//! It is shared with the driver's generated cap, never treated as a loop
//! invariant or restarted with nonzero temporary locals at a bank boundary.
//! The finite product
//! graph (state, bodies remaining) proves individual operations; all other
//! operations retain their original checked helpers.

use crate::ir::{
    task::{TaskBinaryOp, TaskConstant, TaskFunction, TaskOp, TaskTerminator, TaskUnaryOp},
    IrType,
};
use std::cell::Cell;

pub(super) const MAX_EXTRA_SOURCE_BYTES: usize = 256 * 1024;
pub(super) const COMPUTE_BANK_STATES: usize = 64;
pub(super) const COMPUTE_MAX_BANKS: usize = 4;
pub(super) const COMPUTE_PUBLIC_SELECTION_CREDITS: usize = 32;
pub(super) const COMPUTE_PRIVATE_SELECTION_CREDITS: usize = 128;
// A single finite certificate covers the entire extended private invocation.
// It is never restarted from an old guard with nonzero temporary locals.
pub(super) const COMPUTE_PROOF_STATES: usize = 8192;
pub(super) const COMPUTE_RESERVATION_CREDITS: usize = 128;
pub(super) const COMPUTE_RESERVATION_STATES: usize = 8192;
pub(super) const COMPUTE_MAX_OPERATIONS: usize = 65536;
pub(super) const COMPUTE_PHASE_STATES: usize = 256;
pub(super) const COMPUTE_PHASE_OPERATIONS: usize = 1024;
// One shared finite compile-time analysis budget; exhaustion keeps checked lowering.
// The complete8192-state certificate is proved once, never renewed in-flight.
const MAX_WORK: usize = 8_000_000;

#[derive(Clone, Copy)]
struct GenerationState {
    disabled: bool,
    attempted: bool,
    extra: usize,
}

thread_local! {
    static GENERATION: Cell<GenerationState> = const { Cell::new(GenerationState {
        disabled: false, attempted: false, extra: MAX_EXTRA_SOURCE_BYTES,
    }) };
}

struct GenerationScope(GenerationState);

impl GenerationScope {
    fn new() -> Self {
        GENERATION.with(|state| {
            let previous = state.get();
            state.set(GenerationState {
                disabled: previous.disabled,
                attempted: false,
                extra: MAX_EXTRA_SOURCE_BYTES,
            });
            Self(previous)
        })
    }
}

impl Drop for GenerationScope {
    fn drop(&mut self) {
        GENERATION.with(|state| {
            let attempted = state.get().attempted;
            state.set(GenerationState {
                attempted: self.0.attempted || attempted,
                ..self.0
            });
        });
    }
}

/// A candidate can consume space needed by later HTTP/root glue. Retry the
/// complete pure source generator with only this optional optimization off.
/// No panic is caught, and an original error is returned by the original path.
pub(super) fn generate_with_fallback<T>(
    mut generate: impl FnMut() -> crate::error::KuResult<T>,
) -> crate::error::KuResult<T> {
    if !enabled() {
        return generate();
    }
    let _scope = GenerationScope::new();
    let candidate = generate();
    if candidate.is_err() && GENERATION.with(|state| state.get().attempted) {
        GENERATION.with(|state| {
            state.set(GenerationState {
                disabled: true,
                ..state.get()
            })
        });
        generate()
    } else {
        candidate
    }
}

pub(super) fn enabled() -> bool {
    GENERATION.with(|state| !state.get().disabled)
}

pub(super) fn attempted() {
    GENERATION.with(|state| {
        if !state.get().disabled {
            state.set(GenerationState {
                attempted: true,
                ..state.get()
            });
        }
    });
}

pub(super) fn claim_extra(bytes: usize) -> bool {
    GENERATION.with(|state| {
        let old = state.get();
        if let Some(extra) = old.extra.checked_sub(bytes) {
            state.set(GenerationState { extra, ..old });
            true
        } else {
            false
        }
    })
}

pub(super) struct WorkBudget(usize);

impl Default for WorkBudget {
    fn default() -> Self {
        Self(MAX_WORK)
    }
}

impl WorkBudget {
    fn spend(&mut self, count: usize) -> Option<()> {
        if let Some(remaining) = self.0.checked_sub(count) {
            self.0 = remaining;
            Some(())
        } else {
            self.0 = 0;
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Interval {
    low: i64,
    high: i64,
}

impl Interval {
    const FULL: Self = Self {
        low: i64::MIN,
        high: i64::MAX,
    };
    const ZERO: Self = Self { low: 0, high: 0 };
    const GUARDED: Self = Self {
        low: i32::MIN as i64,
        high: i32::MAX as i64,
    };
    const BOOLEAN: Self = Self { low: 0, high: 1 };

    fn constant(value: i64) -> Self {
        Self {
            low: value,
            high: value,
        }
    }

    fn contains(self, value: i64) -> bool {
        self.low <= value && value <= self.high
    }

    fn checked(low: i128, high: i128) -> (Self, bool) {
        if low >= i64::MIN as i128 && high <= i64::MAX as i128 {
            (
                Self {
                    low: low as i64,
                    high: high as i64,
                },
                true,
            )
        } else {
            // Only successful executions continue past the checked helper.
            // FULL is conservative even when all executions would fail.
            (Self::FULL, false)
        }
    }

    fn binary(self, op: TaskBinaryOp, right: Self) -> (Self, bool) {
        let (a, b, c, d) = (
            self.low as i128,
            self.high as i128,
            right.low as i128,
            right.high as i128,
        );
        match op {
            TaskBinaryOp::Add => Self::checked(a + c, b + d),
            TaskBinaryOp::Subtract => Self::checked(a - d, b - c),
            TaskBinaryOp::Multiply => {
                // All four products of i64 endpoints fit in signed i128.
                let products = [a * c, a * d, b * c, b * d];
                Self::checked(
                    *products.iter().min().unwrap(),
                    *products.iter().max().unwrap(),
                )
            }
            TaskBinaryOp::Divide | TaskBinaryOp::Remainder => {
                let safe = !(right.contains(0) || self.contains(i64::MIN) && right.contains(-1));
                if !safe {
                    return (Self::FULL, false);
                }
                if right.low != right.high {
                    return (Self::FULL, true);
                }
                if op == TaskBinaryOp::Divide {
                    // i128 also handles the mathematical MIN/-1 endpoints;
                    // the original checked-division fault was excluded above.
                    let first = a / c;
                    let last = b / c;
                    Self::checked(first.min(last), first.max(last))
                } else {
                    let bound = c.abs() - 1;
                    // C/Rust remainder follows the dividend's sign, including
                    // a negative dividend and either sign of divisor.
                    Self::checked(
                        if a < 0 { a.max(-bound) } else { 0 },
                        if b > 0 { b.min(bound) } else { 0 },
                    )
                }
            }
            _ => (Self::BOOLEAN, false),
        }
    }
}

pub(super) struct RangePlan {
    pub(super) guard_slots: Vec<usize>,
    pub(super) states: Vec<bool>,
    operations: Vec<Vec<bool>>,
}

impl RangePlan {
    pub(super) fn proves(&self, state: usize, operation: usize) -> bool {
        self.operations[state][operation]
    }

    pub(super) fn build(
        function: &TaskFunction,
        states: Vec<bool>,
        cached: impl Fn(usize) -> bool,
        local_zero: impl Fn(usize) -> bool,
        budget: &mut WorkBudget,
    ) -> Option<Self> {
        let count = function.slots.len();
        if count > 64 || states.len() != function.states.len() || !states.iter().any(|yes| *yes) {
            return None;
        }
        budget.spend(count.checked_mul(function.states.len())?)?;
        let guard_slots: Vec<_> = function
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                (cached(index)
                    && matches!(
                        &slot.ty,
                        crate::ir::task::TaskSlotType::Value {
                            ty: IrType::Int,
                            borrowed: false
                        }
                    ))
                .then_some(index)
            })
            .collect();
        if guard_slots.is_empty() {
            return None;
        }
        let initial: Vec<_> = (0..count)
            .map(|index| {
                if guard_slots.contains(&index) {
                    Interval::GUARDED
                } else if local_zero(index) {
                    Interval::ZERO
                } else {
                    Interval::FULL
                }
            })
            .collect();
        let mut operations: Vec<Vec<bool>> = function
            .states
            .iter()
            .enumerate()
            .map(|(state, body)| {
                body.operations
                    .iter()
                    .map(|operation| {
                        states[state]
                            && matches!(
                                operation,
                                TaskOp::Unary {
                                    op: TaskUnaryOp::Negate,
                                    ..
                                } | TaskOp::Binary {
                                    op: TaskBinaryOp::Add
                                        | TaskBinaryOp::Subtract
                                        | TaskBinaryOp::Multiply
                                        | TaskBinaryOp::Divide
                                        | TaskBinaryOp::Remainder,
                                    ..
                                }
                            )
                    })
                    .collect()
            })
            .collect();
        // Only two remaining-quantum layers are needed. Every original state
        // consumes one unit, including empty drains and source loop latches.
        let mut layer: Vec<Option<Vec<Interval>>> = states
            .iter()
            .map(|yes| yes.then(|| initial.clone()))
            .collect();
        for _remaining in (1..=COMPUTE_PROOF_STATES).rev() {
            let mut next: Vec<Option<Vec<Interval>>> = vec![None; states.len()];
            for (state, input) in layer.iter().enumerate() {
                let Some(input) = input else { continue };
                budget.spend(count)?;
                let mut values = input.clone();
                for (index, operation) in function.states[state].operations.iter().enumerate() {
                    budget.spend(1)?;
                    let proven = transfer(&mut values, operation)?;
                    operations[state][index] &= proven;
                }
                let successors = match function.states[state].terminator {
                    TaskTerminator::Jump { target } => [Some(target), None],
                    TaskTerminator::Branch {
                        then_state,
                        else_state,
                        ..
                    } => [Some(then_state), Some(else_state)],
                    TaskTerminator::Suspend { resume, .. } => [Some(resume), None],
                    TaskTerminator::ScopeDrain { ready, .. } => [Some(ready), None],
                    _ => return None,
                };
                for target in successors.into_iter().flatten() {
                    if !states[target.0] {
                        continue;
                    }
                    budget.spend(count)?;
                    if let Some(existing) = &mut next[target.0] {
                        for (old, incoming) in existing.iter_mut().zip(&values) {
                            old.low = old.low.min(incoming.low);
                            old.high = old.high.max(incoming.high);
                        }
                    } else {
                        next[target.0] = Some(values.clone());
                    }
                }
            }
            layer = next;
        }
        operations.iter().flatten().any(|yes| *yes).then_some(Self {
            guard_slots,
            states,
            operations,
        })
    }
}

fn transfer(values: &mut [Interval], operation: &TaskOp) -> Option<bool> {
    match operation {
        TaskOp::Init { dst, value } => {
            values[dst.0] = match value {
                TaskConstant::Int(value) => Interval::constant(*value),
                TaskConstant::Bool(value) => Interval::constant(i64::from(*value)),
                TaskConstant::Null => Interval::ZERO,
                _ => return None,
            };
            Some(false)
        }
        TaskOp::Copy { dst, src } | TaskOp::Move { dst, src } => {
            values[dst.0] = values[src.0];
            // Copy-only Move clears an init bit, not the source's C storage.
            Some(false)
        }
        TaskOp::Unary { dst, op, src } => {
            let (value, proven) = match op {
                TaskUnaryOp::Not => (Interval::BOOLEAN, false),
                TaskUnaryOp::Negate => {
                    Interval::checked(-(values[src.0].high as i128), -(values[src.0].low as i128))
                }
            };
            values[dst.0] = value;
            Some(proven)
        }
        TaskOp::Binary {
            dst,
            op,
            left,
            right,
        } => {
            let (value, proven) = values[left.0].binary(*op, values[right.0]);
            values[dst.0] = value;
            Some(proven)
        }
        TaskOp::Read { .. } | TaskOp::Drop { .. } | TaskOp::DropIfInit { .. } => Some(false),
        TaskOp::ScopeEnter { tasks, .. } if tasks.is_empty() => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::task::*;

    fn repeated(op: TaskBinaryOp, constant: Option<i64>) -> TaskFunction {
        TaskFunction {
            id: TaskFunctionId(0),
            name: "range-fixture".into(),
            slots: (0..3)
                .map(|_| TaskSlot {
                    ty: TaskSlotType::Value {
                        ty: IrType::Int,
                        borrowed: false,
                    },
                })
                .collect(),
            parameters: vec![SlotId(0)],
            entry: StateId(0),
            result: IrType::Result(Box::new(IrType::Int)),
            states: vec![TaskState {
                operations: {
                    let mut ops = Vec::new();
                    if let Some(value) = constant {
                        ops.push(TaskOp::Init {
                            dst: SlotId(1),
                            value: TaskConstant::Int(value),
                        });
                    }
                    ops.push(TaskOp::Binary {
                        dst: SlotId(2),
                        op,
                        left: SlotId(0),
                        right: SlotId(if constant.is_some() { 1 } else { 0 }),
                    });
                    ops.push(TaskOp::Copy {
                        dst: SlotId(0),
                        src: SlotId(2),
                    });
                    ops
                },
                terminator: TaskTerminator::Jump { target: StateId(0) },
            }],
        }
    }

    #[test]
    fn range_proof_unrolls_growth_instead_of_assuming_a_loop_invariant() {
        let square = repeated(TaskBinaryOp::Multiply, None);
        assert!(RangePlan::build(
            &square,
            vec![true],
            |_| true,
            |_| false,
            &mut WorkBudget::default()
        )
        .is_none());
        let increment = repeated(TaskBinaryOp::Add, Some(1));
        let plan = RangePlan::build(
            &increment,
            vec![true],
            |_| true,
            |_| false,
            &mut WorkBudget::default(),
        )
        .unwrap();
        assert!(plan.proves(0, 1));
        assert!(RangePlan::build(
            &increment,
            vec![true],
            |_| true,
            |_| false,
            &mut WorkBudget(0)
        )
        .is_none());
    }

    #[test]
    fn range_proof_covers_every_borrowed_bank_without_restarting_local_facts() {
        assert_eq!(COMPUTE_MAX_BANKS, 4);
        assert_eq!(COMPUTE_PUBLIC_SELECTION_CREDITS, 32);
        assert_eq!(COMPUTE_PRIVATE_SELECTION_CREDITS, 128);
        assert!(COMPUTE_RESERVATION_CREDITS <= COMPUTE_PRIVATE_SELECTION_CREDITS);
        assert_eq!(
            COMPUTE_BANK_STATES * COMPUTE_RESERVATION_CREDITS,
            COMPUTE_RESERVATION_STATES
        );
        assert!(COMPUTE_PROOF_STATES >= COMPUTE_RESERVATION_STATES);
        assert!(COMPUTE_PROOF_STATES >= COMPUTE_BANK_STATES * COMPUTE_MAX_BANKS);
        assert!(COMPUTE_PROOF_STATES >= COMPUTE_BANK_STATES * COMPUTE_PRIVATE_SELECTION_CREDITS);
        let increment = i64::MAX / 4096;
        // Safe over the complete old1024-state callback, but not the new one.
        // Renewing an old certificate without reconstructing local facts
        // would certify signed UB after the first old horizon.
        let first = i32::MAX as i128;
        assert!(first + increment as i128 * COMPUTE_BANK_STATES as i128 <= i64::MAX as i128);
        assert!(first + increment as i128 * 256i128 <= i64::MAX as i128);
        assert!(first + increment as i128 * 1024i128 <= i64::MAX as i128);
        assert!(first + increment as i128 * COMPUTE_PROOF_STATES as i128 > i64::MAX as i128);
        let grows = repeated(TaskBinaryOp::Add, Some(increment));
        assert!(RangePlan::build(
            &grows,
            vec![true],
            |_| true,
            |_| false,
            &mut WorkBudget::default(),
        )
        .is_none());
        // Exhausting the finite analysis budget remains an unknown proof,
        // never an instruction to emit unchecked arithmetic.
        assert!(
            RangePlan::build(&grows, vec![true], |_| true, |_| false, &mut WorkBudget(0),)
                .is_none()
        );
    }

    #[test]
    fn range_work_and_extra_source_budgets_are_shared_and_exhaustion_declines_optimization() {
        let increment = repeated(TaskBinaryOp::Add, Some(1));
        let mut measured = WorkBudget::default();
        assert!(
            RangePlan::build(&increment, vec![true], |_| true, |_| false, &mut measured).is_some()
        );
        let cost = MAX_WORK - measured.0;
        assert!(cost > 0);
        let mut shared = WorkBudget(cost * 2 - 1);
        assert!(
            RangePlan::build(&increment, vec![true], |_| true, |_| false, &mut shared).is_some()
        );
        assert!(
            RangePlan::build(&increment, vec![true], |_| true, |_| false, &mut shared).is_none()
        );
        assert_eq!(shared.0, 0);
        assert!(
            RangePlan::build(&increment, vec![true], |_| true, |_| false, &mut shared).is_none()
        );
        let _scope = GenerationScope::new();
        assert!(claim_extra(100_000));
        assert!(claim_extra(100_000));
        assert!(claim_extra(MAX_EXTRA_SOURCE_BYTES - 200_000));
        assert!(!claim_extra(1));
    }

    #[test]
    fn range_unknown_persistent_inputs_and_copy_initialization_are_not_invented() {
        let mut unknown = repeated(TaskBinaryOp::Multiply, None);
        unknown.parameters.push(SlotId(1));
        let TaskOp::Binary { right, .. } = &mut unknown.states[0].operations[0] else {
            unreachable!()
        };
        *right = SlotId(1);
        assert!(RangePlan::build(
            &unknown,
            vec![true],
            |slot| slot == 0,
            |_| false,
            &mut WorkBudget::default()
        )
        .is_none());
        let mut values = [Interval::constant(5), Interval::ZERO];
        transfer(
            &mut values,
            &TaskOp::Move {
                dst: SlotId(1),
                src: SlotId(0),
            },
        )
        .unwrap();
        transfer(&mut values, &TaskOp::DropIfInit { slot: SlotId(0) }).unwrap();
        assert_eq!(values, [Interval::constant(5), Interval::constant(5)]);
    }

    #[test]
    fn range_proof_keeps_i64_division_and_remainder_failures_and_signed_remainders() {
        for operation in [TaskBinaryOp::Divide, TaskBinaryOp::Remainder] {
            assert!(!Interval::FULL.binary(operation, Interval::constant(-1)).1);
            assert!(!Interval::GUARDED.binary(operation, Interval::constant(0)).1);
            assert!(
                Interval::GUARDED
                    .binary(operation, Interval::constant(-1))
                    .1
            );
        }
        assert_eq!(
            Interval { low: -9, high: -4 }.binary(TaskBinaryOp::Remainder, Interval::constant(3)),
            (Interval { low: -2, high: 0 }, true)
        );
        assert_eq!(
            Interval { low: -9, high: 9 }.binary(TaskBinaryOp::Remainder, Interval::constant(-3)),
            (Interval { low: -2, high: 2 }, true)
        );
        assert!(
            !Interval::FULL
                .binary(TaskBinaryOp::Multiply, Interval::FULL)
                .1
        );
        assert!(!Interval::FULL.binary(TaskBinaryOp::Add, Interval::FULL).1);
    }

    #[test]
    fn certified_interval_operations_contain_checked_i64_oracle_results() {
        let ranges = [
            Interval::FULL,
            Interval::GUARDED,
            Interval::ZERO,
            Interval::constant(i64::MIN),
            Interval::constant(i64::MAX),
            Interval::constant(-1),
            Interval { low: -11, high: -2 },
            Interval { low: -9, high: 9 },
            Interval { low: 2, high: 11 },
        ];
        for left in ranges {
            for right in ranges {
                for operation in [
                    TaskBinaryOp::Add,
                    TaskBinaryOp::Subtract,
                    TaskBinaryOp::Multiply,
                    TaskBinaryOp::Divide,
                    TaskBinaryOp::Remainder,
                ] {
                    let (output, certified) = left.binary(operation, right);
                    if !certified {
                        continue;
                    }
                    for a in [left.low, left.high, -1, 0, 1]
                        .into_iter()
                        .filter(|a| left.contains(*a))
                    {
                        for b in [right.low, right.high, -1, 0, 1]
                            .into_iter()
                            .filter(|b| right.contains(*b))
                        {
                            let actual = match operation {
                                TaskBinaryOp::Add => a.checked_add(b),
                                TaskBinaryOp::Subtract => a.checked_sub(b),
                                TaskBinaryOp::Multiply => a.checked_mul(b),
                                TaskBinaryOp::Divide => a.checked_div(b),
                                TaskBinaryOp::Remainder => a.checked_rem(b),
                                _ => unreachable!(),
                            }
                            .expect("a certified operation cannot fault");
                            assert!(
                                output.contains(actual),
                                "{left:?} {operation:?} {right:?}: {actual} outside {output:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn range_candidate_fallback_restores_nested_state_and_returns_the_original_error() {
        let _scope = GenerationScope::new();
        let calls = Cell::new(0);
        let result = generate_with_fallback(|| {
            calls.set(calls.get() + 1);
            if enabled() {
                attempted();
                assert!(claim_extra(MAX_EXTRA_SOURCE_BYTES));
                assert!(!claim_extra(1));
                Err(crate::error::KuError::runtime(
                    "candidate",
                    Default::default(),
                ))
            } else {
                assert!(!enabled());
                let nested: crate::error::KuResult<()> = generate_with_fallback(|| {
                    assert!(!enabled());
                    Err(crate::error::KuError::runtime(
                        "original",
                        Default::default(),
                    ))
                });
                nested
            }
        });
        assert_eq!(calls.get(), 2);
        assert!(result.unwrap_err().to_string().contains("original"));
        assert!(enabled());
        let calls = Cell::new(0);
        let result: crate::error::KuResult<()> = generate_with_fallback(|| {
            calls.set(calls.get() + 1);
            Err(crate::error::KuError::runtime(
                "no candidate",
                Default::default(),
            ))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
        assert!(enabled());
    }

    #[test]
    fn range_extra_source_cannot_displace_a_previously_fitting_whole_file() {
        use crate::{checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
        let ast = Parser::new(Lexer::new("async fn Work(value:int):int! { total=value; i=0; while(i<5000) { total=(total*33+i)%100000000; i=i+1 } return ok(total) } async fn main():null! { return ok(null) }").lex().unwrap()).parse_program().unwrap();
        Checker::new().check(&ast).unwrap();
        let native = task_lower::lower_program(&ast).unwrap();
        let plan = verify_and_plan(&native.tasks, Default::default()).unwrap();
        let sync = crate::ir::IrProgram {
            functions: Vec::new(),
            layouts: crate::ir::IrLayoutTable {
                structs: Vec::new(),
                enums: Vec::new(),
            },
        };
        let frames = Some((&native.tasks, &plan, Some(native.entry)));
        let _scope = GenerationScope::new();
        GENERATION.with(|state| {
            state.set(GenerationState {
                disabled: true,
                ..state.get()
            })
        });
        let baseline = super::super::generate_c_source_with_frames_bounded(
            &sync,
            &Default::default(),
            64 * 1024 * 1024,
            frames,
        )
        .unwrap();
        GENERATION.with(|state| {
            state.set(GenerationState {
                disabled: false,
                ..state.get()
            })
        });
        let candidate = super::super::generate_c_source_with_frames_bounded(
            &sync,
            &Default::default(),
            64 * 1024 * 1024,
            frames,
        )
        .unwrap();
        assert!(candidate.contains("ku_task_range_state_"));
        assert!(candidate.len() > baseline.len());
        assert!(candidate.len() - baseline.len() <= MAX_EXTRA_SOURCE_BYTES);
        println!(
            "range source bytes: original={} candidate={} extra={}",
            baseline.len(),
            candidate.len(),
            candidate.len() - baseline.len()
        );
        let exact = super::super::generate_c_source_with_frames_bounded(
            &sync,
            &Default::default(),
            baseline.len(),
            frames,
        )
        .unwrap();
        assert_eq!(exact, baseline);
        assert!(super::super::generate_c_source_with_frames_bounded(
            &sync,
            &Default::default(),
            baseline.len() - 1,
            frames
        )
        .is_err());
        assert!(enabled());
    }
}
