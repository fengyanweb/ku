use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    ast::{
        AssignTarget, BinaryOp, Expr, ExprKind, FnDecl, FunctionParam, Item, Literal, MatchPattern,
        Program, Stmt, UnaryOp,
    },
    env::Env,
    error::{KuError, KuResult},
    lexer::Lexer,
    parser::Parser,
    runtime::task::{current_task_cancelled, TaskRuntime},
    span::{Position, Span},
    stdlib,
    value::Value,
};

const MAX_STEPS: usize = 1_000_000;
const MAX_CALL_DEPTH: usize = 16;
const HTTP_HANDLER_TIMEOUT_MESSAGE: &str = "http handler timeout";
const HTTP_ACCEPT_BATCH: usize = 64;
const HTTP_EVENT_LOOP_SLEEP: Duration = Duration::from_millis(1);

static HTTP_LISTENERS: OnceLock<Mutex<HashMap<i64, TcpListener>>> = OnceLock::new();
static NEXT_HTTP_LISTENER_ID: AtomicI64 = AtomicI64::new(1);

enum Flow {
    Continue,
    Break,
    LoopContinue,
    Return(Value),
    Fail(Value),
}

struct FunctionValueCall<'a> {
    params: &'a [String],
    body: &'a [Stmt],
    captures: &'a Env,
    self_name: &'a Option<String>,
    is_async: bool,
    args: Vec<Value>,
    span: Span,
    depth: usize,
}

pub struct Interpreter {
    functions: HashMap<String, FnDecl>,
    structs: HashMap<String, HashSet<String>>,
    enums: HashMap<String, HashMap<String, usize>>,
    base_dir: PathBuf,
    steps: usize,
    call_depth: usize,
    pending_fail: Option<Value>,
    std_modules: HashSet<String>,
    execution_deadline: Option<Instant>,
    task_runtime: Option<TaskRuntime>,
    async_execution: bool,
}

#[derive(Clone)]
struct InterpreterTemplate {
    functions: HashMap<String, FnDecl>,
    structs: HashMap<String, HashSet<String>>,
    enums: HashMap<String, HashMap<String, usize>>,
    base_dir: PathBuf,
    std_modules: HashSet<String>,
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
            std_modules: HashSet::new(),
            execution_deadline: None,
            task_runtime: None,
            async_execution: false,
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
                Item::Module(module) => {
                    if let Some(name) = module.name.strip_prefix("std:") {
                        self.std_modules.insert(name.to_string());
                    }
                }
                Item::Import(_) => {}
            }
        }
        self.task_runtime = Some(TaskRuntime::new());
        let result = (|| -> KuResult<Value> {
            let result = self.call_function("main", Vec::new(), entry_span(), 0)?;
            match result {
                Value::Task(task) => task.await_result(),
                value => Ok(value),
            }
        })();
        let shutdown = self
            .task_runtime
            .as_ref()
            .map(|runtime| runtime.cancel_all_and_wait(Duration::from_secs(1)))
            .transpose();
        let result = result?;
        shutdown?;
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
        if function.is_async {
            return self.spawn_async_function(function, args, span);
        }
        self.call_function_direct(function, args, span, depth)
    }

    fn call_function_direct(
        &mut self,
        function: FnDecl,
        args: Vec<Value>,
        _span: Span,
        depth: usize,
    ) -> KuResult<Value> {
        self.call_depth += 1;
        let result = (|| -> KuResult<Value> {
            if function.params.len() != args.len() {
                return Err(KuError::runtime(
                    format!(
                        "function '{}' expects {} arguments but got {}",
                        function.name,
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
                Flow::Break | Flow::LoopContinue => Err(KuError::runtime(
                    "loop control escaped function",
                    function.span,
                )),
            }
        })();
        self.call_depth -= 1;
        result
    }

    fn spawn_async_function(
        &self,
        function: FnDecl,
        args: Vec<Value>,
        span: Span,
    ) -> KuResult<Value> {
        let runtime = self
            .task_runtime
            .clone()
            .ok_or_else(|| KuError::runtime("async task runtime is not initialized", span))?;
        let template = self.template();
        let child_runtime = runtime.clone();
        let task = runtime.spawn(move || {
            let mut interpreter = Interpreter::from_template(template, child_runtime);
            interpreter.async_execution = true;
            interpreter.call_function_direct(function, args, span, 0)
        });
        Ok(Value::Task(task))
    }

    fn template(&self) -> InterpreterTemplate {
        InterpreterTemplate {
            functions: self.functions.clone(),
            structs: self.structs.clone(),
            enums: self.enums.clone(),
            base_dir: self.base_dir.clone(),
            std_modules: self.std_modules.clone(),
        }
    }

    fn from_template(template: InterpreterTemplate, runtime: TaskRuntime) -> Self {
        Self {
            functions: template.functions,
            structs: template.structs,
            enums: template.enums,
            base_dir: template.base_dir,
            steps: 0,
            call_depth: 0,
            pending_fail: None,
            std_modules: template.std_modules,
            execution_deadline: None,
            task_runtime: Some(runtime),
            async_execution: true,
        }
    }

    fn call_function_value(&mut self, call: FunctionValueCall<'_>) -> KuResult<Value> {
        if call.is_async {
            let runtime = self.task_runtime.clone().ok_or_else(|| {
                KuError::runtime("async task runtime is not initialized", call.span)
            })?;
            let template = self.template();
            let child_runtime = runtime.clone();
            let params = call.params.to_vec();
            let body = call.body.to_vec();
            let captures = call.captures.clone();
            let self_name = call.self_name.clone();
            let args = call.args;
            let span = call.span;
            let task = runtime.spawn(move || {
                let mut interpreter = Interpreter::from_template(template, child_runtime);
                interpreter.call_function_value_direct(
                    FunctionValueCall {
                        params: &params,
                        body: &body,
                        captures: &captures,
                        self_name: &self_name,
                        is_async: true,
                        args,
                        span,
                        depth: 0,
                    },
                    true,
                )
            });
            return Ok(Value::Task(task));
        }
        self.call_function_value_direct(call, false)
    }

    fn call_function_value_direct(
        &mut self,
        call: FunctionValueCall<'_>,
        declared_async: bool,
    ) -> KuResult<Value> {
        let FunctionValueCall {
            params,
            body,
            captures,
            self_name,
            is_async: _,
            args,
            span,
            depth,
        } = call;
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

            let mut env = captures.clone();
            env.push_scope();
            if let Some(name) = self_name {
                env.define(
                    name.clone(),
                    Value::Function {
                        params: params.to_vec(),
                        body: body.to_vec(),
                        captures: captures.clone(),
                        self_name: self_name.clone(),
                        is_async: declared_async,
                    },
                    false,
                    span,
                )?;
            }
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
                Flow::Break | Flow::LoopContinue => Err(KuError::runtime(
                    "loop control escaped function value",
                    span,
                )),
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
            if !matches!(flow, Flow::Continue) {
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
            Stmt::DestructureAssign {
                names,
                values,
                span,
            } => {
                if names.len() != values.len() {
                    return Err(KuError::runtime(
                        format!(
                            "destructuring assignment expects {} values but got {}",
                            names.len(),
                            values.len()
                        ),
                        *span,
                    ));
                }
                let mut evaluated = Vec::with_capacity(values.len());
                for value in values {
                    evaluated.push(self.eval(value, env, depth)?);
                    if let Some(value) = self.take_pending_fail() {
                        return Ok(Flow::Fail(value));
                    }
                }
                for (name, value) in names.iter().zip(evaluated) {
                    let Some(name) = name else {
                        continue;
                    };
                    if env.contains(name) {
                        env.assign(name, value, *span)?;
                    } else {
                        env.define(name.clone(), value, !is_constant_name(name), *span)?;
                    }
                }
                Ok(Flow::Continue)
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                let condition = self.eval(condition, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                if expect_bool_condition(condition, *span)? {
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
                loop {
                    let condition = self.eval(condition, env, depth)?;
                    if let Some(value) = self.take_pending_fail() {
                        return Ok(Flow::Fail(value));
                    }
                    if !expect_bool_condition(condition, *span)? {
                        break;
                    }
                    self.tick(*span)?;
                    match self.exec_block(body, env, depth)? {
                        Flow::Continue => {}
                        Flow::LoopContinue => continue,
                        Flow::Break => break,
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
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
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
                    let mut loop_flow = Flow::Continue;
                    for stmt in body {
                        loop_flow = self.exec_stmt(stmt, env, depth)?;
                        if !matches!(loop_flow, Flow::Continue) {
                            break;
                        }
                    }
                    env.pop_scope();
                    match loop_flow {
                        Flow::Continue | Flow::LoopContinue => {}
                        Flow::Break => return Ok(Flow::Continue),
                        Flow::Return(_) | Flow::Fail(_) => return Ok(loop_flow),
                    }
                }
                Ok(Flow::Continue)
            }
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::LoopContinue),
            Stmt::Function(function) => {
                let params = function
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>();
                let body = function.body.clone();
                let capture_names = function_capture_names(function);
                let captures = env.capture(&capture_names);
                env.define(
                    function.name.clone(),
                    Value::Function {
                        params,
                        body,
                        captures,
                        self_name: Some(function.name.clone()),
                        is_async: function.is_async,
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
                            if !matches!(flow, Flow::Continue) {
                                break;
                            }
                        }
                        env.pop_scope();
                    } else {
                        flow = Flow::Fail(value);
                    }
                }
                let finally_flow = self.exec_block(finally_body, env, depth)?;
                if !matches!(finally_flow, Flow::Continue) {
                    return Ok(finally_flow);
                }
                Ok(flow)
            }
            Stmt::Fail { value, .. } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
                Ok(Flow::Fail(normalize_error_value(value)))
            }
            Stmt::Panic { value, span } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::Fail(value));
                }
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

    fn eval_array_map(
        &mut self,
        target: &Expr,
        args: &[Expr],
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<Value> {
        if args.len() != 1 {
            return Err(KuError::runtime(
                format!("array.map expects 1 argument but got {}", args.len()),
                span,
            ));
        }
        let target = self.eval(target, env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Value::Null);
        }
        let Value::Array(items) = target else {
            return Err(KuError::runtime(
                format!(
                    "type error: map expects array but got {}",
                    target.type_name()
                ),
                span,
            ));
        };
        let mapper = self.eval(&args[0], env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Value::Null);
        }
        let Value::Function {
            params,
            body,
            captures,
            self_name,
            is_async,
        } = mapper
        else {
            return Err(KuError::runtime(
                format!(
                    "type error: array.map expects function but got {}",
                    mapper.type_name()
                ),
                args[0].span,
            ));
        };
        let mut mapped = Vec::with_capacity(items.len());
        for item in items {
            self.tick(span)?;
            mapped.push(self.call_function_value(FunctionValueCall {
                params: &params,
                body: &body,
                captures: &captures,
                self_name: &self_name,
                is_async,
                args: vec![item],
                span,
                depth: depth + 1,
            })?);
        }
        Ok(Value::Array(mapped))
    }

    fn eval_std_method_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<Option<Value>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        if let ExprKind::Variable(module) = &target.kind {
            if stdlib::metadata::is_std_module(module) && !env.contains(module) {
                return Ok(None);
            }
        }
        if let Some((enum_name, _)) = enum_variant_path(callee) {
            if self.enums.contains_key(&enum_name) {
                return Ok(None);
            }
        }
        let target_value = self.eval(target, env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Some(Value::Null));
        }
        if let Value::Task(task) = target_value {
            let value = match name.as_str() {
                "status" => {
                    expect_runtime_arg_count("task.status", args.len(), 0, span)?;
                    Value::String(task.status().to_string())
                }
                "cancel" => {
                    expect_runtime_arg_count("task.cancel", args.len(), 0, span)?;
                    Value::Bool(task.cancel())
                }
                "await_timeout" => {
                    expect_runtime_arg_count("task.await_timeout", args.len(), 1, span)?;
                    let timeout = self.eval(&args[0], env, depth)?;
                    let Value::Int(timeout) = timeout else {
                        return Err(KuError::runtime(
                            "type error: task.await_timeout expects int milliseconds",
                            args[0].span,
                        ));
                    };
                    let timeout = u64::try_from(timeout).map_err(|_| {
                        KuError::runtime(
                            "task.await_timeout milliseconds must be non-negative",
                            args[0].span,
                        )
                    })?;
                    task.await_timeout(Duration::from_millis(timeout))?
                }
                _ => {
                    return Err(KuError::runtime(
                        format!("task has no method '{name}'"),
                        span,
                    ))
                }
            };
            return Ok(Some(value));
        }
        let module = match &target_value {
            Value::String(_) => "string",
            Value::Array(_) if name != "map" => "array",
            _ => return Ok(None),
        };
        let mut values = Vec::with_capacity(args.len() + 1);
        values.push(target_value);
        for arg in args {
            values.push(self.eval(arg, env, depth)?);
            if self.pending_fail.is_some() {
                return Ok(Some(Value::Null));
            }
        }
        match module {
            "string" => stdlib::string::eval(name, &values, span),
            "array" => stdlib::array::eval(name, &values, span),
            _ => Ok(None),
        }
    }

    fn eval_dotted_builtin(
        &mut self,
        callee: &Expr,
        args: &[Value],
        span: Span,
    ) -> KuResult<Option<Value>> {
        if !self.async_execution || !is_blocking_dotted_builtin(callee) {
            return stdlib::eval_dotted_builtin(callee, args, span, &self.base_dir);
        }
        let runtime = self
            .task_runtime
            .clone()
            .ok_or_else(|| KuError::runtime("async task runtime is not initialized", span))?;
        let callee = callee.clone();
        let args = args.to_vec();
        let base_dir = self.base_dir.clone();
        let value = runtime.run_blocking(
            move || {
                stdlib::eval_dotted_builtin(&callee, &args, span, &base_dir)?.ok_or_else(|| {
                    KuError::runtime("blocking stdlib dispatch returned no value", span)
                })
            },
            span,
        )?;
        if let Some(error) = task_error_payload(&value) {
            self.pending_fail = Some(error);
            return Ok(Some(Value::Null));
        }
        Ok(Some(value))
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
            ExprKind::Await(task) => {
                let value = self.eval(task, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let Value::Task(task) = value else {
                    return Err(KuError::runtime(
                        format!("await expects task but got {}", value.type_name()),
                        expr.span,
                    ));
                };
                task.await_result()
            }
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
                    if !expect_bool_condition(left, expr.span)? {
                        return Ok(Value::Bool(false));
                    }
                    let right = self.eval(right, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    return Ok(Value::Bool(expect_bool_condition(right, expr.span)?));
                }
                if *op == BinaryOp::Or {
                    let left = self.eval(left, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    if expect_bool_condition(left, expr.span)? {
                        return Ok(Value::Bool(true));
                    }
                    let right = self.eval(right, env, depth)?;
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                    return Ok(Value::Bool(expect_bool_condition(right, expr.span)?));
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
                if let ExprKind::Field { target, name } = &callee.kind {
                    if name == "map" {
                        return self.eval_array_map(target, args, env, depth, expr.span);
                    }
                }
                if let Some(value) =
                    self.eval_std_method_call(callee, args, env, depth, expr.span)?
                {
                    return Ok(value);
                }
                if let Some(value) =
                    self.eval_http_service_method_call(callee, args, env, depth, expr.span)?
                {
                    return Ok(value);
                }
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval(arg, env, depth)?);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                let args = values;
                if !dotted_builtin_is_shadowed(callee, env) {
                    if let Some(module) = dotted_builtin_module(callee) {
                        if stdlib::metadata::module_requires_import(module)
                            && !self.std_modules.contains(module)
                        {
                            return Err(KuError::runtime(
                                format!("std module '{module}' must be imported before use"),
                                expr.span,
                            ));
                        }
                    }
                    if let Some(value) = self.eval_dotted_builtin(callee, &args, expr.span)? {
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
                        captures,
                        self_name,
                        is_async,
                    } => self.call_function_value(FunctionValueCall {
                        params: &params,
                        body: &body,
                        captures: &captures,
                        self_name: &self_name,
                        is_async,
                        args,
                        span: expr.span,
                        depth: depth + 1,
                    }),
                    other => Err(KuError::runtime(
                        format!("cannot call {}", other.type_name()),
                        callee.span,
                    )),
                }
            }
            ExprKind::Function { params, body, .. } => Ok(Value::Function {
                params: params.iter().map(|param| param.name.clone()).collect(),
                body: body.clone(),
                captures: env.capture(&closure_capture_names(params, body)),
                self_name: None,
                is_async: false,
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
                eval_index_value(target, index, expr.span, false)
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Variable(enum_name) = &target.kind {
                    if enum_name == "http"
                        && !env.contains("http")
                        && self.std_modules.contains("http")
                        && matches!(name.as_str(), "service" | "server")
                    {
                        return stdlib::http::default_server_value(expr.span);
                    }
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
            ExprKind::OptionalField { target, name } => {
                let target = self.eval(target, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match target {
                    Value::Null => Ok(Value::Null),
                    Value::Struct { fields, .. } | Value::Object(fields) => {
                        Ok(fields.get(name).cloned().unwrap_or(Value::Null))
                    }
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
                            if !expect_bool_condition(guard, arm.span)? {
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
                let (value, optional_object_index) =
                    if let ExprKind::Index { target, index } = &inner.kind {
                        let target = self.eval(target, env, depth)?;
                        if self.pending_fail.is_some() {
                            return Ok(Value::Null);
                        }
                        let index = self.eval(index, env, depth)?;
                        if self.pending_fail.is_some() {
                            return Ok(Value::Null);
                        }
                        let optional_object_index = matches!(target, Value::Object(_));
                        (
                            eval_index_value(target, index, inner.span, true)?,
                            optional_object_index,
                        )
                    } else {
                        (self.eval(inner, env, depth)?, false)
                    };
                if optional_object_index {
                    return Ok(value);
                }
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

    fn eval_http_service_method_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<Option<Value>> {
        let ExprKind::Field { target, name } = &callee.kind else {
            return Ok(None);
        };
        if !matches!(
            name.as_str(),
            "get" | "post" | "put" | "del" | "listen" | "bind" | "run" | "close"
        ) {
            return Ok(None);
        }
        if matches!(&target.kind, ExprKind::Variable(module) if module == "http") {
            return Ok(None);
        }
        let target_value = self.eval(target, env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Some(Value::Null));
        }
        if (name == "run" || name == "close") && is_http_listener_object(&target_value) {
            if !args.is_empty() {
                return Err(KuError::runtime(
                    format!(
                        "http listener {name} expects 0 arguments but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            return Ok(Some(if name == "run" {
                result_from_listener_operation(
                    self.run_http_listener(target_value, span),
                    "run_failed",
                )
            } else {
                result_from_listener_operation(
                    close_http_listener_value(target_value, span),
                    "close_failed",
                )
            }));
        }
        if !is_http_service_object(&target_value) {
            return Ok(None);
        }
        if name == "listen" || name == "bind" {
            if args.len() != 1 {
                return Err(KuError::runtime(
                    format!(
                        "http service {name} expects 1 argument but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let address = self.eval(&args[0], env, depth)?;
            if self.pending_fail.is_some() {
                return Ok(Some(Value::Null));
            }
            let Value::String(address) = address else {
                return Err(KuError::runtime(
                    format!("type error: expected str but got {}", address.type_name()),
                    args[0].span,
                ));
            };
            let compiled_router = compile_http_routes(&target_value, span)?;
            let (listener_id, bound_address) = match bind_http_address(&address) {
                Ok(bound) => bound,
                Err(message) => return Ok(Some(http_error_result("bind_failed", message))),
            };
            if name == "bind" {
                return Ok(Some(Value::Result {
                    ok: true,
                    value: Box::new(http_listener_value(
                        listener_id,
                        bound_address,
                        target_value,
                        compiled_router,
                    )),
                }));
            }
            return Ok(Some(http_error_result(
                "listen_failed",
                match self.run_http_listener(
                    http_listener_value(listener_id, bound_address, target_value, compiled_router),
                    span,
                ) {
                    Ok(()) => "http service listen returned unexpectedly".to_string(),
                    Err(err) => err.message,
                },
            )));
        }
        if args.len() != 2 {
            return Err(KuError::runtime(
                format!(
                    "http service {name} expects 2 arguments but got {}",
                    args.len()
                ),
                span,
            ));
        }
        let route_path = self.eval(&args[0], env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Some(Value::Null));
        }
        let Value::String(route_path) = route_path else {
            return Err(KuError::runtime(
                format!(
                    "type error: expected str but got {}",
                    route_path.type_name()
                ),
                args[0].span,
            ));
        };
        let handler_expr = &args[1];
        let handler = self.eval(handler_expr, env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Some(Value::Null));
        }
        if !matches!(handler, Value::Function { .. }) {
            return Err(KuError::runtime(
                format!("http service {name} handler must be a function"),
                handler_expr.span,
            ));
        }
        let mut service = target_value;
        append_http_route(&mut service, name, route_path, handler, span)?;
        let Some(root) = assignment_root(target) else {
            return Err(KuError::runtime(
                "http service route registration target must be a variable",
                target.span,
            ));
        };
        env.assign(&root, service.clone(), span)?;
        Ok(Some(service))
    }

    fn run_http_listener(&mut self, listener: Value, span: Span) -> KuResult<()> {
        let Value::Object(mut fields) = listener else {
            return Err(KuError::runtime("http listener must be an object", span));
        };
        let listener_id = match fields.remove("listener_id") {
            Some(Value::Int(id)) => id,
            _ => return Err(KuError::runtime("http listener missing listener_id", span)),
        };
        let service = fields
            .remove("service")
            .ok_or_else(|| KuError::runtime("http listener missing service", span))?;
        let compiled_router = fields
            .remove("compiled_router")
            .ok_or_else(|| KuError::runtime("http listener missing compiled_router", span))?;
        let tcp = take_http_listener(listener_id, span)?;
        let limits = HttpServerRuntimeLimits::from_service(&service);
        tcp.set_nonblocking(true).map_err(|err| {
            KuError::runtime(
                format!("http listener nonblocking setup failed: {err}"),
                span,
            )
        })?;

        let connection_count = Arc::new(AtomicUsize::new(0));
        let (connection_tx, connection_rx) =
            mpsc::sync_channel::<PendingHttpConnection>(limits.max_pending_requests);
        let shared_connection_rx = Arc::new(Mutex::new(connection_rx));
        let (handler_tx, handler_rx) =
            mpsc::sync_channel::<HttpHandlerJob>(limits.max_active_requests);
        let worker_count = Arc::new(AtomicUsize::new(0));

        loop {
            let mut made_progress = false;
            for _ in 0..HTTP_ACCEPT_BATCH {
                match tcp.accept() {
                    Ok((stream, _)) => {
                        made_progress = true;
                        Self::enqueue_http_connection(
                            stream,
                            &connection_tx,
                            &shared_connection_rx,
                            &handler_tx,
                            &connection_count,
                            &worker_count,
                            limits,
                        )?;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        return Err(KuError::runtime(
                            format!("http listener accept failed: {err}"),
                            span,
                        ));
                    }
                }
            }

            loop {
                match handler_rx.try_recv() {
                    Ok(job) => {
                        made_progress = true;
                        let response = self.execute_http_handler_job(
                            job.request,
                            &compiled_router,
                            span,
                            job.deadline,
                        );
                        let _ = job.response_tx.send(response);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        return Err(KuError::runtime(
                            "http handler worker channel disconnected",
                            span,
                        ));
                    }
                }
            }

            if !made_progress {
                thread::sleep(HTTP_EVENT_LOOP_SLEEP);
            }
        }
    }

    fn enqueue_http_connection(
        mut stream: TcpStream,
        connection_tx: &SyncSender<PendingHttpConnection>,
        connection_rx: &Arc<Mutex<Receiver<PendingHttpConnection>>>,
        handler_tx: &SyncSender<HttpHandlerJob>,
        connection_count: &Arc<AtomicUsize>,
        worker_count: &Arc<AtomicUsize>,
        limits: HttpServerRuntimeLimits,
    ) -> KuResult<()> {
        stream.set_nonblocking(false).map_err(|err| {
            KuError::runtime(
                format!("http connection blocking setup failed: {err}"),
                entry_span(),
            )
        })?;
        let Some(permit) =
            HttpConnectionPermit::try_acquire(Arc::clone(connection_count), limits.max_connections)
        else {
            reject_http_connection(&mut stream, limits);
            return Ok(());
        };
        match connection_tx.try_send(PendingHttpConnection {
            stream,
            _permit: permit,
        }) {
            Ok(()) => ensure_http_workers(
                connection_rx,
                handler_tx,
                connection_count,
                worker_count,
                limits,
            ),
            Err(TrySendError::Full(mut pending)) => {
                reject_http_connection(&mut pending.stream, limits);
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(KuError::runtime(
                "http connection worker channel disconnected",
                entry_span(),
            )),
        }
    }

    fn execute_http_handler_job(
        &mut self,
        request: ParsedHttpRequest,
        compiled_router: &Value,
        span: Span,
        deadline: Instant,
    ) -> HttpWireResponse {
        let route = match find_http_route(compiled_router, &request, span) {
            Ok(route) => route,
            Err(err) => return status_response(500, &err.message),
        };
        match route {
            RouteLookup::Found(handler, params) => {
                let req = http_request_value(&request, params);
                let res = Value::Object(HashMap::new());
                match self.call_http_handler(handler, req, res, span, deadline) {
                    Ok(value) => value_to_http_response(value).unwrap_or_else(|| {
                        status_response(500, "handler did not return HttpResponse")
                    }),
                    Err(err) if err.message == HTTP_HANDLER_TIMEOUT_MESSAGE => {
                        status_response(504, "Gateway Timeout")
                    }
                    Err(err) => status_response(500, &err.message),
                }
            }
            RouteLookup::MethodNotAllowed => status_response(405, "Method Not Allowed"),
            RouteLookup::NotFound => status_response(404, "Not Found"),
        }
    }

    fn call_http_handler(
        &mut self,
        handler: Value,
        req: Value,
        res: Value,
        span: Span,
        deadline: Instant,
    ) -> KuResult<Value> {
        let Value::Function {
            params,
            body,
            captures,
            self_name,
            is_async,
        } = handler
        else {
            return Err(KuError::runtime("http handler must be a function", span));
        };
        if is_async {
            return Err(KuError::runtime(
                "async HTTP handlers are not supported in the first async runtime",
                span,
            ));
        }
        self.steps = 0;
        let previous_deadline = self.execution_deadline.replace(deadline);
        let result = self.call_function_value(FunctionValueCall {
            params: &params,
            body: &body,
            captures: &captures,
            self_name: &self_name,
            is_async: false,
            args: vec![req, res],
            span,
            depth: 0,
        });
        self.execution_deadline = previous_deadline;
        result
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
        if current_task_cancelled() {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "task",
                "cancelled",
                "async task was cancelled",
                span,
            ));
        }
        if self
            .execution_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(KuError::runtime(HTTP_HANDLER_TIMEOUT_MESSAGE, span));
        }
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

fn eval_index_value(
    target: Value,
    index: Value,
    span: Span,
    optional_object: bool,
) -> KuResult<Value> {
    match target {
        Value::Array(values) => {
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
            Ok(values[index as usize].clone())
        }
        Value::String(text) => {
            let Value::Int(index) = index else {
                return Err(KuError::runtime(
                    format!(
                        "type error: expected int index but got {}",
                        index.type_name()
                    ),
                    span,
                ));
            };
            let Some(ch) = (index >= 0)
                .then(|| text.chars().nth(index as usize))
                .flatten()
            else {
                return Err(KuError::runtime("string index out of bounds", span));
            };
            Ok(Value::String(ch.to_string()))
        }
        Value::Object(fields) => {
            let Value::String(key) = index else {
                return Err(KuError::runtime(
                    format!(
                        "type error: expected str index but got {}",
                        index.type_name()
                    ),
                    span,
                ));
            };
            match fields.get(&key).cloned() {
                Some(value) => Ok(value),
                None if optional_object => Ok(Value::Null),
                None => Err(KuError::runtime(format!("object has no key '{key}'"), span)),
            }
        }
        other => Err(KuError::runtime(
            format!("type error: cannot index {}", other.type_name()),
            span,
        )),
    }
}

fn assign_index_value(target: &mut Value, index: Value, value: Value, span: Span) -> KuResult<()> {
    match target {
        Value::Array(values) => {
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
        }
        Value::Object(fields) => {
            let Value::String(key) = index else {
                return Err(KuError::runtime(
                    format!(
                        "type error: expected str index but got {}",
                        index.type_name()
                    ),
                    span,
                ));
            };
            fields.insert(key, value);
        }
        other => {
            return Err(KuError::runtime(
                format!("type error: cannot index {}", other.type_name()),
                span,
            ));
        }
    }
    Ok(())
}

fn normalize_error_value(value: Value) -> Value {
    if is_error_object(&value) {
        return value;
    }
    stdlib::errors::error_object("ku", "fail", value.to_string())
}

fn is_error_object(value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    matches!(fields.get("domain"), Some(Value::String(_)))
        && matches!(fields.get("code"), Some(Value::String(_)))
        && matches!(fields.get("message"), Some(Value::String(_)))
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

fn is_http_service_object(value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    matches!(
        fields.get("kind"),
        Some(Value::String(kind)) if kind == "http.service"
    ) && matches!(fields.get("routes"), Some(Value::Array(_)))
}

fn is_http_listener_object(value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    matches!(
        fields.get("kind"),
        Some(Value::String(kind)) if kind == "http.listener"
    ) && matches!(fields.get("service"), Some(service) if is_http_service_object(service))
}

fn append_http_route(
    service: &mut Value,
    method: &str,
    path: String,
    handler: Value,
    span: Span,
) -> KuResult<()> {
    let Value::Object(fields) = service else {
        return Err(KuError::runtime("http service must be an object", span));
    };
    let Some(Value::Array(routes)) = fields.get_mut("routes") else {
        return Err(KuError::runtime(
            "http service routes field must be an array",
            span,
        ));
    };
    let param_names = route_param_names(&path, span)?;
    routes.push(Value::Object(HashMap::from([
        (
            "method".to_string(),
            Value::String(method.to_ascii_uppercase()),
        ),
        ("path".to_string(), Value::String(path)),
        (
            "param_names".to_string(),
            Value::Array(param_names.into_iter().map(Value::String).collect()),
        ),
        ("handler".to_string(), handler),
    ])));
    Ok(())
}

fn http_listener_value(
    listener_id: i64,
    address: String,
    service: Value,
    compiled_router: Value,
) -> Value {
    Value::Object(HashMap::from([
        (
            "kind".to_string(),
            Value::String("http.listener".to_string()),
        ),
        ("listener_id".to_string(), Value::Int(listener_id)),
        ("address".to_string(), Value::String(address)),
        ("service".to_string(), service),
        ("compiled_router".to_string(), compiled_router),
    ]))
}

fn bind_http_address(address: &str) -> Result<(i64, String), String> {
    let socket_address = if let Some(port) = address.strip_prefix(':') {
        format!("127.0.0.1:{port}")
    } else {
        address.to_string()
    };
    let listener = TcpListener::bind(&socket_address)
        .map_err(|err| format!("http service bind({address}) failed: {err}"))?;
    let local = listener
        .local_addr()
        .map_err(|err| format!("http service bind({address}) failed: {err}"))?;
    let id = NEXT_HTTP_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
    http_listeners()
        .lock()
        .map_err(|_| "http listener registry is poisoned".to_string())?
        .insert(id, listener);
    Ok((id, local.to_string()))
}

fn take_http_listener(id: i64, span: Span) -> KuResult<TcpListener> {
    http_listeners()
        .lock()
        .map_err(|_| KuError::runtime("http listener registry is poisoned", span))?
        .remove(&id)
        .ok_or_else(|| KuError::runtime("http listener was already consumed", span))
}

fn close_http_listener_value(listener: Value, span: Span) -> KuResult<()> {
    let Value::Object(mut fields) = listener else {
        return Err(KuError::runtime("http listener must be an object", span));
    };
    let listener_id = match fields.remove("listener_id") {
        Some(Value::Int(id)) => id,
        _ => return Err(KuError::runtime("http listener missing listener_id", span)),
    };
    close_http_listener(listener_id, span)
}

fn close_http_listener(id: i64, span: Span) -> KuResult<()> {
    http_listeners()
        .lock()
        .map_err(|_| KuError::runtime("http listener registry is poisoned", span))?
        .remove(&id)
        .map(|_| ())
        .ok_or_else(|| KuError::runtime("http listener was already consumed", span))
}

fn http_listeners() -> &'static Mutex<HashMap<i64, TcpListener>> {
    HTTP_LISTENERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn compile_http_routes(service: &Value, span: Span) -> KuResult<Value> {
    let Value::Object(fields) = service else {
        return Err(KuError::runtime("http service must be an object", span));
    };
    let Some(Value::Array(routes)) = fields.get("routes") else {
        return Err(KuError::runtime(
            "http service routes field must be an array",
            span,
        ));
    };
    let mut methods: HashMap<String, Value> = HashMap::new();
    let mut seen = HashSet::new();
    for route in routes {
        let Value::Object(route_fields) = route else {
            return Err(KuError::runtime("http route must be an object", span));
        };
        let Some(Value::String(method)) = route_fields.get("method") else {
            return Err(KuError::runtime("http route method must be str", span));
        };
        let Some(Value::String(path)) = route_fields.get("path") else {
            return Err(KuError::runtime("http route path must be str", span));
        };
        let route_key = normalized_route_key(method, path, span)?;
        if !seen.insert(route_key.clone()) {
            return Err(KuError::runtime(
                format!("duplicate http route '{method} {path}'"),
                span,
            ));
        }
        let (method_key, shape) = route_key
            .split_once(' ')
            .expect("normalized route keys contain a method separator");
        let method_routes = methods
            .entry(method_key.to_string())
            .or_insert_with(|| Value::Object(HashMap::new()));
        let Value::Object(method_fields) = method_routes else {
            unreachable!("method router is always an object");
        };
        method_fields.insert(shape.to_string(), route.clone());
    }
    Ok(Value::Object(methods))
}

fn normalized_route_key(method: &str, path: &str, span: Span) -> KuResult<String> {
    if !path.starts_with('/') {
        return Err(KuError::runtime(
            "http route path must start with '/'",
            span,
        ));
    }
    if path.contains(':') {
        return Err(KuError::runtime(
            "http route params use '{name}', not ':name'",
            span,
        ));
    }
    let mut segments = Vec::new();
    for segment in path.split('/').skip(1) {
        if segment.is_empty() {
            continue;
        }
        if segment.starts_with('{') || segment.ends_with('}') {
            if !(segment.starts_with('{') && segment.ends_with('}')) {
                return Err(KuError::runtime(
                    format!("invalid http route segment '{segment}'"),
                    span,
                ));
            }
            let name = &segment[1..segment.len() - 1];
            if !is_route_param_name(name) {
                return Err(KuError::runtime(
                    format!("invalid http route param '{name}'"),
                    span,
                ));
            }
            segments.push("{}".to_string());
        } else {
            segments.push(segment.to_string());
        }
    }
    Ok(format!(
        "{} /{}",
        method.to_ascii_uppercase(),
        segments.join("/")
    ))
}

fn route_param_names(path: &str, span: Span) -> KuResult<Vec<String>> {
    normalized_route_key("GET", path, span)?;
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for segment in path.split('/').skip(1) {
        if segment.starts_with('{') && segment.ends_with('}') {
            let name = segment[1..segment.len() - 1].to_string();
            if !seen.insert(name.clone()) {
                return Err(KuError::runtime(
                    format!("duplicate http route param '{name}'"),
                    span,
                ));
            }
            names.push(name);
        }
    }
    Ok(names)
}

fn is_route_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Copy)]
struct HttpServerRuntimeLimits {
    read_header_timeout: Duration,
    read_body_timeout: Duration,
    write_timeout: Duration,
    idle_timeout: Duration,
    handler_timeout: Duration,
    max_header_bytes: usize,
    max_body_bytes: usize,
    max_connections: usize,
    max_active_requests: usize,
    max_pending_requests: usize,
}

impl HttpServerRuntimeLimits {
    fn from_service(service: &Value) -> Self {
        Self {
            read_header_timeout: duration_from_service(service, "read_header_timeout_ms"),
            read_body_timeout: duration_from_service(service, "read_body_timeout_ms"),
            write_timeout: duration_from_service(service, "write_timeout_ms"),
            idle_timeout: duration_from_service(service, "idle_timeout_ms"),
            handler_timeout: duration_from_service(service, "handler_timeout_ms"),
            max_header_bytes: service_int(service, "max_header_bytes").unwrap_or(16 * 1024),
            max_body_bytes: service_int(service, "max_body_bytes").unwrap_or(1_000_000),
            max_connections: service_int(service, "max_connections").unwrap_or(1024),
            max_active_requests: service_int(service, "max_active_requests").unwrap_or(256),
            max_pending_requests: service_int(service, "max_pending_requests").unwrap_or(1024),
        }
    }
}

struct HttpConnectionPermit {
    count: Arc<AtomicUsize>,
}

impl HttpConnectionPermit {
    fn try_acquire(count: Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .ok()
            .map(|_| Self { count })
    }
}

impl Drop for HttpConnectionPermit {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PendingHttpConnection {
    stream: TcpStream,
    _permit: HttpConnectionPermit,
}

struct HttpHandlerJob {
    request: ParsedHttpRequest,
    deadline: Instant,
    response_tx: SyncSender<HttpWireResponse>,
}

fn ensure_http_workers(
    connection_rx: &Arc<Mutex<Receiver<PendingHttpConnection>>>,
    handler_tx: &SyncSender<HttpHandlerJob>,
    connection_count: &Arc<AtomicUsize>,
    worker_count: &Arc<AtomicUsize>,
    limits: HttpServerRuntimeLimits,
) -> KuResult<()> {
    let desired = connection_count
        .load(Ordering::Acquire)
        .min(limits.max_active_requests);
    loop {
        let current = worker_count.load(Ordering::Acquire);
        if current >= desired {
            return Ok(());
        }
        if worker_count
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        let connection_rx = Arc::clone(connection_rx);
        let handler_tx = handler_tx.clone();
        if let Err(err) = thread::Builder::new()
            .name(format!("ku-http-{current}"))
            .spawn(move || http_worker_loop(connection_rx, handler_tx, limits))
        {
            worker_count.fetch_sub(1, Ordering::AcqRel);
            return Err(KuError::runtime(
                format!("http worker spawn failed: {err}"),
                entry_span(),
            ));
        }
    }
}

fn http_worker_loop(
    connection_rx: Arc<Mutex<Receiver<PendingHttpConnection>>>,
    handler_tx: SyncSender<HttpHandlerJob>,
    limits: HttpServerRuntimeLimits,
) {
    loop {
        let pending = {
            let Ok(receiver) = connection_rx.lock() else {
                return;
            };
            match receiver.recv() {
                Ok(pending) => pending,
                Err(_) => return,
            }
        };
        handle_http_connection(pending, &handler_tx, limits);
    }
}

fn reject_http_connection(stream: &mut TcpStream, limits: HttpServerRuntimeLimits) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));
    let mut buffer = [0_u8; 1024];
    let mut received = Vec::new();
    while received.len() < limits.max_header_bytes {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                received.extend_from_slice(&buffer[..read]);
                if received.windows(4).any(|window| window == b"\r\n\r\n")
                    || received.windows(2).any(|window| window == b"\n\n")
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = stream.set_write_timeout(Some(limits.write_timeout));
    let _ = write_http_response(stream, status_response(503, "Service Unavailable"));
    let _ = stream.shutdown(Shutdown::Write);
}

fn handle_http_connection(
    mut pending: PendingHttpConnection,
    handler_tx: &SyncSender<HttpHandlerJob>,
    limits: HttpServerRuntimeLimits,
) {
    let request = match read_http_request(&mut pending.stream, limits, entry_span()) {
        Ok(request) => request,
        Err(response) => {
            let _ = pending.stream.set_write_timeout(Some(limits.write_timeout));
            let _ = write_http_response(&mut pending.stream, response);
            return;
        }
    };
    let deadline = Instant::now() + limits.handler_timeout;
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    if handler_tx
        .send(HttpHandlerJob {
            request,
            deadline,
            response_tx,
        })
        .is_err()
    {
        return;
    }

    let response = match response_rx.recv_timeout(limits.handler_timeout) {
        Ok(response) => response,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = pending.stream.set_write_timeout(Some(limits.write_timeout));
            let _ =
                write_http_response(&mut pending.stream, status_response(504, "Gateway Timeout"));
            return;
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => status_response(500, "Internal Server Error"),
    };
    let _ = pending.stream.set_write_timeout(Some(limits.write_timeout));
    let _ = write_http_response(&mut pending.stream, response);
}

#[derive(Debug)]
struct ParsedHttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: String,
}

enum RouteLookup {
    Found(Value, HashMap<String, String>),
    MethodNotAllowed,
    NotFound,
}

fn read_http_request(
    stream: &mut TcpStream,
    limits: HttpServerRuntimeLimits,
    span: Span,
) -> Result<ParsedHttpRequest, HttpWireResponse> {
    let header = read_http_header(
        stream,
        limits.max_header_bytes,
        limits.idle_timeout,
        limits.read_header_timeout,
    )?;
    if header.is_empty() {
        return Err(status_response(400, "Bad Request"));
    }
    let header_text = String::from_utf8(header).map_err(|_| status_response(400, "Bad Request"))?;
    let mut lines = header_text.split("\r\n");
    let Some(first_line) = lines.next() else {
        return Err(status_response(400, "Bad Request"));
    };
    let parts = first_line.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(status_response(400, "Bad Request"));
    }
    let method = parts[0].to_ascii_uppercase();
    let target = parts[1].to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(status_response(400, "Bad Request"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let content_length = match headers.get("content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| status_response(400, "Bad Request"))?,
        None => 0,
    };
    if content_length > limits.max_body_bytes {
        return Err(status_response(413, "Payload Too Large"));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = stream.set_read_timeout(Some(limits.read_body_timeout));
        stream
            .read_exact(&mut body)
            .map_err(|_| status_response(408, "Request Timeout"))?;
    }
    let body = String::from_utf8(body).map_err(|_| status_response(400, "Bad Request"))?;
    let (path, query) =
        split_path_query(&target, span).map_err(|_| status_response(400, "Bad Request"))?;
    Ok(ParsedHttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn read_http_header(
    stream: &mut TcpStream,
    max_header_bytes: usize,
    idle_timeout: Duration,
    read_header_timeout: Duration,
) -> Result<Vec<u8>, HttpWireResponse> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    let _ = stream.set_read_timeout(Some(idle_timeout));
    loop {
        let read = stream
            .read(&mut one)
            .map_err(|_| status_response(408, "Request Timeout"))?;
        if read == 0 {
            break;
        }
        bytes.push(one[0]);
        if bytes.len() == 1 {
            let _ = stream.set_read_timeout(Some(read_header_timeout));
        }
        if bytes.len() > max_header_bytes {
            return Err(status_response(431, "Request Header Fields Too Large"));
        }
        if bytes.ends_with(b"\r\n\r\n") {
            bytes.truncate(bytes.len() - 4);
            return Ok(bytes);
        }
        if bytes.ends_with(b"\n\n") {
            bytes.truncate(bytes.len() - 2);
            return Ok(bytes);
        }
    }
    Err(status_response(400, "Bad Request"))
}

fn split_path_query(target: &str, _span: Span) -> KuResult<(String, HashMap<String, String>)> {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for part in raw_query.split('&').filter(|part| !part.is_empty()) {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        query.insert(name.to_string(), value.to_string());
    }
    Ok((path.to_string(), query))
}

fn find_http_route(
    compiled_router: &Value,
    request: &ParsedHttpRequest,
    span: Span,
) -> KuResult<RouteLookup> {
    let Value::Object(methods) = compiled_router else {
        return Err(KuError::runtime(
            "http compiled_router must be an object",
            span,
        ));
    };
    if let Some(lookup) = find_http_route_in_method(methods.get(&request.method), request, span)? {
        return Ok(lookup);
    }
    let path_exists = methods
        .iter()
        .filter(|(method, _)| *method != &request.method)
        .try_fold(false, |found, (_, routes)| {
            Ok::<_, KuError>(found || method_router_has_path(routes, &request.path, span)?)
        })?;
    Ok(if path_exists {
        RouteLookup::MethodNotAllowed
    } else {
        RouteLookup::NotFound
    })
}

fn find_http_route_in_method(
    routes: Option<&Value>,
    request: &ParsedHttpRequest,
    span: Span,
) -> KuResult<Option<RouteLookup>> {
    let Some(Value::Object(routes)) = routes else {
        return Ok(None);
    };
    let exact_shape = normalized_request_shape(&request.path);
    if let Some(route) = routes.get(&exact_shape) {
        return route_lookup_from_compiled_route(route, &request.path, span).map(Some);
    }
    for (shape, route) in routes {
        if shape == &exact_shape || !shape.contains("{}") {
            continue;
        }
        let lookup = route_lookup_from_compiled_route(route, &request.path, span)?;
        if matches!(lookup, RouteLookup::Found(_, _)) {
            return Ok(Some(lookup));
        }
    }
    Ok(None)
}

fn route_lookup_from_compiled_route(
    route: &Value,
    request_path: &str,
    span: Span,
) -> KuResult<RouteLookup> {
    let Value::Object(route_fields) = route else {
        return Err(KuError::runtime("http route must be an object", span));
    };
    let Some(Value::String(path)) = route_fields.get("path") else {
        return Err(KuError::runtime("http route path must be str", span));
    };
    let Some(params) = match_http_path(path, request_path, span)? else {
        return Ok(RouteLookup::NotFound);
    };
    let Some(handler) = route_fields.get("handler") else {
        return Err(KuError::runtime("http route missing handler", span));
    };
    Ok(RouteLookup::Found(handler.clone(), params))
}

fn method_router_has_path(routes: &Value, request_path: &str, span: Span) -> KuResult<bool> {
    let Value::Object(routes) = routes else {
        return Ok(false);
    };
    let exact_shape = normalized_request_shape(request_path);
    if routes.contains_key(&exact_shape) {
        return Ok(true);
    }
    for (shape, route) in routes {
        if shape.contains("{}")
            && matches!(
                route_lookup_from_compiled_route(route, request_path, span)?,
                RouteLookup::Found(_, _)
            )
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalized_request_shape(path: &str) -> String {
    let segments = path
        .split('/')
        .skip(1)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

fn match_http_path(
    pattern: &str,
    path: &str,
    span: Span,
) -> KuResult<Option<HashMap<String, String>>> {
    let param_names = route_param_names(pattern, span)?;
    let pattern_segments = pattern
        .split('/')
        .skip(1)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let path_segments = path
        .split('/')
        .skip(1)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if pattern_segments.len() != path_segments.len() {
        return Ok(None);
    }
    let mut params = HashMap::new();
    let mut param_index = 0usize;
    for (pattern, actual) in pattern_segments.iter().zip(path_segments.iter()) {
        if pattern.starts_with('{') && pattern.ends_with('}') {
            if let Some(name) = param_names.get(param_index) {
                params.insert(name.clone(), (*actual).to_string());
            }
            param_index += 1;
        } else if pattern != actual {
            return Ok(None);
        }
    }
    Ok(Some(params))
}

fn http_request_value(request: &ParsedHttpRequest, params: HashMap<String, String>) -> Value {
    Value::Object(HashMap::from([
        ("method".to_string(), Value::String(request.method.clone())),
        ("path".to_string(), Value::String(request.path.clone())),
        ("params".to_string(), string_map_value(params)),
        ("query".to_string(), string_map_value(request.query.clone())),
        (
            "headers".to_string(),
            string_map_value(request.headers.clone()),
        ),
        ("body".to_string(), Value::String(request.body.clone())),
    ]))
}

fn string_map_value(map: HashMap<String, String>) -> Value {
    Value::Object(
        map.into_iter()
            .map(|(name, value)| (name, Value::String(value)))
            .collect(),
    )
}

#[derive(Debug, Clone)]
struct HttpWireResponse {
    status: i64,
    headers: HashMap<String, String>,
    body: String,
}

fn value_to_http_response(value: Value) -> Option<HttpWireResponse> {
    let Value::Object(fields) = value else {
        return None;
    };
    let status = match fields.get("status") {
        Some(Value::Int(status)) => *status,
        _ => return None,
    };
    let headers = match fields.get("headers") {
        Some(Value::Object(headers)) => headers
            .iter()
            .filter_map(|(name, value)| match value {
                Value::String(value) => Some((name.clone(), value.clone())),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    let body = match fields.get("body") {
        Some(Value::String(body)) => body.clone(),
        _ => return None,
    };
    Some(HttpWireResponse {
        status,
        headers,
        body,
    })
}

fn status_response(status: i64, message: &str) -> HttpWireResponse {
    HttpWireResponse {
        status,
        headers: HashMap::from([(
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        )]),
        body: message.to_string(),
    }
}

fn write_http_response(stream: &mut TcpStream, response: HttpWireResponse) -> KuResult<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    };
    let mut headers = response.headers;
    headers
        .entry("content-length".to_string())
        .or_insert_with(|| response.body.len().to_string());
    headers
        .entry("connection".to_string())
        .or_insert_with(|| "close".to_string());
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason);
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(response.body.as_bytes()))
        .map_err(|err| {
            KuError::runtime(
                format!("http response write failed: {err}"),
                Span::default(),
            )
        })
}

fn result_from_listener_operation(result: KuResult<()>, code: &str) -> Value {
    match result {
        Ok(()) => Value::Result {
            ok: true,
            value: Box::new(Value::Null),
        },
        Err(err) => http_error_result(code, err.message),
    }
}

fn service_int(service: &Value, name: &str) -> Option<usize> {
    let Value::Object(fields) = service else {
        return None;
    };
    match fields.get(name) {
        Some(Value::Int(value)) => usize::try_from(*value).ok(),
        _ => None,
    }
}

fn duration_from_service(service: &Value, name: &str) -> Duration {
    Duration::from_millis(service_int(service, name).unwrap_or(5_000) as u64)
}

fn http_error_result(code: &str, message: impl Into<String>) -> Value {
    Value::Result {
        ok: false,
        value: Box::new(Value::Object(HashMap::from([
            ("domain".to_string(), Value::String("http".to_string())),
            ("code".to_string(), Value::String(code.to_string())),
            ("message".to_string(), Value::String(message.into())),
        ]))),
    }
}

fn match_pattern(
    pattern: &MatchPattern,
    value: &Value,
    env: &mut Env,
    span: Span,
) -> KuResult<bool> {
    let mut bindings = Vec::new();
    if !collect_match_bindings(pattern, value, &mut bindings, span)? {
        return Ok(false);
    }
    for (name, value) in bindings {
        env.define(name, value, false, span)?;
    }
    Ok(true)
}

fn collect_match_bindings(
    pattern: &MatchPattern,
    value: &Value,
    bindings: &mut Vec<(String, Value)>,
    span: Span,
) -> KuResult<bool> {
    match pattern {
        MatchPattern::Wildcard => Ok(true),
        MatchPattern::Binding(name) => {
            bindings.push((name.clone(), value.clone()));
            Ok(true)
        }
        MatchPattern::Literal(literal) => Ok(value == &value_from_literal(literal)),
        MatchPattern::EnumVariant {
            enum_name,
            variant,
            fields: patterns,
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
            if patterns.len() != fields.len() {
                return Err(KuError::runtime(
                    format!(
                        "match pattern '{enum_name}.{variant}' expects {} fields but got {}",
                        fields.len(),
                        patterns.len()
                    ),
                    span,
                ));
            }
            let snapshot = bindings.len();
            for (pattern, field) in patterns.iter().zip(fields.iter()) {
                if !collect_match_bindings(pattern, field, bindings, span)? {
                    bindings.truncate(snapshot);
                    return Ok(false);
                }
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

fn dotted_builtin_module(expr: &Expr) -> Option<&str> {
    let ExprKind::Field { target, .. } = &expr.kind else {
        return None;
    };
    let ExprKind::Variable(module) = &target.kind else {
        return None;
    };
    Some(module)
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
        | Stmt::DestructureAssign { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span }
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

fn is_blocking_dotted_builtin(expr: &Expr) -> bool {
    let ExprKind::Field { target, name } = &expr.kind else {
        return false;
    };
    let ExprKind::Variable(module) = &target.kind else {
        return false;
    };
    matches!(
        (module.as_str(), name.as_str()),
        ("fs", "read" | "try_read" | "write" | "try_write")
            | ("config", "env" | "env_file" | "yaml")
            | ("http", "get" | "post" | "request")
    )
}

fn task_error_payload(value: &Value) -> Option<Value> {
    let Value::Result { ok: false, value } = value else {
        return None;
    };
    let Value::Object(fields) = value.as_ref() else {
        return None;
    };
    matches!(
        fields.get("domain"),
        Some(Value::String(domain)) if domain == "task"
    )
    .then(|| value.as_ref().clone())
}

fn function_capture_names(function: &FnDecl) -> HashSet<String> {
    let mut bound = HashSet::new();
    bound.insert(function.name.clone());
    for param in &function.params {
        bound.insert(param.name.clone());
    }
    let mut free = HashSet::new();
    collect_free_block(&function.body, &mut bound, &mut free);
    free
}

fn closure_capture_names(params: &[FunctionParam], body: &[Stmt]) -> HashSet<String> {
    let mut bound = HashSet::new();
    for param in params {
        bound.insert(param.name.clone());
    }
    let mut free = HashSet::new();
    collect_free_block(body, &mut bound, &mut free);
    free
}

fn collect_free_block(body: &[Stmt], bound: &mut HashSet<String>, free: &mut HashSet<String>) {
    for stmt in body {
        collect_free_stmt(stmt, bound, free);
    }
}

fn collect_free_stmt(stmt: &Stmt, bound: &mut HashSet<String>, free: &mut HashSet<String>) {
    match stmt {
        Stmt::VarDecl { name, value, .. } | Stmt::Assign { name, value, .. } => {
            collect_free_expr(value, bound, free);
            bound.insert(name.clone());
        }
        Stmt::AssignTarget { target, value, .. } => {
            collect_free_assign_target(target, bound, free);
            collect_free_expr(value, bound, free);
        }
        Stmt::DestructureAssign { names, values, .. } => {
            for value in values {
                collect_free_expr(value, bound, free);
            }
            for name in names.iter().flatten() {
                bound.insert(name.clone());
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_free_expr(condition, bound, free);
            collect_free_block(then_branch, &mut bound.clone(), free);
            collect_free_block(else_branch, &mut bound.clone(), free);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_free_expr(condition, bound, free);
            collect_free_block(body, &mut bound.clone(), free);
        }
        Stmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            collect_free_expr(iterable, bound, free);
            let mut scoped = bound.clone();
            scoped.insert(name.clone());
            collect_free_block(body, &mut scoped, free);
        }
        Stmt::Function(function) => {
            bound.insert(function.name.clone());
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            ..
        } => {
            collect_free_block(body, &mut bound.clone(), free);
            let mut catch_bound = bound.clone();
            if let Some(name) = catch_name {
                catch_bound.insert(name.clone());
            }
            collect_free_block(catch_body, &mut catch_bound, free);
            collect_free_block(finally_body, &mut bound.clone(), free);
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            collect_free_expr(value, bound, free);
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_free_expr(value, bound, free);
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Expr { expr, .. } => collect_free_expr(expr, bound, free),
    }
}

fn collect_free_assign_target(
    target: &AssignTarget,
    bound: &HashSet<String>,
    free: &mut HashSet<String>,
) {
    match target {
        AssignTarget::Variable(name) => {
            if !bound.contains(name) {
                free.insert(name.clone());
            }
        }
        AssignTarget::Index { target, index } => {
            collect_free_expr(target, bound, free);
            collect_free_expr(index, bound, free);
        }
        AssignTarget::Field { target, .. } => collect_free_expr(target, bound, free),
    }
}

fn expect_bool_condition(value: Value, span: Span) -> KuResult<bool> {
    match value {
        Value::Bool(value) => Ok(value),
        other => Err(KuError::runtime(
            format!(
                "type error: condition must be bool but got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

fn expect_runtime_arg_count(
    name: &str,
    actual: usize,
    expected: usize,
    span: Span,
) -> KuResult<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(KuError::runtime(
            format!("{name} expects {expected} arguments but got {actual}"),
            span,
        ))
    }
}

fn collect_free_expr(expr: &Expr, bound: &HashSet<String>, free: &mut HashSet<String>) {
    match &expr.kind {
        ExprKind::Variable(name) => {
            if !bound.contains(name) {
                free.insert(name.clone());
            }
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_free_expr(expr, bound, free);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_free_expr(left, bound, free);
            collect_free_expr(right, bound, free);
        }
        ExprKind::Call { callee, args } => {
            collect_free_expr(callee, bound, free);
            for arg in args {
                collect_free_expr(arg, bound, free);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_free_expr(value, bound, free);
            }
        }
        ExprKind::Index { target, index } => {
            collect_free_expr(target, bound, free);
            collect_free_expr(index, bound, free);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_free_expr(target, bound, free)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_free_expr(value, bound, free);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_free_expr(value, bound, free);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_free_expr(guard, bound, free);
                }
                collect_free_expr(&arm.value, bound, free);
            }
        }
        ExprKind::Function { params, body, .. } => {
            let mut nested = bound.clone();
            for param in params {
                nested.insert(param.name.clone());
            }
            collect_free_block(body, &mut nested, free);
        }
        ExprKind::Literal(_) => {}
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
