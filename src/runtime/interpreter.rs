use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crate::{
    ast::{
        AssignTarget, BinaryOp, Expr, ExprKind, FnDecl, Item, Literal, MatchPattern, Program, Stmt,
        UnaryOp,
    },
    env::Env,
    error::{KuError, KuResult},
    lexer::Lexer,
    parser::Parser,
    span::{Position, Span},
    stdlib,
    value::Value,
};

const MAX_STEPS: usize = 1_000_000;
const MAX_CALL_DEPTH: usize = 16;

enum Flow {
    Continue,
    Return(Value),
    Fail(Value),
}

pub struct Interpreter {
    functions: HashMap<String, FnDecl>,
    structs: HashMap<String, HashSet<String>>,
    enums: HashMap<String, HashMap<String, usize>>,
    base_dir: PathBuf,
    steps: usize,
    call_depth: usize,
    pending_fail: Option<Value>,
}

impl Interpreter {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            base_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            steps: 0,
            call_depth: 0,
            pending_fail: None,
        }
    }

    pub fn with_base_dir(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            ..Self::new()
        }
    }

    pub fn run(&mut self, program: Program) -> KuResult<()> {
        for item in program.items {
            match item {
                Item::Function(function) => {
                    if self.functions.contains_key(&function.name) {
                        return Err(KuError::runtime(
                            format!("function '{}' is already defined", function.name),
                            function.span,
                        ));
                    }
                    self.functions.insert(function.name.clone(), function);
                }
                Item::Struct(decl) => {
                    self.structs.insert(
                        decl.name,
                        decl.fields.into_iter().map(|field| field.name).collect(),
                    );
                }
                Item::Enum(decl) => {
                    self.enums.insert(
                        decl.name,
                        decl.variants
                            .into_iter()
                            .map(|variant| (variant.name, variant.fields.len()))
                            .collect(),
                    );
                }
                Item::Module(_) | Item::Import(_) => {}
            }
        }
        let result = self.call_function("main", Vec::new(), entry_span(), 0)?;
        if let Value::Result { ok: false, value } = result {
            return Err(KuError::runtime(
                format!("unhandled recoverable error: {value}"),
                entry_span(),
            ));
        }
        Ok(())
    }

    fn call_function(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
        depth: usize,
    ) -> KuResult<Value> {
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::runtime(
                "maximum function call depth exceeded",
                span,
            ));
        }
        let function = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| KuError::runtime(format!("undefined function '{name}'"), span))?;
        self.call_depth += 1;
        let result = (|| -> KuResult<Value> {
            if function.params.len() != args.len() {
                return Err(KuError::runtime(
                    format!(
                        "function '{name}' expects {} arguments but got {}",
                        function.params.len(),
                        args.len()
                    ),
                    function.span,
                ));
            }

            let mut env = Env::new();
            for (param, value) in function.params.iter().zip(args) {
                env.define(param.name.clone(), value, false, param.span)?;
            }

            match self.exec_block(&function.body, &mut env, depth)? {
                Flow::Continue => Ok(Value::Null),
                Flow::Return(value) => Ok(value),
                Flow::Fail(value) => Ok(Value::Result {
                    ok: false,
                    value: Box::new(value),
                }),
            }
        })();
        self.call_depth -= 1;
        result
    }

    fn call_function_value(
        &mut self,
        params: &[String],
        body: &[Stmt],
        captured: &Env,
        args: Vec<Value>,
        span: Span,
        depth: usize,
    ) -> KuResult<Value> {
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::runtime(
                "maximum function call depth exceeded",
                span,
            ));
        }
        self.call_depth += 1;
        let result = (|| -> KuResult<Value> {
            if params.len() != args.len() {
                return Err(KuError::runtime(
                    format!(
                        "function value expects {} arguments but got {}",
                        params.len(),
                        args.len()
                    ),
                    span,
                ));
            }

            let mut env = captured.clone();
            env.push_scope();
            for (param, value) in params.iter().zip(args) {
                env.define(param.clone(), value, false, span)?;
            }

            let result = match self.exec_block(body, &mut env, depth)? {
                Flow::Continue => Ok(Value::Null),
                Flow::Return(value) => Ok(value),
                Flow::Fail(value) => Ok(Value::Result {
                    ok: false,
                    value: Box::new(value),
                }),
            };
            env.pop_scope();
            result
        })();
        self.call_depth -= 1;
        result
    }

    fn exec_block(&mut self, body: &[Stmt], env: &mut Env, depth: usize) -> KuResult<Flow> {
        env.push_scope();
        for stmt in body {
            let flow = self.exec_stmt(stmt, env, depth)?;
            if matches!(flow, Flow::Return(_) | Flow::Fail(_)) {
                env.pop_scope();
                return Ok(flow);
            }
        }
        env.pop_scope();
        Ok(Flow::Continue)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, env: &mut Env, depth: usize) -> KuResult<Flow> {
        self.tick(stmt_span(stmt))?;
        match stmt {
            Stmt::VarDecl {
                name,
                mutable,
                value,
                span,
                ..
            } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                env.define(
                    name.clone(),
                    value,
                    *mutable && !is_constant_name(name),
                    *span,
                )?;
                Ok(Flow::Continue)
            }
            Stmt::Assign { name, value, span } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                if env.contains(name) {
                    env.assign(name, value, *span)?;
                } else {
                    env.define(name.clone(), value, !is_constant_name(name), *span)?;
                }
                Ok(Flow::Continue)
            }
            Stmt::AssignTarget {
                target,
                value,
                span,
            } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                self.assign_target(target, value, env, depth, *span)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                Ok(Flow::Continue)
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if self.eval(condition, env, depth)?.is_truthy() {
                    self.exec_block(then_branch, env, depth)
                } else {
                    self.exec_block(else_branch, env, depth)
                }
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                while self.eval(condition, env, depth)?.is_truthy() {
                    self.tick(*span)?;
                    match self.exec_block(body, env, depth)? {
                        Flow::Continue => {}
                        flow @ (Flow::Return(_) | Flow::Fail(_)) => return Ok(flow),
                    }
                }
                Ok(Flow::Continue)
            }
            Stmt::For {
                name,
                iterable,
                body,
                span,
            } => {
                let values = self.eval(iterable, env, depth)?;
                let Value::Array(values) = values else {
                    return Err(KuError::runtime(
                        format!(
                            "type error: for expects array but got {}",
                            values.type_name()
                        ),
                        *span,
                    ));
                };
                for value in values {
                    self.tick(*span)?;
                    env.push_scope();
                    env.define(name.clone(), value, true, *span)?;
                    for stmt in body {
                        let flow = self.exec_stmt(stmt, env, depth)?;
                        if matches!(flow, Flow::Return(_) | Flow::Fail(_)) {
                            env.pop_scope();
                            return Ok(flow);
                        }
                    }
                    env.pop_scope();
                }
                Ok(Flow::Continue)
            }
            Stmt::Function(function) => {
                env.define(
                    function.name.clone(),
                    Value::Function {
                        params: function
                            .params
                            .iter()
                            .map(|param| param.name.clone())
                            .collect(),
                        body: function.body.clone(),
                        env: env.clone(),
                    },
                    false,
                    function.span,
                )?;
                Ok(Flow::Continue)
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                span,
            } => {
                let mut flow = self.exec_block(body, env, depth)?;
                if let Flow::Fail(value) = flow {
                    if let Some(name) = catch_name {
                        env.push_scope();
                        env.define(name.clone(), value, false, *span)?;
                        flow = Flow::Continue;
                        for stmt in catch_body {
                            flow = self.exec_stmt(stmt, env, depth)?;
                            if matches!(flow, Flow::Return(_) | Flow::Fail(_)) {
                                break;
                            }
                        }
                        env.pop_scope();
                    } else {
                        flow = Flow::Fail(value);
                    }
                }
                let finally_flow = self.exec_block(finally_body, env, depth)?;
                if matches!(finally_flow, Flow::Return(_) | Flow::Fail(_)) {
                    return Ok(finally_flow);
                }
                Ok(flow)
            }
            Stmt::Fail { value, .. } => {
                let value = self.eval(value, env, depth)?;
                Ok(Flow::Fail(value))
            }
            Stmt::Panic { value, span } => {
                let value = self.eval(value, env, depth)?;
                Err(KuError::runtime(format!("panic: {value}"), *span))
            }
            Stmt::Return { value, .. } => {
                let value = match value {
                    Some(value) => self.eval(value, env, depth)?,
                    None => Value::Null,
                };
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                Ok(Flow::Return(value))
            }
            Stmt::Print { value, .. } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                println!("{value}");
                Ok(Flow::Continue)
            }
            Stmt::Expr { expr, .. } => {
                self.eval(expr, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                Ok(Flow::Continue)
            }
        }
    }

    fn eval(&mut self, expr: &Expr, env: &mut Env, depth: usize) -> KuResult<Value> {
        self.tick(expr.span)?;
        match &expr.kind {
            ExprKind::Literal(Literal::Int(value)) => Ok(Value::Int(*value)),
            ExprKind::Literal(Literal::Float(value)) => Ok(Value::Float(*value)),
            ExprKind::Literal(Literal::Bool(value)) => Ok(Value::Bool(*value)),
            ExprKind::Literal(Literal::String(value)) => Ok(Value::String(value.clone())),
            ExprKind::Literal(Literal::TemplateString(value)) => self
                .eval_template(value, env, depth, expr.span)
                .map(Value::String),
            ExprKind::Literal(Literal::Null) => Ok(Value::Null),
            ExprKind::Variable(name) => env.get(name, expr.span),
            ExprKind::Unary { op, expr: right } => {
                let value = self.eval(right, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match (op, value) {
                    (UnaryOp::Negate, Value::Int(value)) => value
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| KuError::runtime("integer overflow", expr.span)),
                    (UnaryOp::Negate, Value::Float(value)) => Ok(Value::Float(-value)),
                    (UnaryOp::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
                    (_, value) => Err(KuError::runtime(
                        format!("invalid unary operation for {}", value.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::Binary { left, op, right } => {
                if *op == BinaryOp::And {
                    let left = self.eval(left, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    if !left.is_truthy() {
                        return Ok(Value::Bool(false));
                    }
                    let right = self.eval(right, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    return Ok(Value::Bool(right.is_truthy()));
                }
                if *op == BinaryOp::Or {
                    let left = self.eval(left, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    if left.is_truthy() {
                        return Ok(Value::Bool(true));
                    }
                    let right = self.eval(right, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    return Ok(Value::Bool(right.is_truthy()));
                }
                let left = self.eval(left, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let right = self.eval(right, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                eval_binary(*op, left, right, expr.span)
            }
            ExprKind::Call { callee, args } => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval(arg, env, depth)?);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                let args = values;
                if !dotted_builtin_is_shadowed(callee, env) {
                    if let Some(value) =
                        stdlib::eval_dotted_builtin(callee, &args, expr.span, &self.base_dir)?
                    {
                        return Ok(value);
                    }
                }
                if let Some((enum_name, variant)) = enum_variant_path(callee) {
                    if self.enums.contains_key(&enum_name) {
                        return self.construct_enum(&enum_name, &variant, args, expr.span);
                    }
                }
                if let ExprKind::Variable(name) = &callee.kind {
                    if self.functions.contains_key(name) {
                        return self.call_function(name, args, expr.span, depth + 1);
                    }
                    if !env.contains(name) {
                        if let Some(value) = stdlib::eval_builtin(name, &args, expr.span)? {
                            return Ok(value);
                        }
                    }
                }
                let callee_value = self.eval(callee, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match callee_value {
                    Value::Function {
                        params,
                        body,
                        env: captured,
                    } => self.call_function_value(
                        &params,
                        &body,
                        &captured,
                        args,
                        expr.span,
                        depth + 1,
                    ),
                    other => Err(KuError::runtime(
                        format!("cannot call {}", other.type_name()),
                        callee.span,
                    )),
                }
            }
            ExprKind::Function { params, body, .. } => Ok(Value::Function {
                params: params.iter().map(|param| param.name.clone()).collect(),
                body: body.clone(),
                env: env.clone(),
            }),
            ExprKind::Array(values) => {
                let mut result = Vec::with_capacity(values.len());
                for value in values {
                    result.push(self.eval(value, env, depth)?);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                Ok(Value::Array(result))
            }
            ExprKind::Index { target, index } => {
                let target = self.eval(target, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let Value::Array(values) = target else {
                    return Err(KuError::runtime(
                        format!("type error: cannot index {}", target.type_name()),
                        expr.span,
                    ));
                };
                let Value::Int(index) = index else {
                    return Err(KuError::runtime(
                        format!(
                            "type error: expected int index but got {}",
                            index.type_name()
                        ),
                        expr.span,
                    ));
                };
                if index < 0 || index as usize >= values.len() {
                    return Err(KuError::runtime("array index out of bounds", expr.span));
                }
                Ok(values[index as usize].clone())
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Variable(enum_name) = &target.kind {
                    if self
                        .enums
                        .get(enum_name)
                        .is_some_and(|variants| variants.get(name).is_some_and(|arity| *arity == 0))
                    {
                        return Ok(Value::Enum {
                            name: enum_name.clone(),
                            variant: name.clone(),
                            fields: Vec::new(),
                        });
                    }
                }
                let target = self.eval(target, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match target {
                    Value::Struct {
                        name: struct_name,
                        fields,
                    } => fields.get(name).cloned().ok_or_else(|| {
                        KuError::runtime(
                            format!("struct '{struct_name}' has no field '{name}'"),
                            expr.span,
                        )
                    }),
                    Value::Object(fields) => fields.get(name).cloned().ok_or_else(|| {
                        KuError::runtime(format!("object has no field '{name}'"), expr.span)
                    }),
                    other => Err(KuError::runtime(
                        format!("type error: {} has no fields", other.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::StructLiteral { name, fields } => {
                let mut values = HashMap::new();
                for (field, value) in fields {
                    values.insert(field.clone(), self.eval(value, env, depth)?);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                Ok(Value::Struct {
                    name: name.clone(),
                    fields: values,
                })
            }
            ExprKind::ObjectLiteral { fields } => {
                let mut values = HashMap::new();
                for (field, value) in fields {
                    values.insert(field.clone(), self.eval(value, env, depth)?);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                Ok(Value::Object(values))
            }
            ExprKind::Match { value, arms } => {
                let value = self.eval(value, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                for arm in arms {
                    env.push_scope();
                    let matched = match_pattern(&arm.pattern, &value, env, arm.span)?;
                    if matched {
                        if let Some(guard) = &arm.guard {
                            let guard = self.eval(guard, env, depth)?;
                            if self.pending_fail.is_some() {
                                env.pop_scope();
                                return Ok(Value::Null);
                            }
                            if !guard.is_truthy() {
                                env.pop_scope();
                                continue;
                            }
                        }
                        let result = self.eval(&arm.value, env, depth);
                        env.pop_scope();
                        return result;
                    }
                    env.pop_scope();
                }
                Err(KuError::runtime(
                    "match expression did not match any arm",
                    expr.span,
                ))
            }
            ExprKind::TryUnwrap { expr: inner } => {
                let value = self.eval(inner, env, depth)?;
                match value {
                    Value::Result { ok: true, value } => Ok(*value),
                    Value::Result { ok: false, value } => {
                        self.pending_fail = Some(*value);
                        Ok(Value::Null)
                    }
                    other => Err(KuError::runtime(
                        format!("'?' expects result but got {}", other.type_name()),
                        expr.span,
                    )),
                }
            }
        }
    }

    fn take_pending_fail(&mut self) -> Option<Value> {
        self.pending_fail.take()
    }

    fn construct_enum(
        &self,
        enum_name: &str,
        variant: &str,
        args: Vec<Value>,
        span: Span,
    ) -> KuResult<Value> {
        let Some(variants) = self.enums.get(enum_name) else {
            return Err(KuError::runtime(
                format!("undefined enum '{enum_name}'"),
                span,
            ));
        };
        let Some(expected) = variants.get(variant) else {
            return Err(KuError::runtime(
                format!("enum '{enum_name}' has no variant '{variant}'"),
                span,
            ));
        };
        if *expected != args.len() {
            return Err(KuError::runtime(
                format!(
                    "enum variant '{enum_name}.{variant}' expects {expected} arguments but got {}",
                    args.len()
                ),
                span,
            ));
        }
        Ok(Value::Enum {
            name: enum_name.to_string(),
            variant: variant.to_string(),
            fields: args,
        })
    }

    fn assign_target(
        &mut self,
        target: &AssignTarget,
        value: Value,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        match target {
            AssignTarget::Variable(name) => env.assign(name, value, span),
            AssignTarget::Index { target, index } => {
                let root = assignment_root(target).ok_or_else(|| {
                    KuError::runtime("assignment target must start with a variable", target.span)
                })?;
                let mut root_value = env.get(&root, target.span)?;
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(());
                }
                assign_index_value(&mut root_value, index, value, span)?;
                env.assign(&root, root_value, span)
            }
            AssignTarget::Field { target, name } => {
                let root = assignment_root(target).ok_or_else(|| {
                    KuError::runtime("assignment target must start with a variable", target.span)
                })?;
                let mut root_value = env.get(&root, target.span)?;
                assign_field_value(&mut root_value, name, value, span)?;
                env.assign(&root, root_value, span)
            }
        }
    }

    fn eval_template(
        &mut self,
        raw: &str,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<String> {
        let mut output = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    match next {
                        '{' | '}' => output.push(next),
                        _ => {
                            output.push('\\');
                            output.push(next);
                        }
                    }
                } else {
                    output.push('\\');
                }
                continue;
            }
            if ch != '{' {
                output.push(ch);
                continue;
            }

            let mut expr_source = String::new();
            let mut found_end = false;
            while let Some(inner) = chars.next() {
                if inner == '\\' {
                    if let Some(next) = chars.next() {
                        expr_source.push('\\');
                        expr_source.push(next);
                    }
                    continue;
                }
                if inner == '}' {
                    found_end = true;
                    break;
                }
                expr_source.push(inner);
            }
            if !found_end {
                return Err(KuError::runtime(
                    "unterminated template interpolation",
                    span,
                ));
            }
            if expr_source.trim().is_empty() {
                return Err(KuError::runtime("empty template interpolation", span));
            }

            let tokens = Lexer::new(&expr_source).tokenize()?;
            let expr = Parser::new(tokens).parse_expression_only()?;
            let value = self.eval_template_expr(&expr, env, depth)?;
            if self.pending_fail.is_some() {
                return Ok(String::new());
            }
            output.push_str(&value.to_string());
        }
        Ok(output)
    }

    fn eval_template_expr(&mut self, expr: &Expr, env: &mut Env, depth: usize) -> KuResult<Value> {
        match &expr.kind {
            ExprKind::Binary { left, op, right } if *op == BinaryOp::Add => {
                let left = self.eval_template_expr(left, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let right = self.eval_template_expr(right, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match eval_binary(*op, left.clone(), right.clone(), expr.span) {
                    Ok(value) => Ok(value),
                    Err(_) if can_template_concat_values(&left, &right) => {
                        Ok(Value::String(format!("{left}{right}")))
                    }
                    Err(err) => Err(err),
                }
            }
            ExprKind::Binary { left, op, right } => {
                let left = self.eval_template_expr(left, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let right = self.eval_template_expr(right, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                eval_binary(*op, left, right, expr.span)
            }
            ExprKind::Unary { op, expr: right } => {
                let value = self.eval_template_expr(right, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match (op, value) {
                    (UnaryOp::Negate, Value::Int(value)) => value
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| KuError::runtime("integer overflow", expr.span)),
                    (UnaryOp::Negate, Value::Float(value)) => Ok(Value::Float(-value)),
                    (UnaryOp::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
                    (_, value) => Err(KuError::runtime(
                        format!("invalid unary operation for {}", value.type_name()),
                        expr.span,
                    )),
                }
            }
            _ => self.eval(expr, env, depth),
        }
    }

    fn tick(&mut self, span: Span) -> KuResult<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            Err(KuError::runtime(
                "execution step limit exceeded; possible infinite loop or recursion",
                span,
            ))
        } else {
            Ok(())
        }
    }
}

fn assignment_root(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.clone()),
        _ => None,
    }
}

fn assign_index_value(target: &mut Value, index: Value, value: Value, span: Span) -> KuResult<()> {
    let Value::Array(values) = target else {
        return Err(KuError::runtime(
            format!("type error: cannot index {}", target.type_name()),
            span,
        ));
    };
    let Value::Int(index) = index else {
        return Err(KuError::runtime(
            format!(
                "type error: expected int index but got {}",
                index.type_name()
            ),
            span,
        ));
    };
    if index < 0 || index as usize >= values.len() {
        return Err(KuError::runtime("array index out of bounds", span));
    }
    values[index as usize] = value;
    Ok(())
}

fn assign_field_value(target: &mut Value, name: &str, value: Value, span: Span) -> KuResult<()> {
    match target {
        Value::Struct {
            name: struct_name,
            fields,
        } => {
            if !fields.contains_key(name) {
                return Err(KuError::runtime(
                    format!("struct '{struct_name}' has no field '{name}'"),
                    span,
                ));
            }
            fields.insert(name.to_string(), value);
            Ok(())
        }
        Value::Object(fields) => {
            if !fields.contains_key(name) {
                return Err(KuError::runtime(
                    format!("object has no field '{name}'"),
                    span,
                ));
            }
            fields.insert(name.to_string(), value);
            Ok(())
        }
        other => Err(KuError::runtime(
            format!("type error: {} has no fields", other.type_name()),
            span,
        )),
    }
}

fn match_pattern(
    pattern: &MatchPattern,
    value: &Value,
    env: &mut Env,
    span: Span,
) -> KuResult<bool> {
    match pattern {
        MatchPattern::Wildcard => Ok(true),
        MatchPattern::Literal(literal) => Ok(value == &value_from_literal(literal)),
        MatchPattern::EnumVariant {
            enum_name,
            variant,
            bindings,
        } => {
            let Value::Enum {
                name,
                variant: actual_variant,
                fields,
            } = value
            else {
                return Ok(false);
            };
            if name != enum_name || actual_variant != variant {
                return Ok(false);
            }
            if bindings.len() != fields.len() {
                return Err(KuError::runtime(
                    format!(
                        "match pattern '{enum_name}.{variant}' expects {} bindings but got {}",
                        fields.len(),
                        bindings.len()
                    ),
                    span,
                ));
            }
            for (binding, field) in bindings.iter().zip(fields.iter()) {
                env.define(binding.clone(), field.clone(), false, span)?;
            }
            Ok(true)
        }
    }
}

fn value_from_literal(literal: &Literal) -> Value {
    match literal {
        Literal::Int(value) => Value::Int(*value),
        Literal::Float(value) => Value::Float(*value),
        Literal::Bool(value) => Value::Bool(*value),
        Literal::String(value) | Literal::TemplateString(value) => Value::String(value.clone()),
        Literal::Null => Value::Null,
    }
}

fn enum_variant_path(expr: &Expr) -> Option<(String, String)> {
    let ExprKind::Field { target, name } = &expr.kind else {
        return None;
    };
    let ExprKind::Variable(enum_name) = &target.kind else {
        return None;
    };
    Some((enum_name.clone(), name.clone()))
}

fn dotted_builtin_is_shadowed(expr: &Expr, env: &Env) -> bool {
    let ExprKind::Field { target, .. } = &expr.kind else {
        return false;
    };
    let ExprKind::Variable(module) = &target.kind else {
        return false;
    };
    env.contains(module)
}

fn eval_binary(op: BinaryOp, left: Value, right: Value, span: Span) -> KuResult<Value> {
    match op {
        BinaryOp::Add => match (left, right) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(b)
                .map(Value::Int)
                .ok_or_else(|| KuError::runtime("integer overflow", span)),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + b as f64)),
            (Value::String(a), Value::String(b)) => Ok(Value::String(a + &b)),
            (a, b) => type_error(span, "+", a, b),
        },
        BinaryOp::Subtract => numeric(left, right, span, false, checked_sub, |a, b| a - b),
        BinaryOp::Multiply => numeric(left, right, span, false, checked_mul, |a, b| a * b),
        BinaryOp::Divide => numeric(left, right, span, true, checked_div, |a, b| a / b),
        BinaryOp::Remainder => match (left, right) {
            (Value::Int(_), Value::Int(0)) => Err(KuError::runtime("division by zero", span)),
            (Value::Int(a), Value::Int(b)) => a
                .checked_rem(b)
                .map(Value::Int)
                .ok_or_else(|| KuError::runtime("integer overflow", span)),
            (a, b) => type_error(span, "%", a, b),
        },
        BinaryOp::Equal => Ok(Value::Bool(left == right)),
        BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
        BinaryOp::Less => compare(left, right, span, |a, b| a < b),
        BinaryOp::LessEqual => compare(left, right, span, |a, b| a <= b),
        BinaryOp::Greater => compare(left, right, span, |a, b| a > b),
        BinaryOp::GreaterEqual => compare(left, right, span, |a, b| a >= b),
        BinaryOp::And | BinaryOp::Or => unreachable!("logical operators are short-circuited"),
    }
}

fn numeric(
    left: Value,
    right: Value,
    span: Span,
    check_zero: bool,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> KuResult<Value> {
    match (left, right) {
        (Value::Int(_), Value::Int(0)) if check_zero => {
            Err(KuError::runtime("division by zero", span))
        }
        (Value::Float(_), Value::Float(value)) if check_zero && value == 0.0 => {
            Err(KuError::runtime("division by zero", span))
        }
        (Value::Int(_), Value::Float(value)) if check_zero && value == 0.0 => {
            Err(KuError::runtime("division by zero", span))
        }
        (Value::Float(_), Value::Int(0)) if check_zero => {
            Err(KuError::runtime("division by zero", span))
        }
        (Value::Int(a), Value::Int(b)) => int_op(a, b)
            .map(Value::Int)
            .ok_or_else(|| KuError::runtime("integer overflow", span)),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(float_op(a, b))),
        (Value::Int(a), Value::Float(b)) => Ok(Value::Float(float_op(a as f64, b))),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Float(float_op(a, b as f64))),
        (a, b) => type_error(span, "numeric operator", a, b),
    }
}

fn checked_sub(left: i64, right: i64) -> Option<i64> {
    left.checked_sub(right)
}

fn checked_mul(left: i64, right: i64) -> Option<i64> {
    left.checked_mul(right)
}

fn checked_div(left: i64, right: i64) -> Option<i64> {
    left.checked_div(right)
}

fn compare(left: Value, right: Value, span: Span, op: fn(f64, f64) -> bool) -> KuResult<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Bool(op(a as f64, b as f64))),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Bool(op(a, b))),
        (Value::Int(a), Value::Float(b)) => Ok(Value::Bool(op(a as f64, b))),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Bool(op(a, b as f64))),
        (a, b) => type_error(span, "comparison operator", a, b),
    }
}

fn type_error(span: Span, op: &str, left: Value, right: Value) -> KuResult<Value> {
    Err(KuError::runtime(
        format!(
            "type error: cannot apply {op} to {} and {}",
            left.type_name(),
            right.type_name()
        ),
        span,
    ))
}

fn can_template_concat_values(left: &Value, right: &Value) -> bool {
    matches!(
        (left, right),
        (Value::String(_), Value::Int(_) | Value::Float(_))
            | (Value::Int(_) | Value::Float(_), Value::String(_))
            | (Value::String(_), Value::String(_))
    )
}

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::VarDecl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::AssignTarget { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Function(FnDecl { span, .. })
        | Stmt::Return { span, .. }
        | Stmt::Try { span, .. }
        | Stmt::Fail { span, .. }
        | Stmt::Panic { span, .. }
        | Stmt::Print { span, .. }
        | Stmt::Expr { span, .. } => *span,
    }
}

fn entry_span() -> Span {
    Span::point(Position::new(1, 1, 0))
}

fn is_constant_name(name: &str) -> bool {
    let mut has_alpha = false;
    for ch in name.chars() {
        if ch.is_ascii_alphabetic() {
            has_alpha = true;
            if ch.is_ascii_lowercase() {
                return false;
            }
        } else if ch != '_' && !ch.is_ascii_digit() {
            return false;
        }
    }
    has_alpha
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
