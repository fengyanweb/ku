//! Flow-local evidence for callable cells retained by HTTP handlers.
//!
//! This does not make a value snapshot, add dependency edges, or freeze the
//! binding used to pass a handler by value. Callers supply its capture graph.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{BindingId, Checker, ClosureProvenance, Type};
use crate::{
    error::{DiagnosticId, KuError, KuResult},
    span::Span,
};

// Charge graph nodes, edges, retained evidence and propagation together. In
// particular, many frozen cells times many aliases cannot grow without bound.
const MAX_HTTP_SHARED_GRAPH_WORK: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HttpSharedCallable {
    pub(super) name: String,
    pub(super) registration: Span,
}

type SharedOrigins = BTreeMap<BindingId, HttpSharedCallable>;

struct GraphBudget {
    remaining: usize,
    span: Span,
}

impl GraphBudget {
    fn charge(&mut self) -> KuResult<()> {
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            proof_error(
                "http handler cannot prove HTTP-shared function bindings within the bounded capture analysis budget",
                self.span,
            )
        })?;
        Ok(())
    }
}

fn proof_error(message: &str, span: Span) -> KuError {
    KuError::runtime(message, span).with_diagnostic_id(DiagnosticId::HttpSharedCallableReassignment)
}

fn origin_order(origin: &HttpSharedCallable) -> (usize, usize, usize) {
    let start = origin.registration.start;
    (start.offset, start.line, start.column)
}

fn retain_earliest(origins: &mut SharedOrigins, id: BindingId, origin: &HttpSharedCallable) {
    match origins.entry(id) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(origin.clone());
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            if origin_order(origin) < origin_order(entry.get()) {
                entry.insert(origin.clone());
            }
        }
    }
}

impl Checker {
    pub(super) fn reject_http_shared_unknown_call(
        &self,
        provenance: &ClosureProvenance,
        span: Span,
    ) -> KuResult<()> {
        let mut budget = GraphBudget {
            remaining: MAX_HTTP_SHARED_GRAPH_WORK,
            span,
        };
        let mut pending = BTreeSet::new();
        for id in &provenance.dependencies {
            budget.charge()?;
            pending.insert(*id);
        }
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop_first() {
            budget.charge()?;
            if !visited.insert(id) {
                continue;
            }
            self.reject_http_shared_write(id, span)?;
            if let Some(binding) = self.binding_by_id(id) {
                for dependency in &binding.closure_provenance.dependencies {
                    budget.charge()?;
                    pending.insert(*dependency);
                }
            } else if !provenance.http_shared.is_empty() {
                return Err(proof_error(
                    "cannot prove HTTP-shared function binding safety for an opaque call with an unavailable captured binding", span,
                ));
            }
        }
        if !provenance.complete && !provenance.http_shared.is_empty() {
            return Err(proof_error(
                "cannot prove HTTP-shared function binding safety for an opaque call with incomplete capture provenance", span,
            ));
        }
        Ok(())
    }

    pub(super) fn freeze_http_shared_callables(
        &mut self,
        provenance: &ClosureProvenance,
        span: Span,
    ) -> KuResult<()> {
        // A proven empty environment has no callable cell to freeze. Do not
        // scan unrelated bindings/edges for a capture-free or by-value handler.
        // Empty dependencies alone are insufficient: escaped shared evidence
        // and an uncertain body must still take the conservative path.
        if provenance.complete
            && !provenance.http_callable_body_uncertain
            && provenance.dependencies.is_empty()
            && provenance.http_shared.is_empty()
        {
            return Ok(());
        }
        // Planning is read-only. A missing capture or exhausted budget must not
        // leave a partially frozen state in a speculative branch/loop pass.
        let plan = self.http_shared_freeze_plan(provenance, span)?;
        for binding in self.scopes.iter_mut().flat_map(|scope| scope.values_mut()) {
            if let Some(origins) = plan.get(&binding.binding_id) {
                for (id, origin) in origins {
                    retain_earliest(&mut binding.closure_provenance.http_shared, *id, origin);
                }
            }
        }
        Ok(())
    }

    fn http_shared_freeze_plan(
        &self,
        provenance: &ClosureProvenance,
        span: Span,
    ) -> KuResult<BTreeMap<BindingId, SharedOrigins>> {
        let mut budget = GraphBudget {
            remaining: MAX_HTTP_SHARED_GRAPH_WORK,
            span,
        };
        if !provenance.complete {
            return Err(proof_error(
                "http handler cannot prove HTTP-shared function bindings because its capture provenance is incomplete",
                span,
            ));
        }

        // A replay overlay can contain the same BindingId as its original.
        // Keep the original graph node, but retain evidence from every copy and
        // later update every live copy by identity, never by its spelling.
        let mut bindings = BTreeMap::new();
        let mut binding_copies: BTreeMap<BindingId, usize> = BTreeMap::new();
        let mut reverse: BTreeMap<BindingId, Vec<BindingId>> = BTreeMap::new();
        let mut existing = BTreeMap::new();
        for scope in &self.scopes {
            for (name, binding) in scope {
                budget.charge()?;
                bindings
                    .entry(binding.binding_id)
                    .or_insert((name, binding));
                *binding_copies.entry(binding.binding_id).or_default() += 1;
                for dependency in &binding.closure_provenance.dependencies {
                    budget.charge()?;
                    reverse
                        .entry(*dependency)
                        .or_default()
                        .push(binding.binding_id);
                }
                for (id, origin) in &binding.closure_provenance.http_shared {
                    budget.charge()?;
                    retain_earliest(&mut existing, *id, origin);
                }
            }
        }
        let mut origins = BTreeMap::new();
        for (id, origin) in &provenance.http_shared {
            budget.charge()?;
            retain_earliest(&mut existing, *id, origin);
            retain_earliest(&mut origins, *id, &existing[id]);
        }

        let mut pending = BTreeSet::new();
        for dependency in &provenance.dependencies {
            budget.charge()?;
            pending.insert(*dependency);
        }
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop_first() {
            budget.charge()?;
            if !visited.insert(id) {
                continue;
            }
            let Some((name, binding)) = bindings.get(&id) else {
                return Err(proof_error(
                    "http handler cannot prove HTTP-shared function bindings because a captured lexical binding is unavailable",
                    span,
                ));
            };
            if !binding.closure_provenance.complete {
                return Err(proof_error(
                    "http handler cannot prove HTTP-shared function bindings because a captured binding has incomplete provenance",
                    span,
                ));
            }
            if matches!(&binding.ty, Type::FunctionValue { .. }) {
                if binding.closure_provenance.http_callable_body_uncertain {
                    return Err(proof_error(
                        "http handler cannot prove HTTP-shared function binding safety after an indirect replacement with an unproven body",
                        span,
                    ));
                }
                let origin = existing
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| HttpSharedCallable {
                        name: (*name).clone(),
                        registration: span,
                    });
                retain_earliest(&mut origins, id, &origin);
            }
            for dependency in &binding.closure_provenance.dependencies {
                budget.charge()?;
                if !visited.contains(dependency) {
                    pending.insert(*dependency);
                }
            }
        }

        // Incomplete or escaped intermediate graphs are not evidence of
        // disjointness. Such function-capable values conservatively carry the
        // frozen-cell evidence, not a freeze of their own binding. Call-effect
        // checking still has to reject an opaque potentially relevant write.
        let mut uncertain = Vec::new();
        if !origins.is_empty() {
            for (id, (_, binding)) in &bindings {
                budget.charge()?;
                let mut incomplete = !binding.closure_provenance.complete;
                for dependency in &binding.closure_provenance.dependencies {
                    budget.charge()?;
                    incomplete |= !bindings.contains_key(dependency);
                }
                if incomplete && self.type_may_contain_function_value(&binding.ty) {
                    uncertain.push(*id);
                }
            }
        }

        let mut plan: BTreeMap<BindingId, SharedOrigins> = BTreeMap::new();
        for (target, origin) in origins {
            let mut queue = VecDeque::new();
            let mut reached = BTreeSet::new();
            for id in std::iter::once(target).chain(uncertain.iter().copied()) {
                budget.charge()?;
                if reached.insert(id) {
                    queue.push_back(id);
                }
            }
            while let Some(id) = queue.pop_front() {
                budget.charge()?;
                if bindings.contains_key(&id) {
                    // Account for committing the same identity to its original
                    // and any lexical replay copies after this plan succeeds.
                    for _ in 0..binding_copies[&id] {
                        budget.charge()?;
                    }
                    retain_earliest(plan.entry(id).or_default(), target, &origin);
                }
                if let Some(dependants) = reverse.get(&id) {
                    for dependant in dependants {
                        budget.charge()?;
                        if reached.insert(*dependant) {
                            queue.push_back(*dependant);
                        }
                    }
                }
            }
        }
        Ok(plan)
    }

    pub(super) fn reject_http_shared_write(&self, target: BindingId, span: Span) -> KuResult<()> {
        let origin = self
            .scopes
            .iter()
            .flat_map(|scope| scope.values())
            .filter_map(|binding| binding.closure_provenance.http_shared.get(&target))
            .min_by_key(|origin| origin_order(origin));
        if let Some(origin) = origin {
            return Err(KuError::runtime(
                format!(
                    "cannot reassign HTTP-shared function binding '{}'; shared by HTTP registration at line {}, column {}",
                    origin.name, origin.registration.start.line, origin.registration.start.column,
                ),
                span,
            )
            .with_diagnostic_id(DiagnosticId::HttpSharedCallableReassignment));
        }
        Ok(())
    }
}
