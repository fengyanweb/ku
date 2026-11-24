//! Calls of a statically known lexical closure can use the Task's own bindings.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{Expr, ExprKind, Literal, Stmt, TypeName};
use crate::error::KuResult;
use crate::span::Span;

use super::{
    copy_value, task, unsupported, value_slot, value_type, FunctionLowerer, SlotId, TaskSlotType,
};

#[derive(Clone)]
pub(super) enum LocalBinding<'a> {
    Slot(SlotId),
    Closure(Rc<LexicalClosure<'a>>),
}

#[derive(Clone, Default)]
pub(super) struct LocalScope<'a> {
    bindings: HashMap<String, LocalBinding<'a>>,
}

impl<'a> LocalScope<'a> {
    pub(super) fn insert(&mut self, name: String, slot: SlotId) -> Option<LocalBinding<'a>> {
        self.bindings.insert(name, LocalBinding::Slot(slot))
    }

    pub(super) fn get(&self, name: &str) -> Option<&LocalBinding<'a>> {
        self.bindings.get(name)
    }

    pub(super) fn contains_key(&self, name: &str) -> bool {
        self.bindings.contains_key(name)
    }

    pub(super) fn len(&self) -> usize {
        self.bindings.len()
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &String> {
        self.bindings.keys()
    }
}

pub(super) struct LexicalClosure<'a> {
    body: &'a Expr,
    captures: LocalScope<'a>,
    result: TaskSlotType,
}

fn annotation_result(annotation: Option<&TypeName>, span: Span) -> KuResult<Option<&TypeName>> {
    match annotation {
        None => Ok(None),
        Some(TypeName::Function {
            params,
            param_modes,
            return_type,
            is_async: false,
        }) if params.is_empty() && param_modes.is_empty() => Ok(Some(return_type.as_ref())),
        _ => Err(unsupported("this Task lexical closure signature", span)),
    }
}

impl<'a> FunctionLowerer<'a> {
    pub(super) fn bind_lexical_closure(
        &mut self,
        name: &str,
        annotation: Option<&TypeName>,
        value: &'a Expr,
    ) -> KuResult<()> {
        let span = value.span;
        let ExprKind::Function {
            params,
            return_type,
            body,
        } = &value.kind
        else {
            unreachable!("closure literal dispatch")
        };
        let [Stmt::Return {
            value: Some(body), ..
        }] = body.as_slice()
        else {
            return Err(unsupported(
                "a Task lexical closure body other than a single return expression",
                span,
            ));
        };
        if !params.is_empty() {
            return Err(unsupported("parameters of a Task lexical closure", span));
        }
        let annotated_result = annotation_result(annotation, span)?;
        let result_type = annotated_result.or(return_type.as_ref()).ok_or_else(|| {
            unsupported(
                "a Task lexical closure without an explicit return type",
                span,
            )
        })?;
        let result = value_slot(value_type(result_type, span, self.layouts)?);
        if !copy_value(&result) {
            return Err(unsupported(
                "a Task lexical closure returning a non-Copy value",
                span,
            ));
        }
        self.budget.text(name.len(), span)?;
        let captures = self.capture_slots(body)?;
        self.locals
            .last_mut()
            .expect("active closure scope")
            .bindings
            .insert(
                name.to_owned(),
                LocalBinding::Closure(Rc::new(LexicalClosure {
                    body,
                    captures,
                    result,
                })),
            );
        Ok(())
    }

    pub(super) fn bind_lexical_closure_alias(
        &mut self,
        name: &str,
        annotation: Option<&TypeName>,
        closure: Rc<LexicalClosure<'a>>,
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        self.budget.expression(depth, span)?;
        if let Some(result) = annotation_result(annotation, span)? {
            let annotated = value_slot(value_type(result, span, self.layouts)?);
            if annotated != closure.result {
                return Err(unsupported(
                    "a mismatched Task lexical closure signature",
                    span,
                ));
            }
        }
        self.budget.text(name.len(), span)?;
        // A fresh lexical name refers to the original capture slots. It has
        // no runtime representation and cannot outlive the enclosing scopes.
        self.locals
            .last_mut()
            .expect("active closure scope")
            .bindings
            .insert(name.to_owned(), LocalBinding::Closure(closure));
        Ok(())
    }

    fn capture_slots(&mut self, body: &Expr) -> KuResult<LocalScope<'a>> {
        let mut captured = LocalScope::default();
        // Bounded source analysis even for an uncalled closure. Pure body
        // expressions use the same checked operations as their Task caller.
        let mut pending = vec![(body, 0usize)];
        let mut visited = 0usize;
        while let Some((expression, depth)) = pending.pop() {
            visited += 1;
            if depth > 16 || visited + pending.len() > 64 {
                return Err(unsupported(
                    "Task lexical closure body beyond 16 levels or 64 expressions",
                    expression.span,
                ));
            }
            self.budget.analysis(1, expression.span)?;
            match &expression.kind {
                ExprKind::Literal(
                    Literal::Int(_) | Literal::Bool(_) | Literal::Null | Literal::String(_),
                ) => {}
                ExprKind::Variable(name) => {
                    if captured.contains_key(name) {
                        continue;
                    }
                    let slot = self.local(name).ok_or_else(|| unsupported(&format!("Task lexical closure capture of {name} without an ordinary local binding"), expression.span))?;
                    let ty = &self.function.slots[slot.0].ty;
                    if !matches!(ty, TaskSlotType::Value { ty: inner, borrowed: false }
                        if copy_value(ty)
                            || task::record_type(inner)
                            || task::nullable_record(inner)
                            || task::error_type(inner))
                    {
                        return Err(unsupported(
                            &format!("Task lexical closure capture of {name} with this type"),
                            expression.span,
                        ));
                    }
                    self.budget.text(name.len(), expression.span)?;
                    captured.insert(name.clone(), slot);
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Field { target: expr, .. }
                | ExprKind::OptionalField { target: expr, .. } => pending.push((expr, depth + 1)),
                ExprKind::Binary { left, right, .. } => {
                    pending.push((right, depth + 1));
                    pending.push((left, depth + 1));
                }
                ExprKind::Call { callee, args }
                    if args.is_empty()
                        && matches!(&callee.kind, ExprKind::Field { target, name }
                            if matches!(name.as_str(), "len" | "byte_len")
                                && matches!(&target.kind, ExprKind::Field { .. } | ExprKind::OptionalField { .. })) =>
                {
                    // Keep the ordinary Task receiver/type check. Visiting the
                    // callee accounts for its field path and captures its root.
                    pending.push((callee, depth + 1));
                }
                _ => {
                    return Err(unsupported(
                        "calls, suspension or construction inside a Task lexical closure body",
                        expression.span,
                    ))
                }
            }
        }
        Ok(captured)
    }

    #[inline(never)]
    pub(super) fn call_lexical_closure(
        &mut self,
        closure: Rc<LexicalClosure<'a>>,
        args: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        if !args.is_empty() {
            return Err(unsupported("arguments to a Task lexical closure", span));
        }
        self.budget.analysis(closure.captures.len(), span)?;
        for name in closure.captures.keys() {
            self.budget.text(name.len(), span)?;
        }
        // Each capture denotes its declaration's exact binding, never the
        // caller's shadow of the name. A field read after Await marks that
        // same owner live in the existing Task liveness plan.
        self.locals.push(closure.captures.clone());
        let value = self.expression(closure.body, depth + 1);
        self.locals.pop();
        let value = self.nullable_coerce(value?, &closure.result)?;
        if !self.nullable_argument_matches(value, &closure.result) {
            return Err(unsupported(
                "this Task lexical closure return value",
                closure.body.span,
            ));
        }
        self.copy_or_move(value)
    }
}
