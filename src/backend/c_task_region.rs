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
}
