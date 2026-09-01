use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use crate::error::{KuError, KuResult};
use crate::span::Span;
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
    owner_task_id: i64,
}

impl Binding {
    fn check_assignable(&self, name: &str, span: Span) -> KuResult<()> {
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
                value,
                mutable,
                owner_task_id: crate::runtime::task::current_task_id(),
            })),
        );
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

    /// Only used for a proven unshared `xs = xs.push(piece)` after `piece` has
    /// been evaluated. General `push` remains a pure operation producing a copy.
    pub(crate) fn append_array(&mut self, name: &str, value: Value, span: Span) -> KuResult<()> {
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
    fn captured_bindings_are_not_eligible_for_unshared_array_reuse() {
        let span = Span::default();
        let mut env = Env::new();
        env.define("values".into(), Value::Array(Vec::new()), true, span)
            .unwrap();
        assert!(env.is_unshared("values"));
        let captured = env.capture(&HashSet::from(["values".into()]));
        assert!(!env.is_unshared("values"));
        assert!(!captured.is_unshared("values"));
        drop(captured);
        assert!(env.is_unshared("values"));
    }
}
