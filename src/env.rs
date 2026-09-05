use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use crate::error::{KuError, KuResult};
use crate::span::Span;
use crate::value::{BorrowedValue, Value};

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
    owner_task_id: i64,
    borrowed_parameter: bool,
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

#[derive(Debug, Clone)]
pub struct Env {
    scopes: Vec<HashMap<String, BindingCell>>,
}

impl Env {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
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
        if scope.contains_key(&name) {
            return Err(KuError::runtime(
                format!("variable '{}' is already defined in this scope", name),
                span,
            ));
        }
        scope.insert(
            name,
            Arc::new(Mutex::new(Binding {
                borrowed_parameter: matches!(value, Value::Borrowed(_)),
                value,
                mutable,
                owner_task_id: crate::runtime::task::current_task_id(),
            })),
        );
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
            scopes: vec![captured],
        }
    }

    fn find_cell(&self, name: &str) -> Option<BindingCell> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
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
            if let Some(binding) = scope.get(name) {
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
            .any(|scope| scope.contains_key(name))
    }

    pub fn get(&self, name: &str, span: Span) -> KuResult<Value> {
        self.with_value(name, span, |value| Ok(value.clone()))
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
            if let Some(binding) = scope.get(name) {
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
            .find_map(|scope| scope.get(name))
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
mod collection_tests {
    use super::*;

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
