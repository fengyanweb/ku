use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};

use crate::error::{KuError, KuResult};
use crate::runtime::task::TaskHandle;
use crate::span::Span;
use crate::value::{BorrowedValue, Value, ValueProjection};

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
    owner_task_id: i64,
    borrowed_parameter: bool,
    match_probe: bool,
}

impl Binding {
    fn check_assignable(&self, name: &str, span: Span) -> KuResult<()> {
        if matches!(self.value, Value::Borrowed(_)) {
            return Err(KuError::runtime(
                format!(
                    "cannot modify through borrowed parameter or active borrowed source '{name}'"
                ),
                span,
            ));
        }
        if !self.mutable {
            return Err(KuError::runtime(
                format!("cannot assign to immutable variable '{}'", name),
                span,
            ));
        }
        if self.owner_task_id != crate::runtime::task::current_task_id() {
            return Err(KuError::runtime(
                format!("async task cannot modify captured variable '{}'", name),
                span,
            ));
        }
        Ok(())
    }
}

pub(crate) type BindingCell = Arc<Mutex<Binding>>;

fn collect_binding_owned_tasks(
    binding: &std::sync::MutexGuard<'_, Binding>,
    tasks: &mut Vec<TaskHandle>,
    span: Span,
) -> KuResult<()> {
    if binding.borrowed_parameter
        || binding.match_probe
        || binding.owner_task_id != crate::runtime::task::current_task_id()
    {
        return Ok(());
    }
    match &binding.value {
        // Borrowing an owned caller slot changes its current representation,
        // not its scope owner. Keep the source lock through this read so lease
        // restoration cannot race it; never re-lock the same BindingCell.
        Value::Borrowed(view) => view
            .with_source_read_locked(binding, span, |root| root.collect_owned_tasks(tasks, span)),
        value => value.collect_owned_tasks(tasks, span),
    }
}

/// A call-scoped, non-owning view of caller binding cells. Values are inspected
/// when cancellation is observed, not snapshotted before argument side effects.
#[derive(Debug)]
pub(crate) struct OwnedBindingObserver {
    cells: Vec<Weak<Mutex<Binding>>>,
}

impl OwnedBindingObserver {
    pub(crate) fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub(crate) fn collect_owned_tasks(
        &self,
        tasks: &mut Vec<TaskHandle>,
        span: Span,
    ) -> KuResult<()> {
        for weak in &self.cells {
            let Some(cell) = weak.upgrade() else { continue };
            let binding = cell
                .lock()
                .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
            collect_binding_owned_tasks(&binding, tasks, span)?;
        }
        // No binding lock survives this method. Cancellation/waiting belongs to
        // the caller, after every active observer has contributed its handles.
        Ok(())
    }
}

/// Stable call-owned storage. No binding mutex is held while running Ku code.
pub(crate) struct BorrowLease {
    source: Option<BindingCell>,
    root: Option<Arc<Value>>,
}

impl BorrowLease {
    pub(crate) fn temporary(value: Value) -> (Value, Option<Self>) {
        if value.copy_value().is_some() || matches!(value, Value::Borrowed(_)) {
            return (value, None);
        }
        let root = Arc::new(value);
        let view = Value::Borrowed(BorrowedValue::new(&root));
        (
            view,
            Some(Self {
                source: None,
                root: Some(root),
            }),
        )
    }
}

impl Drop for BorrowLease {
    fn drop(&mut self) {
        let Some(root) = self.root.take() else { return };
        if let Some(source) = &self.source {
            let mut binding = source
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Foreign ordinary reads upgrade only while holding this same
            // mutex. Borrowed reads are restricted to the owning thread/task,
            // whose synchronous callbacks have returned before this guard.
            let value =
                Arc::try_unwrap(root).expect("borrowed reads ended before source restoration");
            binding.value = value;
        }
    }
}

#[derive(Debug)]
pub struct Env {
    scopes: Vec<Scope>,
}

#[derive(Debug, Default)]
struct Scope {
    bindings: HashMap<String, BindingCell>,
    // Sharing an environment shares cells, not responsibility for their scope
    // exit. Only bindings defined through this Env appear in this scope's set.
    owned_bindings: HashSet<String>,
}

impl Clone for Env {
    fn clone(&self) -> Self {
        Self {
            scopes: self
                .scopes
                .iter()
                .map(|scope| Scope {
                    bindings: scope.bindings.clone(),
                    owned_bindings: HashSet::new(),
                })
                .collect(),
        }
    }
}

impl Env {
    pub fn new() -> Self {
        Self {
            scopes: vec![Scope::default()],
        }
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    pub fn define(
        &mut self,
        name: String,
        value: Value,
        mutable: bool,
        span: Span,
    ) -> KuResult<()> {
        value.require_owned(span)?;
        self.define_owned(name, value, mutable, span)
    }

    pub(crate) fn define_owned(
        &mut self,
        name: String,
        value: Value,
        mutable: bool,
        span: Span,
    ) -> KuResult<()> {
        value.require_owned_root(span)?;
        self.define_parameter(name, value, mutable, span)
    }

    pub(crate) fn define_parameter(
        &mut self,
        name: String,
        value: Value,
        mutable: bool,
        span: Span,
    ) -> KuResult<()> {
        let scope = self
            .scopes
            .last_mut()
            .expect("environment always has a scope");
        if scope.bindings.contains_key(&name) {
            return Err(KuError::runtime(
                format!("variable '{}' is already defined in this scope", name),
                span,
            ));
        }
        scope.bindings.insert(
            name.clone(),
            Arc::new(Mutex::new(Binding {
                borrowed_parameter: matches!(value, Value::Borrowed(_)),
                value,
                mutable,
                owner_task_id: crate::runtime::task::current_task_id(),
                match_probe: false,
            })),
        );
        scope.owned_bindings.insert(name);
        Ok(())
    }

    /// A tentative match binding is a non-owning snapshot until its guard wins.
    /// It may be read, but must not move/await Task payloads or cancel them when
    /// a failed guard discards this scope.
    pub(crate) fn define_task_match_probe(
        &mut self,
        name: String,
        value: Value,
        span: Span,
    ) -> KuResult<()> {
        self.define_owned(name.clone(), value, false, span)?;
        let cell = self
            .scopes
            .last()
            .and_then(|scope| scope.bindings.get(&name))
            .expect("new probe binding");
        cell.lock()
            .map_err(|_| KuError::runtime("environment binding is poisoned", span))?
            .match_probe = true;
        self.scopes
            .last_mut()
            .expect("probe scope")
            .owned_bindings
            .remove(&name);
        Ok(())
    }

    pub(crate) fn commit_task_match_probe(
        &mut self,
        name: &str,
        value: Value,
        span: Span,
    ) -> KuResult<()> {
        value.require_owned_root(span)?;
        let cell = self
            .scopes
            .last()
            .and_then(|scope| scope.bindings.get(name))
            .ok_or_else(|| KuError::runtime("missing tentative match binding", span))?;
        let mut binding = cell
            .lock()
            .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
        if !binding.match_probe || binding.owner_task_id != crate::runtime::task::current_task_id()
        {
            return Err(KuError::runtime(
                "invalid tentative match ownership transfer",
                span,
            ));
        }
        let previous = std::mem::replace(&mut binding.value, value);
        binding.match_probe = false;
        drop(binding);
        drop(previous);
        self.scopes
            .last_mut()
            .expect("probe scope")
            .owned_bindings
            .insert(name.to_string());
        Ok(())
    }

    pub(crate) fn borrow(&self, name: &str, span: Span) -> KuResult<(Value, Option<BorrowLease>)> {
        let source = self
            .find_cell(name)
            .ok_or_else(|| KuError::runtime(format!("undefined variable '{name}'"), span))?;
        let mut binding = source
            .lock()
            .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
        if let Value::Borrowed(view) = &binding.value {
            view.check_owner(span)?;
            return Ok((binding.value.clone(), None));
        }
        if let Some(value) = binding.value.copy_value() {
            return Ok((value, None));
        }
        let root = Arc::new(std::mem::replace(&mut binding.value, Value::Null));
        let view = Value::Borrowed(BorrowedValue::new(&root));
        binding.value = view.clone();
        drop(binding);
        Ok((
            view,
            Some(BorrowLease {
                source: Some(source),
                root: Some(root),
            }),
        ))
    }

    pub(crate) fn check_capture(&self, names: &HashSet<String>, span: Span) -> KuResult<()> {
        for name in names {
            if self.contains(name) {
                self.with_value(name, span, |value| {
                    if matches!(value, Value::Borrowed(_)) {
                        Err(KuError::runtime(
                            format!("cannot capture borrowed parameter '{name}'"),
                            span,
                        ))
                    } else {
                        Ok(())
                    }
                })?;
            }
        }
        Ok(())
    }

    pub(crate) fn capture(&self, names: &HashSet<String>) -> Env {
        let mut captured = HashMap::new();
        for name in names {
            if let Some(cell) = self.find_cell(name) {
                captured.insert(name.clone(), cell);
            }
        }
        Env {
            scopes: vec![Scope {
                bindings: captured,
                owned_bindings: HashSet::new(),
            }],
        }
    }

    fn find_cell(&self, name: &str) -> Option<BindingCell> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.bindings.get(name) {
                return Some(binding.clone());
            }
        }
        None
    }

    pub fn assign(&mut self, name: &str, value: Value, span: Span) -> KuResult<()> {
        value.require_owned(span)?;
        self.assign_owned(name, value, span)
    }

    pub(crate) fn assign_owned(&mut self, name: &str, value: Value, span: Span) -> KuResult<()> {
        value.require_owned_root(span)?;
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.get(name) {
                let mut binding = binding
                    .lock()
                    .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
                binding.check_assignable(name, span)?;
                binding.value = value;
                return Ok(());
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{}'", name),
            span,
        ))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.bindings.contains_key(name))
    }

    pub fn get(&self, name: &str, span: Span) -> KuResult<Value> {
        self.with_value(name, span, |value| Ok(value.clone()))
    }

    /// Ku owning reads transfer Task-containing values instead of sharing an
    /// internal handle lease with a moved-from slot. Other values retain get's
    /// existing transparent snapshot/borrow behavior.
    pub(crate) fn get_owning(&self, name: &str, span: Span) -> KuResult<Value> {
        for scope in self.scopes.iter().rev() {
            let Some(cell) = scope.bindings.get(name) else {
                continue;
            };
            let mut binding = cell
                .lock()
                .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
            if binding.value.contains_owned_task(span)? {
                Self::check_task_move(&binding, scope.owned_bindings.contains(name), name, span)?;
                return Ok(std::mem::replace(&mut binding.value, Value::Null));
            }
            if let Value::Borrowed(view) = &binding.value {
                if !view.is_current_owner() {
                    if binding.borrowed_parameter {
                        view.check_owner(span)?;
                    }
                    return view.with_source_read_locked(&binding, span, |value| {
                        if value.contains_owned_task(span)? {
                            return Err(KuError::runtime(
                                format!(
                                    "cannot move Task from captured or borrowed variable '{name}'"
                                ),
                                span,
                            ));
                        }
                        Ok(value.clone())
                    });
                }
            }
            return Ok(binding.value.clone());
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
    }

    fn check_task_move(binding: &Binding, owned: bool, name: &str, span: Span) -> KuResult<()> {
        if binding.match_probe {
            return Err(KuError::runtime(
                format!("cannot consume tentative Task binding '{name}' in a match guard; await or move it only in the selected arm"),
                span,
            ).with_diagnostic_id(crate::error::DiagnosticId::TaskGuardMove));
        }
        if !owned
            || binding.borrowed_parameter
            || binding.owner_task_id != crate::runtime::task::current_task_id()
        {
            return Err(KuError::runtime(
                format!("cannot move Task from captured or borrowed variable '{name}'"),
                span,
            ));
        }
        Ok(())
    }

    /// Only take the selected Task-containing subtree. No user code, cancellation
    /// or waiting may run while this binding lock is held. None preserves normal
    /// lookup diagnostics and leaves all ordinary values and sibling slots intact.
    pub(crate) fn take_task_projection(
        &self,
        name: &str,
        path: &[ValueProjection],
        span: Span,
    ) -> KuResult<Option<Value>> {
        for scope in self.scopes.iter().rev() {
            let Some(cell) = scope.bindings.get(name) else {
                continue;
            };
            let mut binding = cell
                .lock()
                .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
            if !scope.owned_bindings.contains(name)
                || binding.borrowed_parameter
                || binding.owner_task_id != crate::runtime::task::current_task_id()
            {
                if binding.value.contains_task_projection(path, span)? {
                    Self::check_task_move(
                        &binding,
                        scope.owned_bindings.contains(name),
                        name,
                        span,
                    )?;
                }
                return Ok(None);
            }
            return binding.value.take_task_projection(path, span);
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
    }

    pub(crate) fn current_scope_owned_tasks(&self, span: Span) -> KuResult<Vec<TaskHandle>> {
        let index = self.scopes.len() - 1;
        self.collect_scope_owned_tasks(index..self.scopes.len(), span)
    }

    pub(crate) fn all_owned_tasks(&self, span: Span) -> KuResult<Vec<TaskHandle>> {
        self.collect_scope_owned_tasks(0..self.scopes.len(), span)
    }

    pub(crate) fn observe_owned_bindings(&self, span: Span) -> KuResult<OwnedBindingObserver> {
        let mut cells = Vec::new();
        for scope in &self.scopes {
            cells.try_reserve(scope.owned_bindings.len()).map_err(|_| {
                KuError::runtime("caller task ownership observation out of memory", span)
            })?;
            for name in &scope.owned_bindings {
                if let Some(cell) = scope.bindings.get(name) {
                    cells.push(Arc::downgrade(cell));
                }
            }
        }
        Ok(OwnedBindingObserver { cells })
    }

    fn collect_scope_owned_tasks(
        &self,
        indices: std::ops::Range<usize>,
        span: Span,
    ) -> KuResult<Vec<TaskHandle>> {
        let mut tasks = Vec::new();
        for index in indices {
            let scope = &self.scopes[index];
            for name in &scope.owned_bindings {
                let Some(cell) = scope.bindings.get(name) else {
                    continue;
                };
                let binding = cell
                    .lock()
                    .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
                collect_binding_owned_tasks(&binding, &mut tasks, span)?;
            }
        }
        // Each binding guard has left scope. Callers may now request cancellation
        // for the whole batch before waiting with the shared cleanup deadline.
        Ok(tasks)
    }

    /// Project a value without cloning its entire owned container. The callback
    /// must only inspect this value: never evaluate Ku code or lock another
    /// binding while this binding's mutex is held.
    pub(crate) fn with_value<T>(
        &self,
        name: &str,
        span: Span,
        project: impl FnOnce(&Value) -> KuResult<T>,
    ) -> KuResult<T> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.bindings.get(name) {
                let binding = binding
                    .lock()
                    .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
                if let Value::Borrowed(view) = &binding.value {
                    if !view.is_current_owner() {
                        if binding.borrowed_parameter {
                            view.check_owner(span)?;
                        }
                        // Preserve pre-borrow ordinary captured reads: get()
                        // clones the real owned value as before. Do not export
                        // the temporary loan into an unrelated task.
                        return view.with_source_read_locked(&binding, span, project);
                    }
                }
                return project(&binding.value);
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{}'", name),
            span,
        ))
    }

    /// An unshared binding cannot be changed through a closure or another task
    /// while an effect-free subexpression is being evaluated outside the lock.
    pub(crate) fn is_unshared(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .is_some_and(|cell| Arc::strong_count(cell) == 1)
    }

    /// The caller has already evaluated the RHS of `+=`. Reserve before
    /// mutating so allocation failure never destroys the previous value.
    pub(crate) fn append_string(&mut self, name: &str, text: &str, span: Span) -> KuResult<bool> {
        let Some(cell) = self.find_cell(name) else {
            return Ok(false);
        };
        let mut binding = cell
            .lock()
            .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
        if !matches!(&binding.value, Value::String(_)) {
            return Ok(false);
        }
        binding.check_assignable(name, span)?;
        let Value::String(value) = &mut binding.value else {
            unreachable!("string type was checked while holding the binding lock")
        };
        value
            .try_reserve(text.len())
            .map_err(|_| KuError::runtime("string append out of memory", span))?;
        value.push_str(text);
        Ok(true)
    }

    /// Only used for a proven exact `xs = xs.push(piece)` after an effect-free
    /// `piece` has been evaluated. The binding lock and owner-task check make
    /// this equally safe for a locally captured cell; general `push` remains a
    /// pure operation producing a copy.
    pub(crate) fn append_array(&mut self, name: &str, value: Value, span: Span) -> KuResult<()> {
        value.require_owned_root(span)?;
        let cell = self
            .find_cell(name)
            .ok_or_else(|| KuError::runtime(format!("undefined variable '{}'", name), span))?;
        let mut binding = cell
            .lock()
            .map_err(|_| KuError::runtime("environment binding is poisoned", span))?;
        binding.check_assignable(name, span)?;
        let Value::Array(values) = &mut binding.value else {
            return Err(KuError::runtime("type error: expected array", span));
        };
        values
            .try_reserve(1)
            .map_err(|_| KuError::runtime("array append out of memory", span))?;
        values.push(value);
        Ok(())
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod task_ownership_tests {
    use super::*;
    use crate::runtime::task::TaskRuntime;

    fn task(runtime: &TaskRuntime) -> Value {
        Value::Task(runtime.spawn(|| Ok(Value::Int(7))).unwrap())
    }

    fn ids(tasks: Vec<TaskHandle>) -> Vec<i64> {
        let mut ids: Vec<_> = tasks.iter().map(TaskHandle::id).collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn caller_owner_observer_reads_rebound_slots_and_does_not_retain_moved_tasks() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let mut env = Env::new();
        env.define("slot".into(), Value::Null, true, span).unwrap();
        let observer = env.observe_owned_bindings(span).unwrap();
        let mut captured = env.capture(&HashSet::from(["slot".into()]));
        assert!(captured.observe_owned_bindings(span).unwrap().is_empty());
        captured.assign_owned("slot", task(&runtime), span).unwrap();
        let mut tasks = Vec::new();
        observer.collect_owned_tasks(&mut tasks, span).unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "observer must see a Task inserted after registration"
        );
        let expected = tasks[0].id();
        tasks.clear();
        let moved = env.get_owning("slot", span).unwrap();
        assert!(matches!(&moved, Value::Task(task) if task.id() == expected));
        observer.collect_owned_tasks(&mut tasks, span).unwrap();
        assert!(
            tasks.is_empty(),
            "moved-from cells must not cancel the transferred Task"
        );
        captured.assign_owned("slot", task(&runtime), span).unwrap();
        observer.collect_owned_tasks(&mut tasks, span).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_ne!(tasks[0].id(), expected);
        tasks.clear();
        drop(captured);
        drop(env);
        observer.collect_owned_tasks(&mut tasks, span).unwrap();
        assert!(
            tasks.is_empty(),
            "Weak observers must not keep exited bindings alive"
        );
    }

    #[test]
    fn task_owning_read_clears_moved_from_wrapper_and_preserves_plain_reads() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let mut env = Env::new();
        env.define(
            "source".into(),
            Value::Array(vec![task(&runtime)]),
            false,
            span,
        )
        .unwrap();
        env.define(
            "text".into(),
            Value::String("unchanged".into()),
            false,
            span,
        )
        .unwrap();
        let expected = ids(env.current_scope_owned_tasks(span).unwrap());
        let moved = env.get_owning("source", span).unwrap();
        assert_eq!(env.get("source", span).unwrap(), Value::Null);
        assert!(env.current_scope_owned_tasks(span).unwrap().is_empty());
        env.push_scope();
        env.define("destination".into(), moved, false, span)
            .unwrap();
        assert_eq!(ids(env.current_scope_owned_tasks(span).unwrap()), expected);
        assert_eq!(
            env.get_owning("text", span).unwrap(),
            Value::String("unchanged".into())
        );
        assert_eq!(
            env.get("text", span).unwrap(),
            Value::String("unchanged".into())
        );
    }

    #[test]
    fn task_projection_takes_only_selected_nested_slot() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let first = task(&runtime);
        let second = task(&runtime);
        let Value::Task(first_handle) = &first else {
            unreachable!()
        };
        let first_id = first_handle.id();
        let Value::Task(second_handle) = &second else {
            unreachable!()
        };
        let second_id = second_handle.id();
        let mut env = Env::new();
        env.define(
            "root".into(),
            Value::Object(HashMap::from([
                ("tasks".into(), Value::Array(vec![first, second])),
                ("text".into(), Value::String("kept".into())),
            ])),
            true,
            span,
        )
        .unwrap();
        let selected = env
            .take_task_projection(
                "root",
                &[
                    ValueProjection::Field("tasks".into()),
                    ValueProjection::Index(0),
                ],
                span,
            )
            .unwrap()
            .unwrap();
        assert!(matches!(&selected, Value::Task(handle) if handle.id() == first_id));
        assert_eq!(
            ids(env.current_scope_owned_tasks(span).unwrap()),
            vec![second_id]
        );
        assert!(env
            .take_task_projection("root", &[ValueProjection::Field("text".into())], span)
            .unwrap()
            .is_none());
        assert!(env
            .take_task_projection("root", &[ValueProjection::Field("missing".into())], span)
            .unwrap()
            .is_none());
        assert!(env
            .take_task_projection(
                "root",
                &[
                    ValueProjection::Field("tasks".into()),
                    ValueProjection::Index(99),
                ],
                span
            )
            .unwrap()
            .is_none());
        env.with_value("root", span, |value| {
            let Value::Object(fields) = value else {
                unreachable!()
            };
            let Value::Array(values) = &fields["tasks"] else {
                unreachable!()
            };
            assert_eq!(values[0], Value::Null);
            assert_eq!(fields["text"], Value::String("kept".into()));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn task_scope_collection_excludes_captures_foreign_owners_and_releases_locks() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let mut env = Env::new();
        env.define("outer".into(), task(&runtime), false, span)
            .unwrap();
        let outer = ids(env.current_scope_owned_tasks(span).unwrap());
        let captured = env.capture(&HashSet::from(["outer".into()]));
        assert!(captured.all_owned_tasks(span).unwrap().is_empty());
        assert!(captured.get_owning("outer", span).is_err());
        let mut clone = env.clone();
        assert!(clone.all_owned_tasks(span).unwrap().is_empty());
        clone
            .define("new".into(), task(&runtime), false, span)
            .unwrap();
        assert_eq!(clone.current_scope_owned_tasks(span).unwrap().len(), 1);
        env.push_scope();
        env.define("inner".into(), task(&runtime), false, span)
            .unwrap();
        assert_eq!(env.current_scope_owned_tasks(span).unwrap().len(), 1);
        let tasks = env.all_owned_tasks(span).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(env.find_cell("outer").unwrap().try_lock().is_ok());
        assert!(env.find_cell("inner").unwrap().try_lock().is_ok());
        let inner = env.find_cell("inner").unwrap();
        inner.lock().unwrap().owner_task_id =
            crate::runtime::task::current_task_id().wrapping_add(1);
        assert!(env.current_scope_owned_tasks(span).unwrap().is_empty());
        assert_eq!(ids(env.all_owned_tasks(span).unwrap()), outer);
        assert!(env.get_owning("inner", span).is_err());
        assert_eq!(ids(env.all_owned_tasks(span).unwrap()), outer);
    }

    #[test]
    fn task_collection_observes_owned_borrow_source_but_not_borrowed_parameters() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let mut env = Env::new();
        env.define("source".into(), task(&runtime), false, span)
            .unwrap();
        let expected = ids(env.current_scope_owned_tasks(span).unwrap());
        let observer = env.observe_owned_bindings(span).unwrap();
        let (view, lease) = env.borrow("source", span).unwrap();
        let mut parameters = Env::new();
        parameters
            .define_parameter("view".into(), view, false, span)
            .unwrap();
        assert_eq!(ids(env.all_owned_tasks(span).unwrap()), expected);
        let mut observed = Vec::new();
        observer.collect_owned_tasks(&mut observed, span).unwrap();
        assert_eq!(ids(observed), expected);
        assert!(env.find_cell("source").unwrap().try_lock().is_ok());
        assert!(parameters.all_owned_tasks(span).unwrap().is_empty());
        let mut observed_parameters = Vec::new();
        parameters
            .observe_owned_bindings(span)
            .unwrap()
            .collect_owned_tasks(&mut observed_parameters, span)
            .unwrap();
        assert!(observed_parameters.is_empty());
        assert!(parameters
            .take_task_projection("view", &[], span)
            .unwrap()
            .is_none());
        drop(lease);
        assert_eq!(ids(env.all_owned_tasks(span).unwrap()), expected);
    }
}

#[cfg(test)]
mod collection_tests {
    use super::*;

    #[test]
    fn environment_scope_metadata_does_not_inflate_value_layout() {
        assert_eq!(
            std::mem::size_of::<Env>(),
            std::mem::size_of::<Vec<Scope>>(),
            "Env must retain a single vector-sized representation"
        );
        assert!(
            std::mem::size_of::<Value>() <= 128,
            "environment metadata inflated every Value to {} bytes",
            std::mem::size_of::<Value>()
        );
    }

    #[test]
    fn cloned_scope_metadata_shares_cells_without_copying_cleanup_ownership() {
        let span = Span::default();
        let mut env = Env::new();
        env.define("outer".into(), Value::Int(1), true, span)
            .unwrap();
        env.push_scope();
        env.define("inner".into(), Value::Int(2), true, span)
            .unwrap();
        let clone = env.clone();
        assert_eq!(clone.scopes.len(), env.scopes.len());
        for (source, shared) in env.scopes.iter().zip(&clone.scopes) {
            assert_eq!(source.owned_bindings.len(), 1);
            assert!(shared.owned_bindings.is_empty());
            for (name, cell) in &source.bindings {
                assert!(Arc::ptr_eq(cell, &shared.bindings[name]));
            }
        }
        assert!(clone.observe_owned_bindings(span).unwrap().is_empty());
        env.pop_scope();
        assert!(!env.contains("inner"));
        assert_eq!(clone.get("inner", span).unwrap(), Value::Int(2));
        assert!(env.scopes[0].owned_bindings.contains("outer"));
    }

    #[test]
    fn borrowed_views_reject_foreign_threads_before_upgrading_the_source() {
        let span = Span::default();
        let mut env = Env::new();
        env.define(
            "value".into(),
            Value::String("kept".repeat(1024)),
            true,
            span,
        )
        .unwrap();
        let (view, lease) = env.borrow("value", span).unwrap();
        let mut parameter_env = Env::new();
        parameter_env
            .define_parameter("parameter".into(), view.clone(), false, span)
            .unwrap();
        let source_env = env.clone();
        std::thread::spawn(move || {
            let error = view.with_read::<()>(span, |_| panic!("foreign view must not upgrade its root")).unwrap_err();
            assert!(error.message.contains("across threads or async tasks"));
            assert!(parameter_env.get("parameter", span).unwrap_err().message.contains("across threads or async tasks"));
            assert!(source_env.borrow("value", span).is_err(), "overlapping cross-task loan must be rejected explicitly");
            assert!(matches!(source_env.get("value", span).unwrap(), Value::String(value) if value.len() == 4096), "ordinary captured reads must retain their owned-snapshot behavior");
        }).join().unwrap();
        drop(lease);
        assert!(
            matches!(env.get("value", span).unwrap(), Value::String(value) if value.len() == 4096)
        );
    }

    #[test]
    fn source_restoration_serializes_with_foreign_ordinary_reads() {
        use std::sync::mpsc;
        use std::time::Duration;

        let span = Span::default();
        let mut env = Env::new();
        env.define(
            "value".into(),
            Value::String("kept".repeat(1024)),
            true,
            span,
        )
        .unwrap();
        let (_, lease) = env.borrow("value", span).unwrap();
        let reader_env = env.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            reader_env
                .with_value("value", span, |value| {
                    let Value::String(value) = value else {
                        panic!("foreign ordinary reads must see the owned source, not a weak view")
                    };
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert_eq!(value.len(), 4096);
                    Ok(())
                })
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            env.find_cell("value").unwrap().try_lock().is_err(),
            "the ordinary reader must hold the source lock until its callback returns"
        );
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let restoring = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            drop(lease);
            finished_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            finished_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        reader.join().unwrap();
        restoring.join().unwrap();
        assert!(
            matches!(env.get("value", span).unwrap(), Value::String(value) if value.len() == 4096)
        );
    }

    #[test]
    fn public_admission_rejects_nested_views_but_internal_owned_stores_keep_storage() {
        let span = Span::default();
        let mut env = Env::new();
        let (view, _lease) = BorrowLease::temporary(Value::String("borrowed".into()));
        assert!(env
            .define("bad".into(), Value::Array(vec![view]), true, span)
            .is_err());

        let values = vec![Value::String("owned".repeat(100_000))];
        let allocation = values.as_ptr();
        env.define_owned("values".into(), Value::Array(values), true, span)
            .unwrap();
        env.with_value("values", span, |value| {
            let Value::Array(values) = value else {
                panic!("expected array")
            };
            assert_eq!(values.as_ptr(), allocation);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn call_borrow_keeps_large_storage_and_restores_after_errors() {
        let span = Span::default();
        let text = "borrowed bytes".repeat(100_000);
        let original = text.as_ptr();
        let mut env = Env::new();
        env.define("text".into(), Value::String(text), true, span)
            .unwrap();
        for _ in 0..64 {
            let (view, lease) = env.borrow("text", span).unwrap();
            let (second, second_lease) = env.borrow("text", span).unwrap();
            assert!(second_lease.is_none(), "reborrow must not own the root");
            for view in [&view, &second] {
                view.with_read(span, |value| {
                    let Value::String(text) = value else {
                        panic!("expected string")
                    };
                    assert_eq!(
                        text.as_ptr(),
                        original,
                        "a borrow must preserve the original allocation"
                    );
                    Ok(())
                })
                .unwrap();
            }
            assert!(env
                .assign("text", Value::Null, span)
                .unwrap_err()
                .message
                .contains("borrowed"));
            drop(lease);
            assert!(
                view.with_read(span, |_| Ok(())).is_err(),
                "weak view cannot outlive its call"
            );
            env.with_value("text", span, |value| {
                let Value::String(text) = value else {
                    panic!("source not restored")
                };
                assert_eq!(text.as_ptr(), original);
                Ok(())
            })
            .unwrap();
        }
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (_, _lease) = env.borrow("text", span).unwrap();
            panic!("simulate an unwinding call");
        }));
        assert!(unwind.is_err());
        env.with_value("text", span, |value| {
            let Value::String(text) = value else {
                panic!("source not restored after unwind")
            };
            assert_eq!(text.as_ptr(), original);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn temporary_borrow_drops_source_and_projected_views_do_not_retain_it() {
        let span = Span::default();
        let source = Value::Array(vec![Value::String("owned".repeat(100_000))]);
        let (view, lease) = BorrowLease::temporary(source);
        let Value::Borrowed(view) = view else {
            panic!("expected view")
        };
        let projection = view
            .project(crate::value::BorrowProjection::Index(0), span)
            .unwrap();
        assert_eq!(
            projection
                .with_read(span, |value| Ok(value.type_name()))
                .unwrap(),
            "str"
        );
        drop(lease);
        assert!(projection.with_read(span, |_| Ok(())).is_err());
    }

    #[test]
    fn borrowed_projection_and_self_append_keep_the_existing_allocation() {
        let mut values = Vec::with_capacity(8);
        values.push(Value::String("original".into()));
        let array_ptr = values.as_ptr();
        let mut text = String::with_capacity(32);
        text.push_str("Hello");
        let string_ptr = text.as_ptr();
        let mut env = Env::new();
        let span = Span::default();
        env.define("values".into(), Value::Array(values), true, span)
            .unwrap();
        env.define("text".into(), Value::String(text), true, span)
            .unwrap();

        env.with_value("values", span, |value| {
            let Value::Array(values) = value else {
                panic!("expected array");
            };
            assert_eq!(values.as_ptr(), array_ptr, "projection must not clone");
            Ok(())
        })
        .unwrap();
        env.append_array("values", Value::Int(7), span).unwrap();
        assert!(env.append_string("text", " 界\0!", span).unwrap());
        env.with_value("values", span, |value| {
            let Value::Array(values) = value else {
                panic!("expected array");
            };
            assert_eq!(values.as_ptr(), array_ptr, "reuse spare array capacity");
            assert_eq!(values.len(), 2);
            Ok(())
        })
        .unwrap();
        env.with_value("text", span, |value| {
            let Value::String(text) = value else {
                panic!("expected string");
            };
            assert_eq!(text.as_ptr(), string_ptr, "reuse spare string capacity");
            assert_eq!(text, "Hello 界\0!");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn repeated_appends_grow_geometrically_without_cloning_the_prefix() {
        let mut env = Env::new();
        let span = Span::default();
        env.define("values".into(), Value::Array(Vec::new()), true, span)
            .unwrap();
        env.define("text".into(), Value::String(String::new()), true, span)
            .unwrap();
        let mut previous_array_capacity = 0;
        let mut previous_string_capacity = 0;
        let mut array_growths = 0;
        let mut string_growths = 0;
        for index in 0..4096 {
            env.append_array("values", Value::Int(index), span).unwrap();
            assert!(env.append_string("text", "x", span).unwrap());
            let array_capacity = env
                .with_value("values", span, |value| match value {
                    Value::Array(values) => Ok(values.capacity()),
                    _ => panic!("expected array"),
                })
                .unwrap();
            let string_capacity = env
                .with_value("text", span, |value| match value {
                    Value::String(text) => Ok(text.capacity()),
                    _ => panic!("expected string"),
                })
                .unwrap();
            array_growths += usize::from(array_capacity != previous_array_capacity);
            string_growths += usize::from(string_capacity != previous_string_capacity);
            previous_array_capacity = array_capacity;
            previous_string_capacity = string_capacity;
        }
        assert!(array_growths <= 16, "array grew {array_growths} times");
        assert!(string_growths <= 16, "string grew {string_growths} times");
        assert!((4096..=8192).contains(&previous_array_capacity));
        assert!((4096..=8192).contains(&previous_string_capacity));
    }

    #[test]
    fn append_checks_mutability_and_task_ownership_before_changing_values() {
        let span = Span::default();
        let mut env = Env::new();
        env.define("text".into(), Value::String("kept".into()), false, span)
            .unwrap();
        env.define(
            "values".into(),
            Value::Array(vec![Value::Int(1)]),
            true,
            span,
        )
        .unwrap();
        env.find_cell("values")
            .unwrap()
            .lock()
            .unwrap()
            .owner_task_id = crate::runtime::task::current_task_id().wrapping_add(1);
        assert!(env
            .append_string("text", "wrong", span)
            .unwrap_err()
            .to_string()
            .contains("immutable"));
        assert!(env
            .append_array("values", Value::Int(2), span)
            .unwrap_err()
            .to_string()
            .contains("async task cannot modify captured variable"));
        assert!(matches!(env.get("text", span).unwrap(), Value::String(text) if text == "kept"));
        assert!(
            matches!(env.get("values", span).unwrap(), Value::Array(values) if values.len() == 1)
        );
    }

    #[test]
    fn captured_bindings_share_append_updates_across_environments() {
        let span = Span::default();
        let mut env = Env::new();
        env.define("values".into(), Value::Array(Vec::new()), true, span)
            .unwrap();
        assert!(env.is_unshared("values"));
        let mut captured = env.capture(&HashSet::from(["values".into()]));
        assert!(!env.is_unshared("values"));
        assert!(!captured.is_unshared("values"));
        captured
            .append_array("values", Value::Int(7), span)
            .unwrap();
        assert!(matches!(
            env.get("values", span).unwrap(),
            Value::Array(values) if values == vec![Value::Int(7)]
        ));
        drop(captured);
        assert!(env.is_unshared("values"));
    }
}
