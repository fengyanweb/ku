use std::collections::HashMap;

use crate::error::{KuError, KuResult};
use crate::span::Span;
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
}

#[derive(Debug, Clone)]
pub struct Env {
    scopes: Vec<HashMap<String, Binding>>,
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
        scope.insert(name, Binding { value, mutable });
        Ok(())
    }

    pub fn assign(&mut self, name: &str, value: Value, span: Span) -> KuResult<()> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.get_mut(name) {
                if !binding.mutable {
                    return Err(KuError::runtime(
                        format!("cannot assign to immutable variable '{}'", name),
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
                return Ok(binding.value.clone());
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
