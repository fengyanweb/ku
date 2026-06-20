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
                if !binding.mutable {
                    return Err(KuError::runtime(
                        format!("cannot assign to immutable variable '{}'", name),
                        span,
                    ));
                }
                let current_task_id = crate::runtime::task::current_task_id();
                if binding.owner_task_id != current_task_id {
                    return Err(KuError::runtime(
                        format!("async task cannot modify captured variable '{}'", name),
                        span,
                    ));
                }
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
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
                return binding
                    .lock()
                    .map(|binding| binding.value.clone())
                    .map_err(|_| KuError::runtime("environment binding is poisoned", span));
            }
        }
        Err(KuError::runtime(
            format!("undefined variable '{}'", name),
            span,
        ))
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}
