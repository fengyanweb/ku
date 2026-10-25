//! Bounded normal-flow regions over already verified typed Task states.
//!
//! The backend supplies its exact no-host/no-owner predicate. Normal entries,
//! joins and loop latches cut regions. Every other member has one predecessor,
//! so its region entry dominates it; Kahn ordering independently rejects an
//! uncut cycle. Existing cooperative liveness still reserves every slot needed
//! by the precise fallback. This analysis never changes the typed program.

use std::collections::VecDeque;

use crate::ir::task::{TaskFunction, TaskTerminator};

pub(super) const MAX_REGION_STATES: usize = 64;
pub(super) const MAX_REGION_OPERATIONS: usize = 256;
const MAX_ANALYSIS_STATES: usize = 512;
pub(super) const MAX_LOOP_ITERATIONS: usize = 8;
pub(super) const MAX_LOOP_OPERATIONS: usize = 256;
const MAX_LOOP_TOTAL_STATES: usize = 128;

/// One source-loop iteration is a finite DAG after removing its unique
/// automatic Suspend backedge. No other external entry or real suspend is
/// admitted. Costs include the header and the taken exit/latch state, but not
/// the destination outside this loop. The emitter can therefore reserve only
/// capacity, then charge the actual path (including a faulting state).
#[derive(Default)]
pub(super) struct ScalarLoops {
    pub loops: Vec<ScalarLoop>,
    entries: Vec<Option<usize>>,
}

pub(super) struct ScalarLoop {
    pub header: usize,
    pub latch: usize,
    pub states: Vec<usize>,
    pub blocks: Vec<Vec<usize>>,
    pub max_path_states: usize,
    pub max_path_operations: usize,
    pub max_iterations: usize,
    pub private_max_iterations: usize,
}

pub(super) struct LinearLoop {
    pub body: Vec<usize>,
    pub exit: usize,
    pub body_when_true: bool,
}

impl ScalarLoop {
    /// The small outlined form has one conditional header and a single linear
    /// successful path. Every other shape keeps the existing bounded CFG.
    pub fn linear(&self, function: &TaskFunction) -> Option<LinearLoop> {
        let TaskTerminator::Branch {
            then_state,
            else_state,
            ..
        } = function.states[self.header].terminator
        else {
            return None;
        };
        let inside = |s: usize| s != self.header && self.states.binary_search(&s).is_ok();
        let (mut next, exit, body_when_true) = match (inside(then_state.0), inside(else_state.0)) {
            (true, false) if self.states.binary_search(&else_state.0).is_err() => {
                (then_state.0, else_state.0, true)
            }
            (false, true) if self.states.binary_search(&then_state.0).is_err() => {
                (else_state.0, then_state.0, false)
            }
            _ => return None,
        };
        let mut body = Vec::new();
        while body.len() < self.states.len() {
            if !inside(next) || body.contains(&next) {
                return None;
            }
            body.push(next);
            match function.states[next].terminator {
                TaskTerminator::Jump { target }
                | TaskTerminator::ScopeDrain { ready: target, .. } => next = target.0,
                TaskTerminator::Suspend { resume, .. }
                    if next == self.latch && resume.0 == self.header =>
                {
                    break
                }
                _ => return None,
            }
        }
        if body.last().copied() != Some(self.latch) || body.len() + 1 != self.states.len() {
            return None;
        }
        Some(LinearLoop {
            body,
            exit,
            body_when_true,
        })
    }
}

impl ScalarLoops {
    pub fn at_header(&self, state: usize) -> Option<&ScalarLoop> {
        self.entries
            .get(state)
            .copied()
            .flatten()
            .map(|id| &self.loops[id])
    }

    pub fn build(function: &TaskFunction, eligible: &[bool]) -> Self {
        let count = function.states.len();
        if count == 0
            || count > MAX_ANALYSIS_STATES
            || eligible.len() != count
            || function.entry.0 >= count
        {
            return Self::default();
        }
        let edges = function
            .states
            .iter()
            .map(|state| normal_successors(&state.terminator))
            .collect::<Vec<_>>();
        if edges.iter().flatten().any(|&s| s >= count) {
            return Self::default();
        }
        let mut incoming = vec![Vec::new(); count];
        for (from, next) in edges.iter().enumerate() {
            for &to in next {
                incoming[to].push(from);
            }
        }
        let mut plan = Self {
            loops: Vec::new(),
            entries: vec![None; count],
        };
        let mut assigned = vec![false; count];
        let mut total_states = 0usize;
        for latch in 0..count {
            let TaskTerminator::Suspend { resume, .. } = function.states[latch].terminator else {
                continue;
            };
            let header = resume.0;
            if !eligible[latch] || !eligible[header] || latch == header {
                continue;
            }

            // Dominance witness: every normal path from the real entry to a
            // member must pass the loop header. This bounded graph walk also
            // rejects an irreducible/alternate entry without a global solver.
            let mut avoids_header = vec![false; count];
            let mut stack = vec![function.entry.0];
            while let Some(state) = stack.pop() {
                if state == header || avoids_header[state] {
                    continue;
                }
                avoids_header[state] = true;
                stack.extend(edges[state].iter().copied());
            }
            if avoids_header[latch] {
                continue;
            }
            let mut member = vec![false; count];
            member[header] = true;
            let mut stack = vec![latch];
            while let Some(state) = stack.pop() {
                if member[state] {
                    continue;
                }
                member[state] = true;
                stack.extend(incoming[state].iter().copied());
            }
            let states = (0..count).filter(|&s| member[s]).collect::<Vec<_>>();
            if states.len() < 2
                || states.len() > MAX_REGION_STATES
                || states
                    .iter()
                    .any(|&s| !eligible[s] || assigned[s] || avoids_header[s])
                || total_states.saturating_add(states.len()) > MAX_LOOP_TOTAL_STATES
                || states
                    .iter()
                    .any(|&s| s != header && incoming[s].iter().any(|&p| !member[p]))
                || states.iter().any(|&s| {
                    s != latch
                        && matches!(
                            function.states[s].terminator,
                            TaskTerminator::Suspend { .. }
                        )
                })
            {
                continue;
            }

            let mut degree = vec![0usize; count];
            for &s in &states {
                for &next in &edges[s] {
                    if member[next] && !(s == latch && next == header) {
                        degree[next] += 1;
                    }
                }
            }
            let mut queue = states
                .iter()
                .copied()
                .filter(|&s| degree[s] == 0)
                .collect::<VecDeque<_>>();
            let mut order = Vec::new();
            while let Some(s) = queue.pop_front() {
                order.push(s);
                for &next in &edges[s] {
                    if member[next] && !(s == latch && next == header) {
                        degree[next] -= 1;
                        if degree[next] == 0 {
                            queue.push_back(next);
                        }
                    }
                }
            }
            if order.len() != states.len() || order.first().copied() != Some(header) {
                continue;
            }
            let mut state_cost = vec![0usize; count];
            let mut operation_cost = vec![0usize; count];
            for &s in order.iter().rev() {
                let next = edges[s]
                    .iter()
                    .copied()
                    .filter(|&next| member[next] && !(s == latch && next == header));
                state_cost[s] = 1 + next.clone().map(|next| state_cost[next]).max().unwrap_or(0);
                // A terminator is work too: an empty IR state cannot create an
                // unbounded chunk merely because it contains zero TaskOps.
                operation_cost[s] = function.states[s].operations.len()
                    + 1
                    + next.map(|next| operation_cost[next]).max().unwrap_or(0);
            }
            let max_path_states = state_cost[header];
            let max_path_operations = operation_cost[header];
            let max_iterations = MAX_LOOP_ITERATIONS
                .min(super::super::task_range::COMPUTE_BANK_STATES / max_path_states)
                .min(MAX_LOOP_OPERATIONS / max_path_operations);
            if max_iterations < 2 {
                continue;
            }
            let private_max_iterations = (super::super::task_range::COMPUTE_PHASE_STATES
                / max_path_states)
                .min(super::super::task_range::COMPUTE_PHASE_OPERATIONS / max_path_operations);

            // Maximal linear blocks give a constant successful-path debit.
            // Branch joins carry only a small actual-prefix counter; checked
            // arithmetic cold exits settle the prefix within their block.
            let mut starts = vec![false; count];
            for &s in &states {
                let predecessors = incoming[s]
                    .iter()
                    .copied()
                    .filter(|&p| member[p])
                    .collect::<Vec<_>>();
                starts[s] = s == header
                    || predecessors.len() != 1
                    || predecessors
                        .iter()
                        .any(|&p| edges[p].len() != 1 || p == latch);
            }
            let mut blocks = Vec::new();
            let mut covered = vec![false; count];
            for &start in &order {
                if !starts[start] {
                    continue;
                }
                let mut block = Vec::new();
                let mut s = start;
                loop {
                    if covered[s] {
                        break;
                    }
                    covered[s] = true;
                    block.push(s);
                    if s == latch || edges[s].len() != 1 {
                        break;
                    }
                    let next = edges[s][0];
                    if !member[next] || starts[next] {
                        break;
                    }
                    s = next;
                }
                blocks.push(block);
            }
            if states.iter().any(|&s| !covered[s]) {
                continue;
            }
            for &s in &states {
                assigned[s] = true;
            }
            total_states += states.len();
            plan.entries[header] = Some(plan.loops.len());
            plan.loops.push(ScalarLoop {
                header,
                latch,
                states,
                blocks,
                max_path_states,
                max_path_operations,
                max_iterations,
                private_max_iterations,
            });
        }
        plan
    }
}

#[derive(Default)]
pub(super) struct ScalarRegions {
    pub regions: Vec<ScalarRegion>,
    entries: Vec<Option<usize>>,
}

pub(super) struct ScalarRegion {
    pub entry: usize,
    pub states: Vec<usize>,
    pub max_path_states: usize,
    pub operations: usize,
}

fn normal_successors(term: &TaskTerminator) -> Vec<usize> {
    let mut edges = match term {
        TaskTerminator::Jump { target } => vec![target.0],
        TaskTerminator::Branch {
            then_state,
            else_state,
            ..
        } => vec![then_state.0, else_state.0],
        TaskTerminator::Suspend { resume, .. } => vec![resume.0],
        TaskTerminator::ScopeDrain { ready, .. }
        | TaskTerminator::Await { ready, .. }
        | TaskTerminator::Timer { ready, .. }
        | TaskTerminator::BlockingWrite { ready, .. }
        | TaskTerminator::BlockingRead { ready, .. }
        | TaskTerminator::BlockingReadDir { ready, .. }
        | TaskTerminator::BlockingExists { ready, .. }
        | TaskTerminator::BlockingNet { ready, .. }
        | TaskTerminator::PgQuery { ready, .. } => vec![ready.0],
        TaskTerminator::TryResult { ok, err, .. } => vec![ok.0, err.0],
        TaskTerminator::Complete { .. }
        | TaskTerminator::Exit { .. }
        | TaskTerminator::Terminate => Vec::new(),
    };
    edges.sort_unstable();
    edges.dedup();
    edges
}

impl ScalarRegions {
    pub fn at_entry(&self, state: usize) -> Option<&ScalarRegion> {
        self.entries
            .get(state)
            .copied()
            .flatten()
            .map(|id| &self.regions[id])
    }

    pub fn build(function: &TaskFunction, eligible: Vec<bool>) -> Self {
        let count = function.states.len();
        if count == 0 || count > MAX_ANALYSIS_STATES || eligible.len() != count {
            return Self::default();
        }
        let edges = function
            .states
            .iter()
            .map(|state| normal_successors(&state.terminator))
            .collect::<Vec<_>>();
        if edges.iter().flatten().any(|&target| target >= count) {
            return Self::default();
        }
        let mut incoming = vec![0usize; count];
        let mut cut = vec![false; count];
        for (source, targets) in edges.iter().enumerate() {
            for &target in targets {
                incoming[target] += 1;
                if !eligible[source]
                    || matches!(
                        function.states[source].terminator,
                        TaskTerminator::Suspend { .. }
                    )
                {
                    cut[target] = true;
                }
            }
        }
        if function.entry.0 >= count {
            return Self::default();
        }
        cut[function.entry.0] = true;
        for state in 0..count {
            cut[state] |= incoming[state] != 1;
        }
        let mut plan = Self {
            regions: Vec::new(),
            entries: vec![None; count],
        };
        let mut assigned = vec![false; count];
        for entry in 0..count {
            if !cut[entry] || !eligible[entry] {
                continue;
            }
            let mut member = vec![false; count];
            let mut stack = vec![entry];
            let mut states = Vec::new();
            let mut operations = 0usize;
            while let Some(state) = stack.pop() {
                if member[state] || !eligible[state] || (state != entry && cut[state]) {
                    continue;
                }
                member[state] = true;
                states.push(state);
                operations = operations.saturating_add(function.states[state].operations.len());
                if states.len() > MAX_REGION_STATES || operations > MAX_REGION_OPERATIONS {
                    break;
                }
                for &next in &edges[state] {
                    if next != entry {
                        stack.push(next);
                    }
                }
            }
            if states.len() < 2
                || states.len() > MAX_REGION_STATES
                || operations > MAX_REGION_OPERATIONS
                || states.iter().any(|&s| assigned[s])
            {
                continue;
            }
            let mut degree = vec![0usize; count];
            for &state in &states {
                for &next in &edges[state] {
                    if next != entry && member[next] {
                        degree[next] += 1;
                    }
                }
            }
            let mut queue = states
                .iter()
                .copied()
                .filter(|&s| degree[s] == 0)
                .collect::<VecDeque<_>>();
            let mut order = Vec::new();
            while let Some(state) = queue.pop_front() {
                order.push(state);
                for &next in &edges[state] {
                    if next != entry && member[next] {
                        degree[next] -= 1;
                        if degree[next] == 0 {
                            queue.push_back(next);
                        }
                    }
                }
            }
            if order.len() != states.len() {
                continue;
            }
            let mut cost = vec![0usize; count];
            for &state in order.iter().rev() {
                cost[state] = 1 + edges[state]
                    .iter()
                    .copied()
                    .filter(|&s| s != entry && member[s])
                    .map(|s| cost[s])
                    .max()
                    .unwrap_or(0);
            }
            let max_path_states = cost[entry];
            if max_path_states < 2 || max_path_states > MAX_REGION_STATES {
                continue;
            }
            states.sort_unstable();
            for &state in &states {
                assigned[state] = true;
            }
            plan.entries[entry] = Some(plan.regions.len());
            plan.regions.push(ScalarRegion {
                entry,
                states,
                max_path_states,
                operations,
            });
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::task::{SlotId, StateId, TaskFunctionId, TaskOp, TaskState};
    use crate::ir::IrType;

    fn graph(terms: Vec<TaskTerminator>) -> TaskFunction {
        TaskFunction {
            id: TaskFunctionId(0),
            name: "region".into(),
            slots: Vec::new(),
            parameters: Vec::new(),
            entry: StateId(0),
            states: terms
                .into_iter()
                .map(|terminator| TaskState {
                    operations: Vec::new(),
                    terminator,
                })
                .collect(),
            result: IrType::Result(Box::new(IrType::Null)),
        }
    }
    fn jump(target: usize) -> TaskTerminator {
        TaskTerminator::Jump {
            target: StateId(target),
        }
    }

    #[test]
    fn regions_cut_latches_joins_and_nonpure_entries_and_bound_actual_paths() {
        let f = graph(vec![
            jump(1),
            TaskTerminator::Branch {
                condition: SlotId(0),
                then_state: StateId(2),
                else_state: StateId(4),
            },
            jump(3),
            TaskTerminator::Suspend {
                resume: StateId(1),
                cleanup: StateId(5),
            },
            TaskTerminator::Complete { value: SlotId(0) },
            TaskTerminator::Terminate,
        ]);
        let p = ScalarRegions::build(&f, vec![true, true, true, true, false, false]);
        let r = p.at_entry(1).unwrap();
        assert_eq!(r.states, vec![1, 2, 3]);
        assert_eq!(r.max_path_states, 3); // The false branch charges only its prefix.
        assert!(p.at_entry(0).is_none());
        let f = graph(vec![
            TaskTerminator::Branch {
                condition: SlotId(0),
                then_state: StateId(1),
                else_state: StateId(2),
            },
            jump(3),
            jump(3),
            jump(4),
            TaskTerminator::Complete { value: SlotId(0) },
        ]);
        let p = ScalarRegions::build(&f, vec![true, true, true, true, false]);
        assert_eq!(p.at_entry(0).unwrap().states, vec![0, 1, 2]);
        assert_eq!(p.at_entry(0).unwrap().max_path_states, 2);
        assert!(p.at_entry(3).is_none()); // A one-state chunk has no benefit.
        let p = ScalarRegions::build(&f, vec![false, true, true, true, false]);
        assert!(p.regions.is_empty()); // Each branch/join is now its own boundary.
    }

    #[test]
    fn regions_fail_closed_on_large_paths_ops_and_untrusted_shape() {
        let mut f = graph(
            (1..=66)
                .map(jump)
                .chain([TaskTerminator::Terminate])
                .collect(),
        );
        assert!(ScalarRegions::build(&f, vec![true; 67]).regions.is_empty());
        f = graph(vec![jump(1), jump(2), TaskTerminator::Terminate]);
        f.states[0].operations = vec![TaskOp::Read { slot: SlotId(0) }; MAX_REGION_OPERATIONS + 1];
        assert!(ScalarRegions::build(&f, vec![true; 3]).regions.is_empty());
        assert!(ScalarRegions::build(&f, vec![true]).regions.is_empty());
        f.states[0].terminator = jump(99);
        assert!(ScalarRegions::build(&f, vec![true; 3]).regions.is_empty());
    }

    #[test]
    fn loops_prove_one_entry_acyclic_iterations_and_actual_branch_costs() {
        let f = graph(vec![
            jump(1),
            TaskTerminator::Branch {
                condition: SlotId(0),
                then_state: StateId(2),
                else_state: StateId(6),
            },
            TaskTerminator::Branch {
                condition: SlotId(0),
                then_state: StateId(3),
                else_state: StateId(4),
            },
            jump(4),
            jump(5),
            TaskTerminator::Suspend {
                resume: StateId(1),
                cleanup: StateId(7),
            },
            TaskTerminator::Complete { value: SlotId(0) },
            TaskTerminator::Terminate,
        ]);
        let plan = ScalarLoops::build(&f, &[true, true, true, true, true, true, false, false]);
        let region = plan.at_header(1).unwrap();
        assert_eq!(region.states, vec![1, 2, 3, 4, 5]);
        assert_eq!(region.max_path_states, 5);
        assert_eq!(region.max_path_operations, 5);
        assert_eq!(region.max_iterations, 8);
        assert!(
            region.private_max_iterations * region.max_path_states
                <= super::super::super::task_range::COMPUTE_PHASE_STATES
        );
        assert!(
            region.private_max_iterations * region.max_path_operations
                <= super::super::super::task_range::COMPUTE_PHASE_OPERATIONS
        );
        assert_eq!(region.blocks, vec![vec![1], vec![2], vec![3], vec![4, 5]]);
        assert!(region.linear(&f).is_none()); // The body has another branch.
        let mut linear = f.clone();
        linear.states[2].terminator = jump(3);
        let p = ScalarLoops::build(&linear, &[true, true, true, true, true, true, false, false]);
        let shape = p.at_header(1).unwrap().linear(&linear).unwrap();
        assert_eq!(shape.body, vec![2, 3, 4, 5]);
        assert_eq!(shape.exit, 6);
        assert!(shape.body_when_true);
        // A side entry into the body defeats dominance, even when every node
        // is individually scalar. No cyclic path can avoid a chunk boundary.
        let mut side = f.clone();
        side.states[0].terminator = TaskTerminator::Branch {
            condition: SlotId(0),
            then_state: StateId(1),
            else_state: StateId(4),
        };
        assert!(ScalarLoops::build(&side, &[true; 8]).loops.is_empty());
        let mut inner_cycle = f.clone();
        inner_cycle.states[3].terminator = jump(2);
        assert!(ScalarLoops::build(&inner_cycle, &[true; 8])
            .loops
            .is_empty());
        let mut nonpure = vec![true; 8];
        nonpure[4] = false;
        assert!(ScalarLoops::build(&f, &nonpure).loops.is_empty());
    }

    #[test]
    fn loops_fall_back_on_nested_suspend_or_operation_capacity() {
        let mut f = graph(vec![
            jump(1),
            jump(2),
            TaskTerminator::Suspend {
                resume: StateId(1),
                cleanup: StateId(3),
            },
            TaskTerminator::Terminate,
        ]);
        f.states[1].operations = vec![TaskOp::Read { slot: SlotId(0) }; 128];
        assert!(ScalarLoops::build(&f, &[true; 4]).loops.is_empty());
        f.states[1].operations.clear();
        let p = ScalarLoops::build(&f, &[true; 4]);
        assert_eq!(p.at_header(1).unwrap().max_path_states, 2);
        f.states[1].terminator = TaskTerminator::Suspend {
            resume: StateId(2),
            cleanup: StateId(3),
        };
        assert!(ScalarLoops::build(&f, &[true; 4]).loops.is_empty());
        assert!(ScalarLoops::build(&f, &[true]).loops.is_empty());
    }
}
