//! Experimental typed frame IR. This is deliberately separate from synchronous
//! IR and does not enable source async lowering, task submission or awaiting.
//! Limits bound validation and planning of an already constructed frame graph.

use std::collections::{HashSet, VecDeque};

use super::IrType;
use crate::{error::KuError, error::KuResult, span::Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskFunctionId(pub usize);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub usize);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProgram {
    pub functions: Vec<TaskFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFunction {
    pub id: TaskFunctionId,
    pub name: String,
    pub slots: Vec<TaskSlot>,
    pub parameters: Vec<SlotId>,
    pub entry: StateId,
    pub states: Vec<TaskState>,
    /// The completion payload, including its single Result layer.
    pub result: IrType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSlot {
    pub ty: TaskSlotType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSlotType {
    Value { ty: IrType, borrowed: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub operations: Vec<TaskOp>,
    pub terminator: TaskTerminator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskConstant {
    Int(i64),
    Bool(bool),
    Null,
    Str(String),
    Ok(Box<TaskConstant>),
    Err {
        result: IrType,
        domain: String,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOp {
    Init { dst: SlotId, value: TaskConstant },
    Copy { dst: SlotId, src: SlotId },
    Move { dst: SlotId, src: SlotId },
    Read { slot: SlotId },
    Drop { slot: SlotId },
    DropIfInit { slot: SlotId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskTerminator {
    Jump {
        target: StateId,
    },
    Branch {
        condition: SlotId,
        then_state: StateId,
        else_state: StateId,
    },
    Suspend {
        resume: StateId,
        cleanup: StateId,
    },
    Complete {
        value: SlotId,
    },
    /// Only valid in the cancellation region reached from Suspend.cleanup.
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskLimits {
    pub max_functions: usize,
    pub max_states: usize,
    pub max_slots: usize,
    pub max_operations: usize,
    pub max_literal_bytes: usize,
    pub max_analysis_work: usize,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            max_functions: 64,
            max_states: 256,
            max_slots: 64,
            max_operations: 4096,
            max_literal_bytes: 1_000_000,
            max_analysis_work: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFramePlan {
    pub functions: Vec<TaskFunctionFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFunctionFrame {
    pub function: TaskFunctionId,
    /// Original dense SlotIds: parameters plus live values crossing suspension.
    pub slots: Vec<SlotId>,
    pub suspensions: Vec<TaskSuspensionLive>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSuspensionLive {
    pub state: StateId,
    /// Union of normal-resume and cancellation-cleanup live values.
    pub slots: Vec<SlotId>,
}

fn invalid(message: &str) -> KuError {
    KuError::runtime(format!("invalid task frame IR: {message}"), Span::default())
}

fn primitive(ty: &IrType) -> bool {
    matches!(ty, IrType::Int | IrType::Bool | IrType::Null | IrType::Str)
}

fn value_type(ty: &IrType) -> bool {
    primitive(ty) || matches!(ty, IrType::Result(inner) if primitive(inner))
}

fn result_type(ty: &IrType) -> bool {
    matches!(ty, IrType::Result(inner) if primitive(inner))
}

fn copy_type(ty: &IrType) -> bool {
    matches!(ty, IrType::Int | IrType::Bool | IrType::Null)
}

fn slot_type(slot: &TaskSlot) -> (&IrType, bool) {
    let TaskSlotType::Value { ty, borrowed } = &slot.ty;
    (ty, *borrowed)
}

fn bit(slot: SlotId) -> u64 {
    1u64 << slot.0
}

fn slots(mask: u64) -> Vec<SlotId> {
    (0..64)
        .filter(|index| mask & (1u64 << index) != 0)
        .map(SlotId)
        .collect()
}

fn add_bounded(total: &mut usize, amount: usize, limit: usize, message: &str) -> KuResult<()> {
    *total = total
        .checked_add(amount)
        .filter(|sum| *sum <= limit)
        .ok_or_else(|| invalid(message))?;
    Ok(())
}

struct Budget {
    remaining: usize,
}

impl Budget {
    fn spend(&mut self) -> KuResult<()> {
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| invalid("analysis work limit exceeded"))?;
        Ok(())
    }
}

fn constant_type(value: &TaskConstant) -> KuResult<IrType> {
    fn scalar(value: &TaskConstant) -> KuResult<IrType> {
        match value {
            TaskConstant::Int(_) => Ok(IrType::Int),
            TaskConstant::Bool(_) => Ok(IrType::Bool),
            TaskConstant::Null => Ok(IrType::Null),
            TaskConstant::Str(_) => Ok(IrType::Str),
            _ => Err(invalid("constant supports only one Result layer")),
        }
    }
    match value {
        TaskConstant::Ok(inner) => Ok(IrType::Result(Box::new(scalar(inner)?))),
        TaskConstant::Err { result, .. } if result_type(result) => Ok(result.clone()),
        TaskConstant::Err { .. } => Err(invalid("error constant has an unsupported result type")),
        _ => scalar(value),
    }
}

fn preflight(program: &TaskProgram, limits: TaskLimits) -> KuResult<()> {
    let hard = TaskLimits::default();
    if limits.max_functions > hard.max_functions
        || limits.max_states > hard.max_states
        || limits.max_slots > hard.max_slots
        || limits.max_operations > hard.max_operations
        || limits.max_literal_bytes > hard.max_literal_bytes
        || limits.max_analysis_work > hard.max_analysis_work
    {
        return Err(invalid("custom limits may only tighten hard limits"));
    }
    if program.functions.len() > limits.max_functions {
        return Err(invalid("function limit exceeded"));
    }
    let mut state_count = 0;
    let mut slot_count = 0;
    let mut operation_count = 0;
    let mut literal_bytes = 0;
    let state_limit = limits
        .max_functions
        .checked_mul(limits.max_states)
        .ok_or_else(|| invalid("state budget overflow"))?;
    let slot_limit = limits
        .max_functions
        .checked_mul(limits.max_slots)
        .ok_or_else(|| invalid("slot budget overflow"))?;
    for function in &program.functions {
        if function.states.len() > limits.max_states || function.slots.len() > limits.max_slots {
            return Err(invalid("per-function state or slot limit exceeded"));
        }
        if function.parameters.len() > function.slots.len() {
            return Err(invalid("parameter count exceeds slot count"));
        }
        add_bounded(
            &mut state_count,
            function.states.len(),
            state_limit,
            "total state limit exceeded",
        )?;
        add_bounded(
            &mut slot_count,
            function.slots.len(),
            slot_limit,
            "total slot limit exceeded",
        )?;
        add_bounded(
            &mut literal_bytes,
            function.name.len(),
            limits.max_literal_bytes,
            "literal byte limit exceeded",
        )?;
        // Inspect the shape without recursively formatting or cloning raw types.
        if !result_type(&function.result) {
            return Err(invalid(
                "function completion must be Result of a supported primitive",
            ));
        }
        for slot in &function.slots {
            if !value_type(slot_type(slot).0) {
                return Err(invalid("unsupported slot payload type"));
            }
        }
        for state in &function.states {
            add_bounded(
                &mut operation_count,
                state.operations.len(),
                limits.max_operations,
                "operation limit exceeded",
            )?;
            for operation in &state.operations {
                if let TaskOp::Init { value, .. } = operation {
                    let leaf = match value {
                        TaskConstant::Ok(inner) => inner.as_ref(),
                        other => other,
                    };
                    match leaf {
                        TaskConstant::Str(text) => add_bounded(
                            &mut literal_bytes,
                            text.len(),
                            limits.max_literal_bytes,
                            "literal byte limit exceeded",
                        )?,
                        TaskConstant::Err {
                            result,
                            domain,
                            code,
                            message,
                        } => {
                            if !result_type(result) || matches!(value, TaskConstant::Ok(_)) {
                                return Err(invalid("constant supports only one Result layer"));
                            }
                            for text in [domain, code, message] {
                                add_bounded(
                                    &mut literal_bytes,
                                    text.len(),
                                    limits.max_literal_bytes,
                                    "literal byte limit exceeded",
                                )?;
                            }
                        }
                        TaskConstant::Ok(_) => {
                            return Err(invalid("constant supports only one Result layer"))
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn successors(term: &TaskTerminator) -> [Option<StateId>; 2] {
    match term {
        TaskTerminator::Jump { target } => [Some(*target), None],
        TaskTerminator::Branch {
            then_state,
            else_state,
            ..
        } => [Some(*then_state), Some(*else_state)],
        TaskTerminator::Suspend { resume, cleanup } => [Some(*resume), Some(*cleanup)],
        TaskTerminator::Complete { .. } | TaskTerminator::Terminate => [None, None],
    }
}

fn validate_shape(function: &TaskFunction, budget: &mut Budget) -> KuResult<(u64, u64, u64)> {
    let get_slot = |id: SlotId| {
        function
            .slots
            .get(id.0)
            .ok_or_else(|| invalid("unknown slot"))
    };
    if function.entry.0 >= function.states.len() {
        return Err(invalid("unknown entry state"));
    }
    let mut parameters = 0;
    let mut owned = 0;
    let mut borrowed = 0;
    for (index, slot) in function.slots.iter().enumerate() {
        let (ty, is_borrowed) = slot_type(slot);
        if is_borrowed {
            borrowed |= bit(SlotId(index));
        } else if !copy_type(ty) {
            owned |= bit(SlotId(index));
        }
    }
    for &parameter in &function.parameters {
        let (_, is_borrowed) = slot_type(get_slot(parameter)?);
        if is_borrowed {
            return Err(invalid("borrowed parameter cannot enter a task frame"));
        }
        if parameters & bit(parameter) != 0 {
            return Err(invalid("duplicate parameter slot"));
        }
        parameters |= bit(parameter);
    }
    for state in &function.states {
        budget.spend()?;
        for operation in &state.operations {
            budget.spend()?;
            match operation {
                TaskOp::Init { dst, value } => {
                    if slot_type(get_slot(*dst)?).0 != &constant_type(value)? {
                        return Err(invalid("initializer type does not match slot"));
                    }
                }
                TaskOp::Copy { dst, src } | TaskOp::Move { dst, src } => {
                    let (destination, destination_borrowed) = slot_type(get_slot(*dst)?);
                    let (source, source_borrowed) = slot_type(get_slot(*src)?);
                    if dst == src || destination != source || destination_borrowed {
                        return Err(invalid("copy or move has invalid source/destination types"));
                    }
                    match operation {
                        TaskOp::Copy { .. } if !copy_type(source) => {
                            return Err(invalid("owned payload cannot be implicitly copied"))
                        }
                        TaskOp::Move { .. } if source_borrowed || copy_type(source) => {
                            return Err(invalid("move requires an owned source"))
                        }
                        _ => {}
                    }
                }
                TaskOp::Read { slot } => {
                    get_slot(*slot)?;
                }
                TaskOp::Drop { slot } | TaskOp::DropIfInit { slot } => {
                    if slot_type(get_slot(*slot)?).1 {
                        return Err(invalid("cannot drop a borrowed slot"));
                    }
                }
            }
        }
        for target in successors(&state.terminator).into_iter().flatten() {
            if target.0 >= function.states.len() {
                return Err(invalid("unknown successor state"));
            }
        }
        match &state.terminator {
            TaskTerminator::Branch { condition, .. }
                if slot_type(get_slot(*condition)?).0 != &IrType::Bool =>
            {
                return Err(invalid("branch condition must be bool"))
            }
            TaskTerminator::Complete { value } => {
                let (ty, is_borrowed) = slot_type(get_slot(*value)?);
                if ty != &function.result || is_borrowed {
                    return Err(invalid("completion must move the matching owned Result"));
                }
            }
            _ => {}
        }
    }
    Ok((parameters, owned, borrowed))
}

fn validate_regions_and_progress(function: &TaskFunction, budget: &mut Budget) -> KuResult<()> {
    let count = function.states.len();
    let mut normal = vec![false; count];
    let mut pending = vec![function.entry];
    while let Some(id) = pending.pop() {
        budget.spend()?;
        if normal[id.0] {
            continue;
        }
        normal[id.0] = true;
        match &function.states[id.0].terminator {
            TaskTerminator::Suspend { resume, .. } => pending.push(*resume),
            TaskTerminator::Terminate => {
                return Err(invalid(
                    "normal execution cannot terminate without a cancellation context",
                ))
            }
            term => pending.extend(successors(term).into_iter().flatten()),
        }
    }
    let mut cleanup = vec![false; count];
    for state in &function.states {
        if let TaskTerminator::Suspend {
            cleanup: target, ..
        } = state.terminator
        {
            pending.push(target);
        }
    }
    while let Some(id) = pending.pop() {
        budget.spend()?;
        if normal[id.0] {
            return Err(invalid("cleanup cannot reenter normal execution"));
        }
        if cleanup[id.0] {
            continue;
        }
        cleanup[id.0] = true;
        match &function.states[id.0].terminator {
            TaskTerminator::Jump { .. } | TaskTerminator::Branch { .. } => pending.extend(
                successors(&function.states[id.0].terminator)
                    .into_iter()
                    .flatten(),
            ),
            TaskTerminator::Terminate => {}
            _ => return Err(invalid("cleanup cannot complete or suspend")),
        }
    }
    // Every cycle must cross an actual suspension. This also rejects all cleanup
    // cycles: R1 has no budget-poll instruction with which to bound them.
    let mut incoming = vec![0usize; count];
    for state in &function.states {
        if matches!(state.terminator, TaskTerminator::Suspend { .. }) {
            continue;
        }
        for target in successors(&state.terminator).into_iter().flatten() {
            incoming[target.0] += 1;
        }
    }
    let mut ready: VecDeque<usize> = incoming
        .iter()
        .enumerate()
        .filter_map(|(id, &edges)| (edges == 0).then_some(id))
        .collect();
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        budget.spend()?;
        visited += 1;
        if matches!(
            function.states[id].terminator,
            TaskTerminator::Suspend { .. }
        ) {
            continue;
        }
        for target in successors(&function.states[id].terminator)
            .into_iter()
            .flatten()
        {
            incoming[target.0] -= 1;
            if incoming[target.0] == 0 {
                ready.push_back(target.0);
            }
        }
    }
    if visited != count {
        return Err(invalid("cycle without suspension is not supported"));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Initialized {
    must: u64,
    may: u64,
}

fn transfer(mut state: Initialized, operation: &TaskOp) -> Initialized {
    match operation {
        TaskOp::Init { dst, .. } | TaskOp::Copy { dst, .. } => {
            state.must |= bit(*dst);
            state.may |= bit(*dst);
        }
        TaskOp::Move { dst, src } => {
            state.must = (state.must & !bit(*src)) | bit(*dst);
            state.may = (state.may & !bit(*src)) | bit(*dst);
        }
        TaskOp::Drop { slot } | TaskOp::DropIfInit { slot } => {
            state.must &= !bit(*slot);
            state.may &= !bit(*slot);
        }
        TaskOp::Read { .. } => {}
    }
    state
}

fn initialized_states(
    function: &TaskFunction,
    parameters: u64,
    budget: &mut Budget,
) -> KuResult<Vec<Option<Initialized>>> {
    let mut inputs = vec![None; function.states.len()];
    inputs[function.entry.0] = Some(Initialized {
        must: parameters,
        may: parameters,
    });
    let mut queued = vec![false; function.states.len()];
    queued[function.entry.0] = true;
    let mut pending = VecDeque::from([function.entry]);
    while let Some(id) = pending.pop_front() {
        budget.spend()?;
        queued[id.0] = false;
        let mut output = inputs[id.0].expect("queued states have input facts");
        for operation in &function.states[id.0].operations {
            budget.spend()?;
            output = transfer(output, operation);
        }
        for target in successors(&function.states[id.0].terminator)
            .into_iter()
            .flatten()
        {
            budget.spend()?;
            let merged = match inputs[target.0] {
                Some(existing) => Initialized {
                    must: existing.must & output.must,
                    may: existing.may | output.may,
                },
                None => output,
            };
            if inputs[target.0] != Some(merged) {
                inputs[target.0] = Some(merged);
                if !queued[target.0] {
                    queued[target.0] = true;
                    pending.push_back(target);
                }
            }
        }
    }
    Ok(inputs)
}

fn live_states(function: &TaskFunction, budget: &mut Budget) -> KuResult<Vec<u64>> {
    let mut live_in = vec![0u64; function.states.len()];
    let mut live_out = vec![0u64; function.states.len()];
    loop {
        let mut changed = false;
        for (id, state) in function.states.iter().enumerate().rev() {
            budget.spend()?;
            let output = successors(&state.terminator)
                .into_iter()
                .flatten()
                .fold(0, |live, next| live | live_in[next.0]);
            let mut live = output;
            match state.terminator {
                TaskTerminator::Branch { condition, .. } => live |= bit(condition),
                TaskTerminator::Complete { value } => live |= bit(value),
                _ => {}
            }
            for operation in state.operations.iter().rev() {
                budget.spend()?;
                match operation {
                    TaskOp::Init { dst, .. } => live &= !bit(*dst),
                    TaskOp::Copy { dst, src } | TaskOp::Move { dst, src } => {
                        live = (live & !bit(*dst)) | bit(*src)
                    }
                    TaskOp::Read { slot } | TaskOp::Drop { slot } | TaskOp::DropIfInit { slot } => {
                        live |= bit(*slot)
                    }
                }
            }
            changed |= live_in[id] != live;
            live_in[id] = live;
            live_out[id] = output;
        }
        if !changed {
            return Ok(live_out);
        }
    }
}

fn validate_ownership_and_plan(
    function: &TaskFunction,
    parameters: u64,
    owned: u64,
    borrowed: u64,
    inputs: &[Option<Initialized>],
    live_out: &[u64],
    budget: &mut Budget,
) -> KuResult<TaskFunctionFrame> {
    let mut frame = parameters;
    let mut suspensions = Vec::new();
    for (id, block) in function.states.iter().enumerate() {
        let Some(mut state) = inputs[id] else {
            continue;
        };
        budget.spend()?;
        for operation in &block.operations {
            budget.spend()?;
            let source = match operation {
                TaskOp::Copy { src, .. } | TaskOp::Move { src, .. } => Some(*src),
                TaskOp::Read { slot } | TaskOp::Drop { slot } => Some(*slot),
                _ => None,
            };
            if source.is_some_and(|slot| state.must & bit(slot) == 0) {
                return Err(invalid(
                    "read, move or drop of a slot that is not definitely initialized",
                ));
            }
            let destination = match operation {
                TaskOp::Init { dst, .. } | TaskOp::Copy { dst, .. } | TaskOp::Move { dst, .. } => {
                    Some(*dst)
                }
                _ => None,
            };
            if destination.is_some_and(|slot| state.may & owned & bit(slot) != 0) {
                return Err(invalid(
                    "overwriting a possibly initialized owned slot without drop",
                ));
            }
            state = transfer(state, operation);
        }
        match block.terminator {
            TaskTerminator::Branch { condition, .. } if state.must & bit(condition) == 0 => {
                return Err(invalid("branch reads an uninitialized condition"))
            }
            TaskTerminator::Complete { value } => {
                if state.must & bit(value) == 0 {
                    return Err(invalid("completion reads an uninitialized Result"));
                }
                if state.may & owned & !bit(value) != 0 {
                    return Err(invalid("completion leaves owned slots without cleanup"));
                }
            }
            TaskTerminator::Terminate if state.may & owned != 0 => {
                return Err(invalid("termination leaves owned slots without cleanup"))
            }
            TaskTerminator::Suspend { .. } => {
                let saved = live_out[id] & state.may;
                if saved & borrowed != 0 {
                    return Err(invalid("borrowed value cannot cross suspension"));
                }
                if state.may & owned & !saved != 0 {
                    return Err(invalid(
                        "dead owned slot must be explicitly dropped before suspension",
                    ));
                }
                frame |= saved;
                suspensions.push(TaskSuspensionLive {
                    state: StateId(id),
                    slots: slots(saved),
                });
            }
            _ => {}
        }
    }
    Ok(TaskFunctionFrame {
        function: function.id,
        slots: slots(frame),
        suspensions,
    })
}

/// Validate types, cancellation regions, cooperative progress and fixed-point
/// ownership before deciding which values actually need persistent frame slots.
pub fn verify_and_plan(program: &TaskProgram, limits: TaskLimits) -> KuResult<TaskFramePlan> {
    preflight(program, limits)?;
    let mut budget = Budget {
        remaining: limits.max_analysis_work,
    };
    let mut ids = HashSet::new();
    let mut functions = Vec::new();
    for function in &program.functions {
        budget.spend()?;
        if !ids.insert(function.id) {
            return Err(invalid("duplicate function id"));
        }
        let (parameters, owned, borrowed) = validate_shape(function, &mut budget)?;
        validate_regions_and_progress(function, &mut budget)?;
        let inputs = initialized_states(function, parameters, &mut budget)?;
        let live = live_states(function, &mut budget)?;
        functions.push(validate_ownership_and_plan(
            function,
            parameters,
            owned,
            borrowed,
            &inputs,
            &live,
            &mut budget,
        )?);
    }
    Ok(TaskFramePlan { functions })
}
