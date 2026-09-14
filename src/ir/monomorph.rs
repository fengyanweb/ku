//! Reify the checker's bounded, concrete generic plan before ordinary lowering.
//! There is no generic runtime representation and no change to call evaluation.
use super::*;
use crate::{
    ast::FnDecl,
    checker::{native_local_generic_span, native_specialization_plan, GenericCallSite},
};
use std::collections::{BTreeMap, BTreeSet};

const MAX_REWRITE_DEPTH: usize = 64;
const MAX_REWRITE_NODES: usize = 262_144;

fn error(span: Span, message: &str) -> KuError {
    KuError::runtime(format!("native generic specialization: {message}"), span)
}

pub(super) fn specialize(program: &Program) -> KuResult<Option<Program>> {
    let mut templates = BTreeMap::new();
    for item in &program.items {
        if let Item::Function(function) = item {
            if let Some(span) = native_local_generic_span(&function.body, function.span)? {
                return Err(error(
                    span,
                    "local generic functions are not supported by this native slice yet",
                ));
            }
            if !function.type_params.is_empty() {
                templates.insert(function.name.as_str(), function);
            }
        }
    }
    if templates.is_empty() {
        return Ok(None);
    }
    let plan = native_specialization_plan(program)?;
    let mut result = Program {
        items: Vec::with_capacity(program.items.len() + plan.instances.len()),
    };
    let empty = BTreeMap::new();
    let mut work = 0;
    for item in &program.items {
        match item {
            Item::Function(function) if !function.type_params.is_empty() => {}
            Item::Function(function) => {
                let mut function = function.clone();
                let context = function.name.clone();
                Rewriter::new(&context, &empty, &plan.calls, &mut work).function(&mut function)?;
                result.items.push(Item::Function(function));
            }
            _ => result.items.push(item.clone()),
        }
    }
    for instance in plan.instances {
        let template = templates
            .get(instance.source.as_str())
            .ok_or_else(|| KuError::message("native generic declaration identity is missing"))?;
        let mut function = (**template).clone();
        function.name = instance.symbol;
        function.type_params.clear();
        let context = function.name.clone();
        Rewriter::new(&context, &instance.bindings, &plan.calls, &mut work)
            .function(&mut function)?;
        // These types are already concrete. Do not substitute them a second
        // time: a nominal type can have the same name as a template variable.
        for (parameter, ty) in function.params.iter_mut().zip(instance.parameters) {
            parameter.ty = Some(ty);
        }
        function.return_type = Some(instance.returns);
        result.items.push(Item::Function(function));
    }
    Ok(Some(result))
}

struct Rewriter<'a> {
    context: &'a str,
    bindings: &'a BTreeMap<String, TypeName>,
    calls: &'a BTreeMap<GenericCallSite, String>,
    used: BTreeSet<GenericCallSite>,
    work: &'a mut usize,
}

impl<'a> Rewriter<'a> {
    fn new(
        context: &'a str,
        bindings: &'a BTreeMap<String, TypeName>,
        calls: &'a BTreeMap<GenericCallSite, String>,
        work: &'a mut usize,
    ) -> Self {
        Self {
            context,
            bindings,
            calls,
            used: BTreeSet::new(),
            work,
        }
    }

    fn charge(&mut self, depth: usize, span: Span) -> KuResult<()> {
        if depth > MAX_REWRITE_DEPTH || *self.work >= MAX_REWRITE_NODES {
            return Err(error(span, "expanded syntax work/depth limit exceeded"));
        }
        *self.work += 1;
        Ok(())
    }

    fn ty(&mut self, ty: &mut TypeName, depth: usize, span: Span) -> KuResult<()> {
        self.charge(depth, span)?;
        match ty {
            TypeName::Custom(name) => {
                if let Some(concrete) = self.bindings.get(name) {
                    *ty = concrete.clone();
                }
            }
            TypeName::Array(inner) | TypeName::Result(inner) => self.ty(inner, depth + 1, span)?,
            TypeName::Function {
                params,
                return_type,
                ..
            } => {
                for parameter in params {
                    self.ty(parameter, depth + 1, span)?;
                }
                self.ty(return_type, depth + 1, span)?;
            }
            TypeName::Union(types) => {
                for ty in types {
                    self.ty(ty, depth + 1, span)?;
                }
            }
            TypeName::Int
            | TypeName::Float
            | TypeName::Bool
            | TypeName::String
            | TypeName::Null => {}
        }
        Ok(())
    }

    fn function(&mut self, function: &mut FnDecl) -> KuResult<()> {
        for parameter in &mut function.params {
            if let Some(ty) = &mut parameter.ty {
                self.ty(ty, 0, parameter.span)?;
            }
        }
        if let Some(ty) = &mut function.return_type {
            self.ty(ty, 0, function.span)?;
        }
        self.body(&mut function.body, 0)?;
        if self
            .calls
            .keys()
            .any(|site| site.context == self.context && !self.used.contains(site))
        {
            return Err(error(function.span, "generic calls inside reparsed templates or unresolved callable bodies are not supported yet"));
        }
        Ok(())
    }

    fn body(&mut self, body: &mut [Stmt], depth: usize) -> KuResult<()> {
        for statement in body {
            self.charge(depth, Span::default())?;
            match statement {
                Stmt::VarDecl {
                    ty, value, span, ..
                } => {
                    if let Some(ty) = ty {
                        self.ty(ty, depth + 1, *span)?;
                    }
                    self.expr(value, depth + 1)?;
                }
                Stmt::Assign { value, .. }
                | Stmt::Fail { value, .. }
                | Stmt::Panic { value, .. }
                | Stmt::Print { value, .. } => self.expr(value, depth + 1)?,
                Stmt::ObjectDestructureAssign {
                    bindings, value, ..
                } => {
                    self.expr(value, depth + 1)?;
                    for binding in bindings {
                        if let Some(default) = &mut binding.default {
                            self.expr(default, depth + 1)?;
                        }
                    }
                }
                Stmt::AssignTarget { target, value, .. }
                | Stmt::CompoundAssign { target, value, .. } => {
                    self.target(target, depth + 1)?;
                    self.expr(value, depth + 1)?;
                }
                Stmt::Expr { expr, .. } => self.expr(expr, depth + 1)?,
                Stmt::Return { value, .. } => {
                    if let Some(value) = value {
                        self.expr(value, depth + 1)?;
                    }
                }
                Stmt::DestructureAssign { values, .. } => {
                    for value in values {
                        self.expr(value, depth + 1)?;
                    }
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    self.expr(condition, depth + 1)?;
                    self.body(then_branch, depth + 1)?;
                    self.body(else_branch, depth + 1)?;
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    self.expr(condition, depth + 1)?;
                    self.body(body, depth + 1)?;
                }
                Stmt::For { iterable, body, .. } => {
                    self.expr(iterable, depth + 1)?;
                    self.body(body, depth + 1)?;
                }
                Stmt::Try {
                    body,
                    catch_body,
                    finally_body,
                    ..
                } => {
                    self.body(body, depth + 1)?;
                    self.body(catch_body, depth + 1)?;
                    self.body(finally_body, depth + 1)?;
                }
                Stmt::Function(function) => {
                    if !function.type_params.is_empty() {
                        return Err(error(
                            function.span,
                            "local generic functions are not supported yet",
                        ));
                    }
                    for parameter in &mut function.params {
                        if let Some(ty) = &mut parameter.ty {
                            self.ty(ty, depth + 1, parameter.span)?;
                        }
                    }
                    if let Some(ty) = &mut function.return_type {
                        self.ty(ty, depth + 1, function.span)?;
                    }
                    self.body(&mut function.body, depth + 1)?;
                }
                Stmt::Break { .. } | Stmt::Continue { .. } => {}
            }
        }
        Ok(())
    }

    fn target(&mut self, target: &mut AssignTarget, depth: usize) -> KuResult<()> {
        match target {
            AssignTarget::Variable(_) => Ok(()),
            AssignTarget::Field { target, .. } => self.expr(target, depth),
            AssignTarget::Index { target, index } => {
                self.expr(target, depth)?;
                self.expr(index, depth)
            }
        }
    }

    fn expr(&mut self, expression: &mut Expr, depth: usize) -> KuResult<()> {
        self.charge(depth, expression.span)?;
        match &mut expression.kind {
            ExprKind::Call { callee, args } => {
                if let ExprKind::Variable(name) = &mut callee.kind {
                    let site = GenericCallSite::new(self.context, name, expression.span);
                    if let Some(symbol) = self.calls.get(&site) {
                        *name = symbol.clone();
                        self.used.insert(site);
                    }
                }
                self.expr(callee, depth + 1)?;
                for argument in args {
                    self.expr(argument, depth + 1)?;
                }
            }
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::TryUnwrap { expr } => {
                self.expr(expr, depth + 1)?
            }
            ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
                self.expr(target, depth + 1)?
            }
            ExprKind::Binary { left, right, .. } => {
                self.expr(left, depth + 1)?;
                self.expr(right, depth + 1)?;
            }
            ExprKind::Index { target, index } => {
                self.expr(target, depth + 1)?;
                self.expr(index, depth + 1)?;
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.expr(value, depth + 1)?;
                }
            }
            ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                for (_, value) in fields {
                    self.expr(value, depth + 1)?;
                }
            }
            ExprKind::Match { value, arms } => {
                self.expr(value, depth + 1)?;
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.expr(guard, depth + 1)?;
                    }
                    self.expr(&mut arm.value, depth + 1)?;
                }
            }
            ExprKind::Function {
                params,
                return_type,
                body,
            } => {
                for parameter in params {
                    if let Some(ty) = &mut parameter.ty {
                        self.ty(ty, depth + 1, parameter.span)?;
                    }
                }
                if let Some(ty) = return_type {
                    self.ty(ty, depth + 1, expression.span)?;
                }
                self.body(body, depth + 1)?;
            }
            ExprKind::Literal(_) | ExprKind::Variable(_) => {}
        }
        Ok(())
    }
}
