use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::{
    ast::{
        is_pure_append_argument, AssignTarget, BinaryOp, Expr, ExprKind, FnDecl, FunctionParam,
        Item, Literal, MatchPattern, ParamMode, Program, Stmt, UnaryOp,
    },
    env::{BorrowLease, Env},
    error::{KuError, KuResult, RuntimeTermination, TerminationReason},
    lexer::Lexer,
    parser::Parser,
    runtime::{
        http_listener_registry,
        task::{
            current_cleanup_context, current_task_cancellation, set_execution_termination,
            CancellationContext, CleanupGuard, ExecutionTerminationGuard, TaskRuntime,
            TaskRuntimeSnapshot, TaskStressReport,
        },
    },
    span::{Position, Span},
    stdlib,
    value::{BorrowProjection, BorrowedValue, HttpListenerLease, Value, ValueProjection},
};

const MAX_CALL_DEPTH: usize = 512;
const HTTP_HANDLER_TIMEOUT_MESSAGE: &str = "http handler timeout";
const HTTP_HANDLER_CLEANUP_GRACE: Duration = Duration::from_secs(1);
const HTTP_ACCEPT_BATCH: usize = 64;
const HTTP_EVENT_LOOP_SLEEP: Duration = Duration::from_millis(1);
const HTTP_MAX_METHOD_BYTES: usize = 32;

// Existing nested regression modules share these parent test imports. The
// production I/O owner keeps its channels and lock imports in http_io.rs.
#[cfg(test)]
use std::sync::{
    mpsc::{self, Receiver, SyncSender},
    Mutex,
};

const HTTP_LISTENER_LEASE_FIELD: &str = "\0ku.http.listener.lease";

mod cancellation;
mod http_io;
mod netpoll;

#[cfg(test)]
use std::thread;

// The flat core handles statically closed scalar call trees. Other calling,
// waiting and cleanup paths still require migration; P0 is not yet complete.
mod execution;

#[cfg(test)]
mod task_cleanup_tests;

#[cfg(test)]
mod stack_safety_tests;

#[cfg(test)]
mod match_task_tests;

#[cfg(test)]
mod caller_owner_tests;

enum Flow {
    Continue,
    Break(Option<RuntimeTermination>),
    LoopContinue(Option<RuntimeTermination>),
    Return {
        value: Value,
        cleanup: Option<RuntimeTermination>,
    },
    Fail {
        value: Value,
        cleanup: Option<RuntimeTermination>,
    },
}

impl Flow {
    fn into_call_value(self, span: Span, kind: &str) -> KuResult<Value> {
        match self {
            Self::Continue => Ok(Value::Null),
            Self::Return { value, .. } => Ok(value),
            Self::Fail { value, .. } => Ok(Value::Result {
                ok: false,
                value: Box::new(value),
            }),
            Self::Break(_) | Self::LoopContinue(_) => Err(KuError::runtime(
                format!("loop control escaped {kind}"),
                span,
            )),
        }
    }

    fn into_call_flow(self, span: Span, kind: &str) -> KuResult<Self> {
        let cleanup = cancellation::ScopeExit::cleanup_context(&self);
        self.into_call_value(span, kind)
            .map(|value| Self::Return { value, cleanup })
    }

    fn fail(value: Value) -> Self {
        Self::Fail {
            value,
            cleanup: None,
        }
    }

    fn returned(value: Value) -> Self {
        Self::Return {
            value,
            cleanup: None,
        }
    }
}

struct HttpHandlerDeadline {
    deadline: Instant,
    timed_out: bool,
    cleanup_deadline: Option<Instant>,
}

impl HttpHandlerDeadline {
    fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            timed_out: false,
            cleanup_deadline: None,
        }
    }

    fn poll(&mut self, now: Instant) -> bool {
        self.timed_out |= now >= self.deadline;
        self.timed_out
    }
}

struct FunctionValueCall<'a> {
    params: &'a [String],
    param_modes: &'a [ParamMode],
    body: &'a [Stmt],
    captures: &'a Env,
    self_name: &'a Option<String>,
    is_async: bool,
    args: Vec<Value>,
    span: Span,
    depth: usize,
}

// Code is immutable during execution. Sharing this internal source table lets
// a continuation retain stable AST references while polling the interpreter,
// without cloning user values or copying every function body at each call.
#[derive(Clone, Default)]
struct FunctionTable(Arc<HashMap<String, FnDecl>>);

impl std::ops::Deref for FunctionTable {
    type Target = HashMap<String, FnDecl>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for FunctionTable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

pub struct Interpreter {
    functions: FunctionTable,
    structs: HashMap<String, HashSet<String>>,
    enums: HashMap<String, HashMap<String, usize>>,
    base_dir: PathBuf,
    steps: usize,
    call_depth: usize,
    pending_fail: Option<Value>,
    std_modules: HashSet<String>,
    execution_deadline: Option<HttpHandlerDeadline>,
    task_runtime: Option<TaskRuntime>,
    async_execution: bool,
    termination: Option<RuntimeTermination>,
    cleanup_timeout_recorded: bool,
    caller_owners: Vec<crate::env::OwnedBindingObserver>,
}

#[derive(Clone)]
struct InterpreterTemplate {
    functions: FunctionTable,
    structs: HashMap<String, HashSet<String>>,
    enums: HashMap<String, HashMap<String, usize>>,
    base_dir: PathBuf,
    std_modules: HashSet<String>,
}

impl Interpreter {
    pub fn new() -> Self {
        Self {
            functions: FunctionTable::default(),
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
            termination: None,
            cleanup_timeout_recorded: false,
            caller_owners: Vec::new(),
        }
    }

    pub fn with_base_dir(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            ..Self::new()
        }
    }

    pub fn run(&mut self, program: Program) -> KuResult<()> {
        let _execution = ExecutionTerminationGuard::enter();
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
                Value::Task(task) => {
                    let mut ticket = crate::runtime::task::AwaitTicket::begin(task);
                    ticket.drive_sync();
                    ticket.finish_with(|result| self.observe_termination(result))
                }
                value => Ok(value),
            }
        })();
        self.observe_termination(&result);
        let _shutdown_cleanup = result
            .as_ref()
            .err()
            .and_then(KuError::scope_cleanup)
            .map(CleanupGuard::enter);
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
        self.call_function_flow(name, args, span, depth)?
            .into_call_value(span, "function")
    }

    fn call_function_flow(
        &mut self,
        name: &str,
        mut args: Vec<Value>,
        span: Span,
        depth: usize,
    ) -> KuResult<Flow> {
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "runtime",
                "call_depth_exceeded",
                format!("maximum call depth exceeded: {MAX_CALL_DEPTH}"),
                span,
            ));
        }
        // A private immutable table snapshot keeps selection independent of
        // &mut self without recursively cloning the entry's AST first.
        let source = self.functions.clone();
        let function = source
            .get(name)
            .ok_or_else(|| KuError::runtime(format!("undefined function '{name}'"), span))?;
        if function.is_async {
            return self
                .spawn_async_function(function.clone(), args, span)
                .map(Flow::returned);
        }
        if let Some(result) =
            execution::try_execute_borrowed_string(self, function, &mut args, depth)
        {
            return result;
        }
        if let Some(result) = execution::try_execute_owned(self, function, &mut args, depth) {
            // Unsupported selection leaves the original argument Vec intact.
            // A selected owner or rejection returns directly without replay.
            return result;
        }
        if let Some(result) = execution::try_execute(self, function, &args, depth) {
            // The driver has completed the real scope/call epilogues and kept
            // their carried cleanup context in this Flow.
            return result;
        }
        self.call_function_direct_flow(function.clone(), args, span, depth)
    }

    fn call_function_direct(
        &mut self,
        function: FnDecl,
        args: Vec<Value>,
        span: Span,
        depth: usize,
    ) -> KuResult<Value> {
        self.call_function_direct_flow(function, args, span, depth)?
            .into_call_value(span, "function")
    }

    fn call_function_direct_flow(
        &mut self,
        function: FnDecl,
        args: Vec<Value>,
        _span: Span,
        depth: usize,
    ) -> KuResult<Flow> {
        self.call_depth += 1;
        let result = (|| -> KuResult<Flow> {
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

            let mut leases = Vec::new();
            let mut env = Env::new();
            for (param, value) in function.params.iter().zip(args) {
                if param.mode == ParamMode::Owned {
                    value.require_owned_root(param.span)?;
                }
                let value = if param.mode == ParamMode::View {
                    let (view, lease) = BorrowLease::temporary(value);
                    leases.extend(lease);
                    view
                } else {
                    value
                };
                env.define_parameter(param.name.clone(), value, false, param.span)?;
            }

            let result = self.exec_block_with_fallthrough(
                &function.body,
                &mut env,
                depth,
                Flow::returned(Value::Null),
            );
            self.finish_call_scope(env, leases, result, function.span)?
                .into_call_flow(function.span, "function")
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
        self.reject_cleanup_submission(span)?;
        if function
            .params
            .iter()
            .any(|param| param.mode == ParamMode::View)
        {
            return Err(KuError::runtime(
                "async functions cannot declare borrowed parameters",
                span,
            ));
        }
        for value in &args {
            value.require_owned_root(span)?;
        }
        let runtime = self
            .task_runtime
            .clone()
            .ok_or_else(|| KuError::runtime("async task runtime is not initialized", span))?;
        let child_runtime = runtime.clone();
        let task = runtime.spawn_runner_deferred(|| {
            let template = self.template();
            move || {
                let mut interpreter = Interpreter::from_template(template, child_runtime);
                interpreter.async_execution = true;
                execution::ClaimedNamedTask::new(interpreter, function, args, span)
            }
        })?;
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
            termination: None,
            cleanup_timeout_recorded: false,
            caller_owners: Vec::new(),
        }
    }

    fn call_function_value(&mut self, call: FunctionValueCall<'_>) -> KuResult<Value> {
        let span = call.span;
        self.call_function_value_flow(call)?
            .into_call_value(span, "function value")
    }

    fn call_function_value_flow(&mut self, call: FunctionValueCall<'_>) -> KuResult<Flow> {
        if call.is_async {
            let span = call.span;
            self.reject_cleanup_submission(span)?;
            if call.param_modes.contains(&ParamMode::View) {
                return Err(KuError::runtime(
                    "async functions cannot declare borrowed parameters",
                    span,
                ));
            }
            for value in &call.args {
                value.require_owned_root(span)?;
            }
            let runtime = self
                .task_runtime
                .clone()
                .ok_or_else(|| KuError::runtime("async task runtime is not initialized", span))?;
            let template = self.template();
            let child_runtime = runtime.clone();
            let prepared = execution::prepare_async_function_value(self, call);
            let task = runtime.spawn_runner_deferred(|| {
                move || {
                    let interpreter = Interpreter::from_template(template, child_runtime);
                    execution::ClaimedFunctionValueTask::new(interpreter, prepared)
                }
            })?;
            return Ok(Flow::returned(Value::Task(task)));
        }
        self.call_function_value_direct_flow(call, false)
    }

    fn call_function_value_direct(
        &mut self,
        call: FunctionValueCall<'_>,
        declared_async: bool,
    ) -> KuResult<Value> {
        let span = call.span;
        self.call_function_value_direct_flow(call, declared_async)?
            .into_call_value(span, "function value")
    }

    fn call_function_value_direct_flow(
        &mut self,
        call: FunctionValueCall<'_>,
        declared_async: bool,
    ) -> KuResult<Flow> {
        if call.depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "runtime",
                "call_depth_exceeded",
                format!("maximum call depth exceeded: {MAX_CALL_DEPTH}"),
                call.span,
            ));
        }
        if !declared_async {
            if let Some(result) = execution::try_execute_function_value(self, &call) {
                return result;
            }
        }
        let FunctionValueCall {
            params,
            param_modes,
            body,
            captures,
            self_name,
            is_async: _,
            args,
            span,
            depth,
        } = call;
        self.call_depth += 1;
        let result = (|| -> KuResult<Flow> {
            if params.len() != args.len() || params.len() != param_modes.len() {
                return Err(KuError::runtime(
                    format!(
                        "function value expects {} arguments but got {}",
                        params.len(),
                        args.len()
                    ),
                    span,
                ));
            }

            let mut leases = Vec::new();
            let mut env = captures.clone();
            env.push_scope();
            if let Some(name) = self_name {
                env.define_owned(
                    name.clone(),
                    Value::Function {
                        params: params.to_vec(),
                        param_modes: param_modes.to_vec(),
                        body: body.to_vec(),
                        captures: captures.clone(),
                        self_name: self_name.clone(),
                        is_async: declared_async,
                    },
                    false,
                    span,
                )?;
            }
            for ((param, mode), value) in params.iter().zip(param_modes).zip(args) {
                if *mode == ParamMode::Owned {
                    value.require_owned_root(span)?;
                }
                let value = if *mode == ParamMode::View {
                    let (view, lease) = BorrowLease::temporary(value);
                    leases.extend(lease);
                    view
                } else {
                    value
                };
                env.define_parameter(param.clone(), value, false, span)?;
            }

            let result = self.exec_block_with_fallthrough(
                body,
                &mut env,
                depth,
                Flow::returned(Value::Null),
            );
            self.finish_call_scope(env, leases, result, span)?
                .into_call_flow(span, "function value")
        })();
        self.call_depth -= 1;
        result
    }

    fn exec_block(&mut self, body: &[Stmt], env: &mut Env, depth: usize) -> KuResult<Flow> {
        self.exec_block_with_fallthrough(body, env, depth, Flow::Continue)
    }

    fn exec_block_with_fallthrough(
        &mut self,
        body: &[Stmt],
        env: &mut Env,
        depth: usize,
        fallthrough: Flow,
    ) -> KuResult<Flow> {
        env.push_scope();
        let result = (|| {
            for stmt in body {
                let flow = self.exec_stmt(stmt, env, depth)?;
                if !matches!(flow, Flow::Continue) {
                    return Ok(flow);
                }
            }
            Ok(fallthrough)
        })();
        self.finish_owned_scope(
            env,
            result,
            body.first().map(stmt_span).unwrap_or_default(),
            true,
        )
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
                    return Ok(Flow::fail(value));
                }
                env.define_owned(
                    name.clone(),
                    value,
                    *mutable && !is_constant_name(name),
                    *span,
                )?;
                Ok(Flow::Continue)
            }
            Stmt::Assign { name, value, span } => {
                if self.try_self_array_push(name, value, env, depth, *span)? {
                    return Ok(self.take_pending_fail().map_or(Flow::Continue, Flow::fail));
                }
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                if env.contains(name) {
                    env.assign_owned(name, value, *span)?;
                } else {
                    env.define_owned(name.clone(), value, !is_constant_name(name), *span)?;
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
                    return Ok(Flow::fail(value));
                }
                self.assign_target(target, value, env, depth, *span)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                Ok(Flow::Continue)
            }
            Stmt::CompoundAssign {
                target,
                op,
                value,
                span,
            } => {
                let right = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                self.compound_assign_target(target, *op, right, env, depth, *span)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
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
                        return Ok(Flow::fail(value));
                    }
                }
                for (name, value) in names.iter().zip(evaluated) {
                    let Some(name) = name else {
                        continue;
                    };
                    if env.contains(name) {
                        env.assign_owned(name, value, *span)?;
                    } else {
                        env.define_owned(name.clone(), value, !is_constant_name(name), *span)?;
                    }
                }
                Ok(Flow::Continue)
            }
            Stmt::ObjectDestructureAssign {
                bindings,
                rest,
                value,
                span,
            } => {
                let source = self.eval_object_destructure_source(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                self.object_destructure_assign(bindings, rest.as_ref(), source, env, depth, *span)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
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
                    return Ok(Flow::fail(value));
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
                        return Ok(Flow::fail(value));
                    }
                    if !expect_bool_condition(condition, *span)? {
                        break;
                    }
                    self.tick(*span)?;
                    match self.exec_block(body, env, depth)? {
                        Flow::Continue => {}
                        Flow::LoopContinue(_) => continue,
                        Flow::Break(_) => break,
                        flow @ (Flow::Return { .. } | Flow::Fail { .. }) => return Ok(flow),
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
                let iterable = self.eval(iterable, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                match iterable {
                    Value::Borrowed(_) => return Err(KuError::runtime("for over a borrowed value is not supported; clone to create an owned iterable", *span)),
                    Value::Array(values) => {
                        for value in values {
                            self.tick(*span)?;
                            match self.exec_for_iteration(name, value, body, env, depth, *span)? {
                                Flow::Continue | Flow::LoopContinue(_) => {}
                                Flow::Break(_) => return Ok(Flow::Continue),
                                flow @ (Flow::Return { .. } | Flow::Fail { .. }) => return Ok(flow),
                            }
                        }
                    }
                    Value::Int(limit) => {
                        if limit < 0 {
                            return Err(KuError::runtime(
                                "for int iterator expects a non-negative int",
                                *span,
                            ));
                        }
                        let mut current = 0;
                        while current < limit {
                            self.tick(*span)?;
                            match self.exec_for_iteration(
                                name,
                                Value::Int(current),
                                body,
                                env,
                                depth,
                                *span,
                            )? {
                                Flow::Continue | Flow::LoopContinue(_) => {}
                                Flow::Break(_) => return Ok(Flow::Continue),
                                flow @ (Flow::Return { .. } | Flow::Fail { .. }) => return Ok(flow),
                            }
                            current += 1;
                        }
                    }
                    other => {
                        return Err(KuError::runtime(
                            format!(
                                "type error: for expects array or int but got {}",
                                other.type_name()
                            ),
                            *span,
                        ));
                    }
                }
                Ok(Flow::Continue)
            }
            Stmt::Break { .. } => Ok(Flow::Break(None)),
            Stmt::Continue { .. } => Ok(Flow::LoopContinue(None)),
            Stmt::Function(function) => {
                let params = function
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>();
                let body = function.body.clone();
                let capture_names = function_capture_names(function)?;
                env.check_capture(&capture_names, function.span)?;
                let captures = env.capture(&capture_names);
                env.define_owned(
                    function.name.clone(),
                    Value::Function {
                        params,
                        param_modes: function.params.iter().map(|param| param.mode).collect(),
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
                let mut result = self.exec_block(body, env, depth);
                if let (Ok(Flow::Fail { value, .. }), Some(name)) = (&mut result, catch_name) {
                    let value = std::mem::replace(value, Value::Null);
                    result = {
                        env.push_scope();
                        let caught = (|| {
                            env.define_owned(name.clone(), value, false, *span)?;
                            for stmt in catch_body {
                                let flow = self.exec_stmt(stmt, env, depth)?;
                                if !matches!(flow, Flow::Continue) {
                                    return Ok(flow);
                                }
                            }
                            Ok(Flow::Continue)
                        })();
                        self.finish_owned_scope(env, caught, *span, true)
                    };
                }
                let mode = self.prepare_finally(env, &result, *span);
                if matches!(mode, cancellation::FinallyMode::Skip) {
                    return result;
                }
                let guard = match mode {
                    cancellation::FinallyMode::Protected(context) => {
                        Some(CleanupGuard::enter(context))
                    }
                    _ => None,
                };
                let finally_result = self.exec_block(finally_body, env, depth);
                let result = self.finish_finally(result, finally_result, mode, *span);
                drop(guard);
                result
            }
            Stmt::Fail { value, span } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                value.require_owned_root(*span)?;
                Ok(Flow::fail(normalize_error_value(value)))
            }
            Stmt::Panic { value, span } => {
                let value = self.eval(value, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                Err(KuError::runtime(format!("panic: {value}"), *span))
            }
            Stmt::Return { value, span } => {
                let value = match value {
                    Some(value) => self.eval(value, env, depth)?,
                    None => Value::Null,
                };
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                value.require_owned_root(*span)?;
                Ok(Flow::returned(value))
            }
            Stmt::Print { value, span } => {
                let mut leases = Vec::new();
                let value = self.eval_read_argument(value, env, depth, &mut leases)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
                }
                print!("{value}");
                std::io::stdout().flush().map_err(|err| {
                    KuError::runtime(format!("failed to flush stdout: {err}"), *span)
                })?;
                Ok(Flow::Continue)
            }
            Stmt::Expr { expr, .. } => {
                self.eval(expr, env, depth)?;
                if let Some(value) = self.take_pending_fail() {
                    return Ok(Flow::fail(value));
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
            param_modes,
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
                param_modes: &param_modes,
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

    fn eval_borrow_argument(
        &mut self,
        expr: &Expr,
        env: &mut Env,
        depth: usize,
        leases: &mut Vec<BorrowLease>,
    ) -> KuResult<Value> {
        match &expr.kind {
            ExprKind::Variable(name) if env.contains(name) => {
                self.tick(expr.span)?;
                let (value, lease) = env.borrow(name, expr.span)?;
                leases.extend(lease);
                return Ok(value);
            }
            ExprKind::Field { target, name } if !matches!(&target.kind, ExprKind::Variable(name) if !env.contains(name)) =>
            {
                self.tick(expr.span)?;
                let target = self.eval_borrow_argument(target, env, depth, leases)?;
                return field_value(&target, name, expr.span);
            }
            ExprKind::Index { target, index } => {
                self.tick(expr.span)?;
                let target = self.eval_borrow_argument(target, env, depth, leases)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                return eval_index_value(&target, index, expr.span, false);
            }
            _ => {}
        }
        let value = self.eval(expr, env, depth)?;
        let (value, lease) = BorrowLease::temporary(value);
        leases.extend(lease);
        Ok(value)
    }

    /// Builtin read helpers run synchronously and cannot retain their arguments.
    /// An owned temporary can therefore stay directly in the argument vector;
    /// only addressable sources/projections need a guarded read view.
    fn eval_read_argument(
        &mut self,
        expr: &Expr,
        env: &mut Env,
        depth: usize,
        leases: &mut Vec<BorrowLease>,
    ) -> KuResult<Value> {
        if matches!(&expr.kind, ExprKind::Variable(name) if env.contains(name))
            || matches!(expr.kind, ExprKind::Field { .. } | ExprKind::Index { .. })
        {
            return self.eval_borrow_argument(expr, env, depth, leases);
        }
        self.eval(expr, env, depth)
    }

    fn eval_call_arguments(
        &mut self,
        args: &[Expr],
        modes: &[ParamMode],
        env: &mut Env,
        depth: usize,
        leases: &mut Vec<BorrowLease>,
        values: &mut Vec<Value>,
    ) -> KuResult<()> {
        // Reject either argument order before evaluating effects. Paths are
        // deliberately conservative at the root, matching the checker contract.
        if modes.contains(&ParamMode::View) && modes.contains(&ParamMode::Owned) {
            let mut roots = HashMap::<String, u8>::new();
            for (index, arg) in args.iter().enumerate() {
                if let Some(root) = assignment_expr_root(arg) {
                    let mode = if modes.get(index) == Some(&ParamMode::View) {
                        1
                    } else {
                        2
                    };
                    let previous = roots.entry(root.clone()).or_default();
                    *previous |= mode;
                    if *previous == 3
                        && env.contains(&root)
                        && env
                            .with_value(&root, arg.span, |value| Ok(value.copy_value().is_none()))?
                    {
                        return Err(KuError::runtime(
                            "borrow conflicts with move in the same call",
                            arg.span,
                        ));
                    }
                }
            }
        }
        for (index, arg) in args.iter().enumerate() {
            let value = if modes.get(index) == Some(&ParamMode::View) {
                self.eval_borrow_argument(arg, env, depth, leases)?
            } else {
                let value = self.eval(arg, env, depth)?;
                value.require_owned_root(arg.span)?;
                value
            };
            values.push(value);
            if self.pending_fail.is_some() {
                break;
            }
        }
        Ok(())
    }

    fn call_argument_value(&mut self, result: KuResult<Flow>, span: Span) -> KuResult<Value> {
        match result? {
            // Only argument evaluation leaves Fail here. A callee's own Fail
            // was already wrapped as a returned Result at its call boundary.
            Flow::Fail { value, .. } => {
                self.pending_fail = Some(value);
                Ok(Value::Null)
            }
            flow => flow.into_call_value(span, "function"),
        }
    }

    fn eval_readonly_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<Option<Value>> {
        // These direct helpers cannot execute Ku code or acquire another
        // binding. Keep bootstrap length and character tests allocation-free.
        if let ExprKind::Variable(name) = &callee.kind {
            if name == "len" && !env.contains(name) && !self.functions.contains_key(name) {
                if let [arg] = args {
                    if let ExprKind::Variable(local) = &arg.kind {
                        if env.contains(local) {
                            self.tick(arg.span)?;
                            return env.with_value(local, arg.span, |value| {
                                value.with_read(span, |value| {
                                    stdlib::eval_builtin(name, std::slice::from_ref(value), span)
                                })
                            });
                        }
                    }
                }
            }
        }
        if let ExprKind::Field { target, name } = &callee.kind {
            if let ExprKind::Variable(local) = &target.kind {
                if env.contains(local) && args.is_empty() {
                    let result = env.with_value(local, target.span, |value| {
                        value.with_read(span, |value| Ok(readonly_length(value, name)))
                    })?;
                    if result.is_some() {
                        self.tick(target.span)?;
                        return Ok(result);
                    }
                }
                if !env.contains(local) && stdlib::metadata::is_std_module(local) {
                    if let Some(full_name) = readonly_dotted_name(local, name) {
                        if let [arg] = args {
                            if let ExprKind::Variable(argument) = &arg.kind {
                                if env.contains(argument) {
                                    if stdlib::metadata::module_requires_import(local)
                                        && !self.std_modules.contains(local)
                                    {
                                        return Err(KuError::runtime(
                                            format!(
                                                "std module '{local}' must be imported before use"
                                            ),
                                            span,
                                        ));
                                    }
                                    self.tick(arg.span)?;
                                    return env.with_value(argument, arg.span, |value| {
                                        eval_readonly_builtin(
                                            full_name,
                                            std::slice::from_ref(value),
                                            span,
                                        )
                                    });
                                }
                            }
                        }
                    }
                }
            }
            if let ExprKind::Literal(Literal::String(text)) = &target.kind {
                if matches!(name.as_str(), "contains" | "starts_with" | "ends_with") {
                    if let [arg] = args {
                        if let ExprKind::Variable(local) = &arg.kind {
                            if env.contains(local) {
                                self.tick(target.span)?;
                                self.tick(arg.span)?;
                                return env.with_value(local, arg.span, |value| {
                                    value.with_read(span, |value| {
                                        readonly_string_predicate(name, text, value, span).map(Some)
                                    })
                                });
                            }
                        }
                    }
                }
            }
        }

        let mut leases = Vec::new();
        let mut values = Vec::new();
        let name = match &callee.kind {
            ExprKind::Variable(name)
                if !env.contains(name)
                    && !self.functions.contains_key(name)
                    && stdlib::metadata::supports_borrowed_call(name) =>
            {
                name.as_str()
            }
            ExprKind::Field { target, name } => {
                let module = match &target.kind {
                    ExprKind::Variable(module)
                        if !env.contains(module) && stdlib::metadata::is_std_module(module) =>
                    {
                        Some(module)
                    }
                    _ => None,
                };
                if let Some(module) = module {
                    let Some(full_name) = readonly_dotted_name(module, name) else {
                        return Ok(None);
                    };
                    if stdlib::metadata::module_requires_import(module)
                        && !self.std_modules.contains(module)
                    {
                        return Err(KuError::runtime(
                            format!("std module '{module}' must be imported before use"),
                            span,
                        ));
                    }
                    full_name
                } else {
                    if !matches!(
                        name.as_str(),
                        "len" | "byte_len" | "is_empty" | "contains" | "starts_with" | "ends_with"
                    ) {
                        return Ok(None);
                    }
                    let value = self.eval_read_argument(target, env, depth, &mut leases)?;
                    if self.pending_fail.is_some() {
                        return Ok(Some(Value::Null));
                    }
                    let full_name = value.with_read(span, |value| {
                        Ok(match value {
                            Value::String(_) => readonly_dotted_name("string", name),
                            Value::Array(_) => readonly_dotted_name("array", name),
                            _ => None,
                        })
                    })?;
                    let Some(full_name) = full_name else {
                        return Ok(None);
                    };
                    values.push(value);
                    full_name
                }
            }
            _ => return Ok(None),
        };
        for arg in args {
            values.push(self.eval_read_argument(arg, env, depth, &mut leases)?);
            if self.pending_fail.is_some() {
                return Ok(Some(Value::Null));
            }
        }
        eval_readonly_builtin(name, &values, span)
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
        if name == "clone" {
            expect_runtime_arg_count("clone", args.len(), 0, span)?;
            if matches!(target_value, Value::Borrowed(_)) {
                return target_value.with_read(span, |value| {
                    if value_contains_task(value, span)? {
                        return Err(KuError::runtime("task values cannot be cloned", span));
                    }
                    Ok(Some(value.clone()))
                });
            }
            if value_contains_task(&target_value, span)? {
                return Err(KuError::runtime("task values cannot be cloned", span));
            }
            return Ok(Some(target_value));
        }
        if let Value::Task(task) = target_value {
            let _ = task;
            match name.as_str() {
                "status" => {
                    expect_runtime_arg_count("task.status", args.len(), 0, span)?;
                    return Err(KuError::runtime(
                        "task handles can only be awaited; status() is not part of Ku's user task API",
                        span,
                    ));
                }
                "cancel" => {
                    expect_runtime_arg_count("task.cancel", args.len(), 0, span)?;
                    return Err(KuError::runtime(
                        "task handles can only be awaited; cancel() is not part of Ku's user task API",
                        span,
                    ));
                }
                "await_timeout" => {
                    expect_runtime_arg_count("task.await_timeout", args.len(), 1, span)?;
                    return Err(KuError::runtime(
                        "task handles can only be awaited; await_timeout() is not part of Ku's user task API",
                        span,
                    ));
                }
                _ => {
                    return Err(KuError::runtime(
                        format!("task has no method '{name}'"),
                        span,
                    ))
                }
            }
        }
        let module = target_value.with_read(span, |target| {
            Ok(match target {
                // KuValue converters win over the concrete-type modules: a value read
                // from an object may carry any tag, so `.as_int()`/`.as_str()` must
                // dispatch to the kuvalue path regardless of the runtime tag.
                _ if name == "as_int" || name == "as_str" => "kuvalue",
                Value::String(_) => "string",
                Value::Array(_) if name != "map" => "array",
                Value::Object(_) if name == "get_or" => "object",
                _ => "",
            })
        })?;
        if module.is_empty() {
            return Ok(None);
        }
        target_value.require_owned_root(span)?;
        let mut values = Vec::with_capacity(args.len() + 1);
        values.push(target_value);
        for arg in args {
            let value = self.eval(arg, env, depth)?;
            value.require_owned_root(arg.span)?;
            values.push(value);
            if self.pending_fail.is_some() {
                return Ok(Some(Value::Null));
            }
        }
        match module {
            "string" => stdlib::string::eval(name, &values, span),
            "array" => stdlib::array::eval(name, &values, span),
            "object" => stdlib::object::eval(name, &values, span),
            "kuvalue" => Ok(eval_kuvalue_method(name, &values)),
            _ => Ok(None),
        }
    }

    fn eval_dotted_builtin(
        &mut self,
        callee: &Expr,
        args: &[Value],
        span: Span,
    ) -> KuResult<Option<Value>> {
        if let ExprKind::Field { target, name } = &callee.kind {
            if matches!(&target.kind, ExprKind::Variable(module) if module == "task")
                && self.std_modules.contains("task")
            {
                let runtime = self.task_runtime.clone().ok_or_else(|| {
                    KuError::runtime("async task runtime is not initialized", span)
                })?;
                return match name.as_str() {
                    "stats" => {
                        expect_runtime_arg_count("task.stats", args.len(), 0, span)?;
                        Ok(Some(task_runtime_snapshot_value(runtime.snapshot()?)))
                    }
                    "stress" => {
                        expect_runtime_arg_count("task.stress", args.len(), 3, span)?;
                        let demand = stress_usize_arg("demand", &args[0], span)?;
                        let producers = stress_usize_arg("producers", &args[1], span)?;
                        let hold_ms = stress_u64_arg("hold_ms", &args[2], span)?;
                        let report = runtime.stress_concurrent_demand(
                            demand,
                            producers,
                            Duration::from_millis(hold_ms),
                        )?;
                        Ok(Some(task_stress_report_value(report)))
                    }
                    _ => Ok(None),
                };
            }
        }
        if is_blocking_dotted_builtin(callee) {
            self.reject_cleanup_submission(span)?;
        }
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
            ExprKind::Variable(name) => self.eval_variable(name, env, expr.span),
            ExprKind::Await(task) => {
                self.reject_cleanup_submission(expr.span)?;
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
                let mut ticket = crate::runtime::task::AwaitTicket::begin(task);
                ticket.drive_sync();
                ticket.finish_with(|result| self.observe_termination(result))
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
                // Register before even the first argument is evaluated: a later
                // nested call can observe cancellation before this callee starts.
                let previous_observers = self.caller_owners.len();
                if self
                    .task_runtime
                    .as_ref()
                    .is_some_and(TaskRuntime::has_task_submissions)
                {
                    let observer = env.observe_owned_bindings(expr.span)?;
                    if !observer.is_empty() {
                        self.caller_owners.try_reserve(1).map_err(|_| {
                            KuError::runtime(
                                "caller task ownership observation out of memory",
                                expr.span,
                            )
                        })?;
                        self.caller_owners.push(observer);
                    }
                }
                // All existing early returns/? leave this closure first, so an
                // ordinary error cannot retain an observer past the expression.
                let result = (|| {
                    if let Some(value) =
                        self.eval_readonly_call(callee, args, env, depth, expr.span)?
                    {
                        return Ok(value);
                    }
                    if let ExprKind::Variable(name) = &callee.kind {
                        if let Some(function) = self.functions.get(name) {
                            let modes = explicit_borrow_modes(function);
                            let mut leases = Vec::new();
                            let mut values = Vec::with_capacity(args.len());
                            let result = (|| {
                                self.eval_call_arguments(
                                    args,
                                    &modes,
                                    env,
                                    depth,
                                    &mut leases,
                                    &mut values,
                                )?;
                                if let Some(value) = self.take_pending_fail() {
                                    return Ok(Flow::fail(value));
                                }
                                self.call_function_flow(
                                    name,
                                    std::mem::take(&mut values),
                                    expr.span,
                                    depth + 1,
                                )
                            })();
                            let result =
                                self.finish_call_arguments(env, leases, values, result, expr.span);
                            return self.call_argument_value(result, expr.span);
                        }
                    }
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
                    // Resolve a first-class callable before its arguments so the
                    // declared modes, including imported/local closures, drive reads.
                    if !matches!(&callee.kind, ExprKind::Variable(name) if !env.contains(name))
                        && (dotted_builtin_is_shadowed(callee, env)
                            || dotted_builtin_module(callee).is_none())
                    {
                        let callable = self.eval(callee, env, depth)?;
                        return callable.with_read(expr.span, |callable| {
                            let Value::Function {
                                params,
                                param_modes,
                                body,
                                captures,
                                self_name,
                                is_async,
                            } = callable
                            else {
                                return Err(KuError::runtime(
                                    format!("cannot call {}", callable.type_name()),
                                    callee.span,
                                ));
                            };
                            let mut leases = Vec::new();
                            let mut values = Vec::with_capacity(args.len());
                            let result = (|| {
                                self.eval_call_arguments(
                                    args,
                                    param_modes,
                                    env,
                                    depth,
                                    &mut leases,
                                    &mut values,
                                )?;
                                if let Some(value) = self.take_pending_fail() {
                                    return Ok(Flow::fail(value));
                                }
                                self.call_function_value_flow(FunctionValueCall {
                                    params,
                                    param_modes,
                                    body,
                                    captures,
                                    self_name,
                                    is_async: *is_async,
                                    args: std::mem::take(&mut values),
                                    span: expr.span,
                                    depth: depth + 1,
                                })
                            })();
                            let result =
                                self.finish_call_arguments(env, leases, values, result, expr.span);
                            self.call_argument_value(result, expr.span)
                        });
                    }
                    let mut values = Vec::with_capacity(args.len());
                    for arg in args {
                        let value = self.eval(arg, env, depth)?;
                        value.require_owned_root(arg.span)?;
                        values.push(value);
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
                            param_modes,
                            body,
                            captures,
                            self_name,
                            is_async,
                        } => self.call_function_value(FunctionValueCall {
                            params: &params,
                            param_modes: &param_modes,
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
                })();
                self.caller_owners.truncate(previous_observers);
                result
            }
            ExprKind::Function { params, body, .. } => {
                let names = closure_capture_names(params, body)?;
                env.check_capture(&names, expr.span)?;
                Ok(Value::Function {
                    params: params.iter().map(|param| param.name.clone()).collect(),
                    param_modes: params.iter().map(|param| param.mode).collect(),
                    body: body.clone(),
                    captures: env.capture(&names),
                    self_name: None,
                    is_async: false,
                })
            }
            ExprKind::Array(values) => {
                let mut result = Vec::with_capacity(values.len());
                for value in values {
                    let evaluated = self.eval(value, env, depth)?;
                    evaluated.require_owned_root(value.span)?;
                    result.push(evaluated);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                Ok(Value::Array(result))
            }
            ExprKind::Index { .. } => self.eval_index_expr(expr, env, depth, false),
            ExprKind::Field { target, name } => {
                if let Some(value) = self.take_task_field(expr, env)? {
                    return Ok(value);
                }
                if let ExprKind::Variable(enum_name) = &target.kind {
                    if enum_name == "http"
                        && !env.contains("http")
                        && self.std_modules.contains("http")
                    {
                        match name.as_str() {
                            "service" | "server" => {
                                return Err(KuError::runtime(
                                    format!(
                                        "std module member 'http.{name}' is a function; call it as 'http.{name}()'"
                                    ),
                                    expr.span,
                                ));
                            }
                            "status" => return Ok(stdlib::http::status_object_value()),
                            "code" => return Ok(stdlib::http::code_object_value()),
                            _ => {}
                        }
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
                    if env.contains(enum_name) {
                        self.tick(target.span)?;
                        return env.with_value(enum_name, target.span, |value| {
                            field_value(value, name, expr.span)
                        });
                    }
                }
                let target = self.eval(target, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                field_value(&target, name, expr.span)
            }
            ExprKind::OptionalField { target, name } => {
                let target = self.eval(target, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match target {
                    Value::Borrowed(view) => {
                        let present = view.with_read(expr.span, |value| match value {
                            Value::Null => Ok(false),
                            Value::Struct { fields, .. } | Value::Object(fields) => {
                                Ok(fields.contains_key(name))
                            }
                            value => Err(KuError::runtime(
                                format!("type error: {} has no fields", value.type_name()),
                                expr.span,
                            )),
                        })?;
                        if present {
                            view.project(BorrowProjection::Field(name.clone()), expr.span)
                        } else {
                            Ok(Value::Null)
                        }
                    }
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
                    let evaluated = self.eval(value, env, depth)?;
                    evaluated.require_owned_root(value.span)?;
                    values.insert(field.clone(), evaluated);
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
                    let evaluated = self.eval(value, env, depth)?;
                    evaluated.require_owned_root(value.span)?;
                    values.insert(field.clone(), evaluated);
                    if self.pending_fail.is_some() {
                        return Ok(Value::Null);
                    }
                }
                Ok(Value::Object(values))
            }
            ExprKind::Match { value, arms } => {
                let mut value = self.eval(value, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let result = (|| {
                    for arm in arms {
                        env.push_scope();
                        let result = (|| {
                            let Some(task_bindings) =
                                match_pattern(&arm.pattern, &value, env, arm.span)?
                            else {
                                return Ok(Flow::Continue);
                            };
                            if let Some(guard) = &arm.guard {
                                let guard = self.eval(guard, env, depth)?;
                                if self.pending_fail.is_some() {
                                    return Ok(Flow::returned(Value::Null));
                                }
                                if !expect_bool_condition(guard, arm.span)? {
                                    return Ok(Flow::Continue);
                                }
                            }
                            for (name, path) in task_bindings {
                                let selected = value.take_task_projection(&path, arm.span)?
                                    .ok_or_else(|| KuError::runtime("tentative Task match payload was lost before arm selection", arm.span))?;
                                env.commit_task_match_probe(&name, selected, arm.span)?;
                            }
                            self.eval(&arm.value, env, depth).map(Flow::returned)
                        })();
                        let result = self.finish_owned_scope(env, result, arm.span, true);
                        let flow = result?;
                        if !matches!(flow, Flow::Continue) {
                            return Ok(flow);
                        }
                    }
                    Err(KuError::runtime(
                        "match expression did not match any arm",
                        expr.span,
                    ))
                })();
                // Selected Task payloads have moved out of `value`. Wildcard
                // and unselected payloads still belong to this expression and
                // must finish bounded cleanup before its result can escape.
                let result = if value.contains_owned_task(expr.span)? {
                    let mut residual = Env::new();
                    residual.define_owned("\0match.scrutinee".into(), value, false, expr.span)?;
                    self.finish_owned_scope(&mut residual, result, expr.span, false)
                } else {
                    result
                };
                match result? {
                    Flow::Return { value, .. } => Ok(value),
                    _ => Err(KuError::runtime(
                        "match expression lost its selected value",
                        expr.span,
                    )),
                }
            }
            ExprKind::TryUnwrap { expr: inner } => {
                let value = if matches!(&inner.kind, ExprKind::Index { .. }) {
                    // Object indexing under `?` yields Result(ok / err missing_key);
                    // `?` then unwraps or propagates it like any other Result.
                    self.eval_index_expr(inner, env, depth, true)?
                } else {
                    self.eval(inner, env, depth)?
                };
                // A nested `?` inside `inner` (e.g. `arr[i]?.as_int()?`) may have
                // already failed and set pending_fail, returning a placeholder
                // Null. Propagate that failure instead of matching Null as a
                // Result.
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                match value {
                    Value::Borrowed(_) => Err(KuError::runtime(
                        "cannot unwrap a borrowed Result; clone to create an owned value",
                        expr.span,
                    )),
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

    fn eval_index_expr(
        &mut self,
        expr: &Expr,
        env: &mut Env,
        depth: usize,
        optional_object: bool,
    ) -> KuResult<Value> {
        let ExprKind::Index { target, index } = &expr.kind else {
            unreachable!("index evaluator only accepts index expressions")
        };
        if let ExprKind::Variable(name) = &target.kind {
            // No other task/capture may change the target and the index cannot
            // run user code. Otherwise preserve the old receiver snapshot taken
            // before evaluating the index (e.g. xs[change_xs()]).
            if env.is_unshared(name) && is_pure_append_argument(index, "") {
                self.tick(target.span)?;
                env.with_value(name, target.span, |_| Ok(()))?;
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                return env.with_value(name, target.span, |target| {
                    eval_index_value(target, index, expr.span, optional_object)
                });
            }
        }
        let target = self.eval(target, env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Value::Null);
        }
        let index = self.eval(index, env, depth)?;
        if self.pending_fail.is_some() {
            return Ok(Value::Null);
        }
        eval_index_value(&target, index, expr.span, optional_object)
    }

    fn try_self_array_push(
        &mut self,
        name: &str,
        value: &Expr,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<bool> {
        let ExprKind::Call { callee, args } = &value.kind else {
            return Ok(false);
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &callee.kind
        else {
            return Ok(false);
        };
        if method != "push"
            || !matches!(&target.kind, ExprKind::Variable(receiver) if receiver == name)
            || args.len() != 1
            || !is_pure_append_argument(&args[0], name)
            || self.enums.contains_key(name)
            || !env.with_value(name, target.span, |value| {
                Ok(matches!(value, Value::Array(_)))
            })?
        {
            return Ok(false);
        }
        // Account for the same call/receiver evaluation steps as the ordinary
        // method path, without cloning the receiver. Never hold its lock while
        // evaluating even the restricted argument expression.
        self.tick(value.span)?;
        self.tick(target.span)?;
        let piece = self.eval(&args[0], env, depth)?;
        if self.pending_fail.is_none() {
            env.append_array(name, piece, span)?;
        }
        Ok(true)
    }

    fn take_pending_fail(&mut self) -> Option<Value> {
        self.pending_fail.take()
    }

    fn eval_object_destructure_source(
        &mut self,
        value: &Expr,
        env: &mut Env,
        depth: usize,
    ) -> KuResult<Value> {
        if let ExprKind::Variable(module) = &value.kind {
            if stdlib::metadata::is_std_module(module)
                && !env.contains(module)
                && self.std_modules.contains(module)
            {
                return std_module_object_value(module, value.span);
            }
        }
        self.eval(value, env, depth)
    }

    fn object_destructure_assign(
        &mut self,
        bindings: &[crate::ast::ObjectDestructureBinding],
        rest: Option<&crate::ast::ObjectDestructureRest>,
        source: Value,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        source.require_owned_root(span)?;
        let Value::Object(mut fields) = source else {
            return Err(KuError::runtime(
                format!(
                    "type error: object destructuring expects object but got {}",
                    source.type_name()
                ),
                span,
            ));
        };
        for binding in bindings {
            let value = match fields.remove(&binding.field) {
                Some(value) => value,
                None => match &binding.default {
                    Some(default) => self.eval(default, env, depth)?,
                    None => {
                        return Err(KuError::runtime(
                            format!("object has no key '{}'", binding.field),
                            binding.span,
                        ))
                    }
                },
            };
            if self.pending_fail.is_some() {
                return Ok(());
            }
            if let Some(local) = &binding.local {
                if env.contains(local) {
                    env.assign_owned(local, value, binding.span)?;
                } else {
                    env.define_owned(local.clone(), value, !is_constant_name(local), binding.span)?;
                }
            }
        }
        if let Some((rest, local)) =
            rest.and_then(|rest| rest.local.as_ref().map(|name| (rest, name)))
        {
            let value = Value::Object(fields);
            if env.contains(local) {
                env.assign_owned(local, value, rest.span)?;
            } else {
                env.define_owned(local.clone(), value, !is_constant_name(local), rest.span)?;
            }
        }
        Ok(())
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

    fn exec_for_iteration(
        &mut self,
        name: &str,
        value: Value,
        body: &[Stmt],
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<Flow> {
        env.push_scope();
        let result = (|| -> KuResult<Flow> {
            env.define_owned(name.to_string(), value, true, span)?;
            let mut loop_flow = Flow::Continue;
            for stmt in body {
                loop_flow = self.exec_stmt(stmt, env, depth)?;
                if !matches!(loop_flow, Flow::Continue) {
                    break;
                }
            }
            Ok(loop_flow)
        })();
        self.finish_owned_scope(env, result, span, true)
    }

    fn assign_target(
        &mut self,
        target: &AssignTarget,
        value: Value,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        value.require_owned_root(span)?;
        match target {
            AssignTarget::Variable(name) => env.assign_owned(name, value, span),
            AssignTarget::Index { .. } | AssignTarget::Field { .. } => {
                let root = assignment_target_root(target).ok_or_else(|| {
                    KuError::runtime("assignment target must start with a variable", span)
                })?;
                let mut root_value = env.get(&root, span)?;
                if matches!(root_value, Value::Borrowed(_)) {
                    return Err(KuError::runtime(
                        "cannot modify through borrowed parameter",
                        span,
                    ));
                }
                self.assign_into_target(&mut root_value, target, value, env, depth, span)?;
                if self.pending_fail.is_some() {
                    return Ok(());
                }
                env.assign_owned(&root, root_value, span)
            }
        }
    }

    fn assign_into_target(
        &mut self,
        root_value: &mut Value,
        target: &AssignTarget,
        value: Value,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        match target {
            AssignTarget::Variable(_) => {
                *root_value = value;
                Ok(())
            }
            AssignTarget::Index { target, index } => {
                let container = self.assign_expr_value_mut(root_value, target, env, depth)?;
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(());
                }
                assign_index_value(container, index, value, span)
            }
            AssignTarget::Field { target, name } => {
                let container = self.assign_expr_value_mut(root_value, target, env, depth)?;
                assign_field_value(container, name, value, span)
            }
        }
    }

    fn assign_expr_value_mut<'a>(
        &mut self,
        current: &'a mut Value,
        expr: &Expr,
        env: &mut Env,
        depth: usize,
    ) -> KuResult<&'a mut Value> {
        match &expr.kind {
            ExprKind::Variable(_) => Ok(current),
            ExprKind::Index { target, index } => {
                let target = self.assign_expr_value_mut(current, target, env, depth)?;
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(target);
                }
                index_value_mut(target, index, expr.span)
            }
            ExprKind::Field { target, name } => {
                let target = self.assign_expr_value_mut(current, target, env, depth)?;
                field_value_mut(target, name, expr.span)
            }
            _ => Err(KuError::runtime(
                "assignment target must start with a variable",
                expr.span,
            )),
        }
    }

    fn compound_assign_target(
        &mut self,
        target: &AssignTarget,
        op: BinaryOp,
        right: Value,
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        match target {
            AssignTarget::Variable(name) => {
                if op == BinaryOp::Add {
                    if let Value::String(text) = &right {
                        if env.append_string(name, text, span)? {
                            return Ok(());
                        }
                    }
                }
                let left = env.get(name, span)?;
                let value = eval_binary(op, left, right, span)?;
                env.assign_owned(name, value, span)
            }
            AssignTarget::Index { .. } | AssignTarget::Field { .. } => {
                let root = assignment_target_root(target).ok_or_else(|| {
                    KuError::runtime("assignment target must start with a variable", span)
                })?;
                let mut root_value = env.get(&root, span)?;
                self.compound_assign_into_target(
                    &mut root_value,
                    target,
                    (op, right),
                    env,
                    depth,
                    span,
                )?;
                if self.pending_fail.is_some() {
                    return Ok(());
                }
                env.assign_owned(&root, root_value, span)
            }
        }
    }

    fn compound_assign_into_target(
        &mut self,
        root_value: &mut Value,
        target: &AssignTarget,
        operation: (BinaryOp, Value),
        env: &mut Env,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        let (op, right) = operation;
        match target {
            AssignTarget::Variable(_) => {
                let left = root_value.clone();
                *root_value = eval_binary(op, left, right, span)?;
                Ok(())
            }
            AssignTarget::Index { target, index } => {
                let container = self.assign_expr_value_mut(root_value, target, env, depth)?;
                let index = self.eval(index, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(());
                }
                let left = eval_index_value(container, index.clone(), span, false)?;
                let value = eval_binary(op, left, right, span)?;
                assign_index_value(container, index, value, span)
            }
            AssignTarget::Field { target, name } => {
                let container = self.assign_expr_value_mut(root_value, target, env, depth)?;
                let left = field_value(container, name, span)?;
                let value = eval_binary(op, left, right, span)?;
                assign_field_value(container, name, value, span)
            }
        }
    }

    fn eval_variable(&self, name: &str, env: &Env, span: Span) -> KuResult<Value> {
        if env.contains(name) {
            return env.get_owning(name, span);
        }
        if let Some(function) = self.functions.get(name) {
            return Ok(Value::Function {
                params: function
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect(),
                param_modes: function.params.iter().map(|param| param.mode).collect(),
                body: function.body.clone(),
                captures: Env::new(),
                self_name: Some(function.name.clone()),
                is_async: function.is_async,
            });
        }
        Err(KuError::runtime(
            format!("undefined variable '{name}'"),
            span,
        ))
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
            if name == "run" {
                self.reject_cleanup_submission(span)?;
            }
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
                )?
            } else {
                result_from_listener_operation(
                    close_http_listener_value(target_value, span),
                    "close_failed",
                )?
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
            self.reject_cleanup_submission(span)?;
            let compiled_router = compile_http_routes(&target_value, span)?;
            // Validate field assignments before opening a socket. Otherwise an
            // invalid post-construction limit would fail inside listener.run and
            // leave the just-bound listener stranded in the registry.
            HttpServerRuntimeLimits::from_service(&target_value, span)?;
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
                    Err(err) if err.runtime_termination().is_some() => return Err(err),
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
        env.assign_owned(&root, service.clone(), span)?;
        Ok(Some(Value::Null))
    }

    fn run_http_listener(&mut self, listener: Value, span: Span) -> KuResult<()> {
        self.reject_cleanup_submission(span)?;
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
        let limits = HttpServerRuntimeLimits::from_service(&service, span)?;
        let tcp = take_http_listener(listener_id, span)?;
        let mut io = http_io::HttpIo::start(tcp, limits).map_err(|err| {
            KuError::runtime(format!("http I/O reactor setup failed: {err}"), span)
        })?;
        loop {
            self.poll_termination(span)?;
            // Preserve the original termination observation interval. Socket
            // readiness and every connection deadline progress independently on
            // the single I/O thread, including while this handler is running.
            let Some(job) = io
                .next_job(HTTP_EVENT_LOOP_SLEEP)
                .map_err(|err| KuError::runtime(format!("http I/O reactor failed: {err}"), span))?
            else {
                continue;
            };
            let response =
                self.execute_http_handler_job(job.request, &compiled_router, span, job.deadline);
            job.reply.respond(job.token, response).map_err(|err| {
                KuError::runtime(format!("http handler reply failed: {err}"), span)
            })?;
        }
    }

    fn execute_http_handler_job(
        &mut self,
        request: ParsedHttpRequest,
        compiled_router: &Value,
        span: Span,
        deadline: Instant,
    ) -> HttpWireResponse {
        // Queueing spends the same handler deadline. Do not prepare an expired
        // job or enter its handler/cleanup state after the original deadline.
        if Instant::now() >= deadline {
            return status_response(504, "Gateway Timeout");
        }
        let route = match find_http_route(compiled_router, &request, span) {
            Ok(route) => route,
            Err(_) => return status_response(500, "Internal Server Error"),
        };
        match route {
            RouteLookup::Found(handler, params) => {
                let req = http_request_value(request, params);
                match self.call_http_handler(handler, req, span, deadline) {
                    Ok(value) => value_to_http_response(value).unwrap_or_else(|| {
                        status_response(500, "handler did not return HttpResponse")
                    }),
                    Err(err) if err.message == HTTP_HANDLER_TIMEOUT_MESSAGE => {
                        status_response(504, "Gateway Timeout")
                    }
                    Err(_) => status_response(500, "Internal Server Error"),
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
        span: Span,
        deadline: Instant,
    ) -> KuResult<Value> {
        let _execution = ExecutionTerminationGuard::enter();
        let Value::Function {
            params,
            param_modes,
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
        let args = match params.len() {
            0 => Vec::new(),
            1 => vec![req],
            _ => return Err(KuError::runtime(
                "ordinary HTTP route handler accepts fn() or fn(req); fn(req, res) is not allowed",
                span,
            )),
        };
        self.steps = 0;
        let previous_caller_owners = std::mem::take(&mut self.caller_owners);
        let previous_termination = self.termination.take();
        let previous_cleanup_recorded =
            std::mem::replace(&mut self.cleanup_timeout_recorded, false);
        let previous_deadline = self
            .execution_deadline
            .replace(HttpHandlerDeadline::new(deadline));
        let result = self.call_function_value(FunctionValueCall {
            params: &params,
            param_modes: &param_modes,
            body: &body,
            captures: &captures,
            self_name: &self_name,
            is_async: false,
            args,
            span,
            depth: 0,
        });
        let timed_out = self.execution_deadline.as_mut().is_some_and(|state| {
            state.poll(Instant::now());
            state.timed_out
        });
        self.execution_deadline = previous_deadline;
        self.caller_owners = previous_caller_owners;
        self.termination = previous_termination;
        self.cleanup_timeout_recorded = previous_cleanup_recorded;
        if timed_out && result.is_ok() {
            Err(KuError::runtime(HTTP_HANDLER_TIMEOUT_MESSAGE, span))
        } else {
            result
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
            ExprKind::Binary { left, op, right } if *op == BinaryOp::And || *op == BinaryOp::Or => {
                // Keep template-specific arithmetic/concatenation in operands,
                // but share ordinary expressions' lazy bool-only semantics.
                let left = self.eval_template_expr(left, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                let left = expect_bool_condition(left, expr.span)?;
                if (*op == BinaryOp::And && !left) || (*op == BinaryOp::Or && left) {
                    return Ok(Value::Bool(left));
                }
                let right = self.eval_template_expr(right, env, depth)?;
                if self.pending_fail.is_some() {
                    return Ok(Value::Null);
                }
                Ok(Value::Bool(expect_bool_condition(right, expr.span)?))
            }
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
        self.poll_termination(span)?;
        self.steps = self.steps.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
use crate::registry_server::native_test_harness as finally_bounded_process;

#[cfg(test)]
mod finally_tests {
    use super::*;

    pub(super) fn body(source: &str) -> Vec<Stmt> {
        let source = format!("fn test() {{\n{source}\n}}");
        let program = Parser::new(Lexer::new(&source).tokenize().expect("lex fixture"))
            .parse_program()
            .expect("parse fixture");
        let Item::Function(function) = program.items.into_iter().next().expect("fixture function")
        else {
            panic!("expected fixture function")
        };
        function.body
    }

    pub(super) fn marker_env() -> Env {
        let mut env = Env::new();
        env.define_owned("marker".into(), Value::Int(0), true, Span::default())
            .unwrap();
        env
    }

    pub(super) fn marker(env: &Env) -> i64 {
        env.with_value("marker", Span::default(), |value| match value {
            Value::Int(value) => Ok(*value),
            _ => panic!("marker must stay int"),
        })
        .unwrap()
    }

    fn handler(source: &str, captures: &Env) -> Value {
        Value::Function {
            params: Vec::new(),
            param_modes: Vec::new(),
            body: body(source),
            captures: captures.clone(),
            self_name: None,
            is_async: false,
        }
    }

    #[test]
    fn interpreter_finally_preserves_normal_control_flow_and_owned_payloads() {
        for (source, expected) in [
            ("try { marker = 1 } finally { marker += 10 }", "continue"),
            (
                "try { return [\"saved\"] } finally { marker = 11 }",
                "return",
            ),
            ("try { fail \"saved\" } finally { marker = 11 }", "fail"),
            ("try { break } finally { marker = 11 }", "break"),
            ("try { continue } finally { marker = 11 }", "loop"),
        ] {
            let mut interpreter = Interpreter::new();
            let mut env = marker_env();
            let flow = interpreter.exec_block(&body(source), &mut env, 0).unwrap();
            let actual = match flow {
                Flow::Continue => "continue",
                Flow::Break(_) => "break",
                Flow::LoopContinue(_) => "loop",
                Flow::Return { value, .. } => {
                    assert_eq!(value, Value::Array(vec![Value::String("saved".into())]));
                    "return"
                }
                Flow::Fail { value, .. } => {
                    assert_eq!(value, normalize_error_value(Value::String("saved".into())));
                    "fail"
                }
            };
            assert_eq!(actual, expected, "{source}");
            assert_eq!(marker(&env), 11, "{source}");
            assert!(interpreter.pending_fail.is_none());
        }
    }

    #[test]
    fn interpreter_finally_preserves_catch_fields_and_finally_override() {
        let mut interpreter = Interpreter::new();
        let mut env = marker_env();
        let flow = interpreter
            .exec_block(
                &body(
                    r#"
            try { fail { domain: "sample", code: "failed", message: "saved" } }
            catch (err) {
                marker = 1
                return [err.domain, err.code, err.message]
            } finally { marker += 10 }
        "#,
                ),
                &mut env,
                0,
            )
            .unwrap();
        let Flow::Return { value, .. } = flow else {
            panic!("expected saved return")
        };
        assert_eq!(
            value,
            Value::Array(vec![
                Value::String("sample".into()),
                Value::String("failed".into()),
                Value::String("saved".into()),
            ])
        );
        assert_eq!(marker(&env), 11);
        assert!(!env.contains("err"));
        let flow = interpreter
            .exec_block(
                &body(
                    r#"
            try { return "discarded" } finally { return "replacement" }
        "#,
                ),
                &mut env,
                0,
            )
            .unwrap();
        assert!(
            matches!(flow, Flow::Return { value: Value::String(value), .. } if value == "replacement")
        );
    }

    #[test]
    fn interpreter_finally_fatal_paths_pop_block_catch_and_match_scopes() {
        for source in [
            "local = probe\npanic(\"fatal\")",
            "try { local = probe\npanic(\"fatal\") } finally { marker = 9 }",
            "try { fail \"caught\" } catch (err) { local = probe\npanic(\"fatal\") } finally { marker = 9 }",
            "local = probe\nselected = match 1 { bound if (missing == true) => 0\n_ => 1 }",
        ] {
            let mut env = marker_env();
            let probe = HttpListenerLease::new(-1);
            env.define_owned("probe".into(), Value::HttpListenerLease(probe.clone()), false, Span::default()).unwrap();
            let error = Interpreter::new().exec_block(&body(source), &mut env, 0).err().expect("fatal error");
            assert!(error.message.contains("panic:") || error.message.contains("undefined variable"));
            assert_eq!(marker(&env), 0, "fatal panic keeps its existing semantics");
            for name in ["local", "err", "selected", "bound"] {
                assert!(!env.contains(name), "{name} leaked in {source}");
            }
            assert_eq!(Arc::strong_count(&probe), 2, "block-owned references must drop on error");
        }
    }

    #[test]
    fn interpreter_finally_http_cleanup_budget_is_shared_and_never_restarted() {
        let start = Instant::now();
        let end = start + HTTP_HANDLER_CLEANUP_GRACE;
        let context = CancellationContext::with_deadline(TerminationReason::TimedOut, end);
        let outer = CleanupGuard::enter(context);
        let nested = CleanupGuard::enter(RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: end + Duration::from_secs(1),
        });
        assert_eq!(current_cleanup_context(), Some(context));
        drop(nested);
        assert_eq!(current_cleanup_context(), Some(context));
        let nested = CleanupGuard::enter(RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: end - Duration::from_millis(1),
        });
        drop(nested);
        assert_eq!(
            current_cleanup_context().unwrap().cleanup_deadline,
            end - Duration::from_millis(1)
        );
        drop(outer);
        assert!(current_cleanup_context().is_none());
    }

    #[test]
    fn interpreter_finally_http_deadline_is_inactive_outside_handlers() {
        let mut interpreter = Interpreter::new();
        interpreter.tick(Span::default()).unwrap();
        let mut state = HttpHandlerDeadline::new(Instant::now() + Duration::from_secs(10));
        assert!(!state.poll(Instant::now()));
        assert!(!state.timed_out);
    }

    #[test]
    fn interpreter_http_expired_job_stops_before_routing_and_preserves_state() {
        let mut interpreter = Interpreter::new();
        let span = Span::default();
        let expired = Instant::now() - Duration::from_millis(1);
        let request = |method: &str, path: &str| ParsedHttpRequest {
            method: method.into(),
            path: path.into(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: String::new(),
        };
        // An invalid router proves the expired path does not even inspect it.
        assert_eq!(
            interpreter
                .execute_http_handler_job(request("GET", "/"), &Value::Null, span, expired)
                .status,
            504
        );
        assert_eq!(
            interpreter
                .execute_http_handler_job(
                    request("GET", "/"),
                    &Value::Null,
                    span,
                    http_deadline(Duration::from_secs(10)),
                )
                .status,
            500
        );
        let captures = marker_env();
        let route = Value::Object(HashMap::from([
            ("path".into(), Value::String("/".into())),
            (
                "handler".into(),
                handler(
                    "try { marker = 1 } finally { marker += 10 }\nreturn {status: 200, headers: {}, body: \"ok\"}",
                    &captures,
                ),
            ),
        ]));
        let router = Value::Object(HashMap::from([(
            "GET".into(),
            Value::Object(HashMap::from([("/".into(), route)])),
        )]));
        for (method, path) in [("GET", "/"), ("GET", "/missing"), ("POST", "/")] {
            assert_eq!(
                interpreter
                    .execute_http_handler_job(request(method, path), &router, span, expired)
                    .status,
                504
            );
            assert_eq!(marker(&captures), 0);
            assert!(interpreter.execution_deadline.is_none());
            assert!(interpreter.termination.is_none());
            assert!(interpreter.pending_fail.is_none());
            assert_eq!(interpreter.call_depth, 0);
            assert!(current_cleanup_context().is_none());
        }
        for (method, path, status) in [
            ("GET", "/missing", 404),
            ("POST", "/", 405),
            ("GET", "/", 200),
        ] {
            let response = interpreter.execute_http_handler_job(
                request(method, path),
                &router,
                span,
                http_deadline(Duration::from_secs(10)),
            );
            assert_eq!(response.status, status);
            if status == 200 {
                assert_eq!(response.body, "ok");
                assert_eq!(marker(&captures), 11);
            } else {
                assert_eq!(marker(&captures), 0);
            }
            assert!(interpreter.execution_deadline.is_none());
            assert!(interpreter.termination.is_none());
            assert!(interpreter.pending_fail.is_none());
            assert_eq!(interpreter.call_depth, 0);
        }
    }

    #[test]
    fn interpreter_http_request_transfer_preserves_owned_buffers() {
        fn owned(text: &str) -> String {
            let mut value = String::with_capacity(text.len() + 64);
            value.push_str(text);
            value
        }

        fn identity(value: &String) -> (usize, usize) {
            (value.as_ptr() as usize, value.capacity())
        }

        fn assert_buffer(value: &String, expected: &str, original: (usize, usize)) {
            assert_eq!(value, expected);
            assert_eq!(
                identity(value),
                original,
                "request buffer was copied: {expected:?}"
            );
        }

        fn assert_field(
            fields: &HashMap<String, Value>,
            name: &str,
            expected: &str,
            original: (usize, usize),
        ) {
            let Some(Value::String(value)) = fields.get(name) else {
                panic!("missing request string field {name}");
            };
            assert_buffer(value, expected, original);
        }

        fn assert_map_entry(
            fields: &HashMap<String, Value>,
            name: &str,
            key: &str,
            expected: &str,
            original: ((usize, usize), (usize, usize)),
        ) {
            let Some(Value::Object(values)) = fields.get(name) else {
                panic!("missing request object field {name}");
            };
            let Some((actual_key, Value::String(value))) = values.get_key_value(key) else {
                panic!("missing request map entry {name}.{key}");
            };
            assert_buffer(actual_key, key, original.0);
            assert_buffer(value, expected, original.1);
        }

        let request = ParsedHttpRequest {
            method: owned("POST"),
            path: owned("/echo"),
            query: HashMap::from([(owned("term"), owned("中文🙂\0查询"))]),
            headers: HashMap::from([(owned("x-title"), owned("中文🙂"))]),
            body: owned("中文🙂\0正文"),
        };
        let params = HashMap::from([(owned("id"), owned("参数🙂"))]);
        let method_buffer = identity(&request.method);
        let path_buffer = identity(&request.path);
        let body_buffer = identity(&request.body);
        let (query_key, query_value) = request.query.iter().next().unwrap();
        let query_buffers = (identity(query_key), identity(query_value));
        let (header_key, header_value) = request.headers.iter().next().unwrap();
        let header_buffers = (identity(header_key), identity(header_value));
        let (param_key, param_value) = params.iter().next().unwrap();
        let param_buffers = (identity(param_key), identity(param_value));

        let value = http_request_value(request, params);
        let Value::Object(fields) = &value else {
            panic!("request must become a Ku object");
        };
        // Read through the final owner after the parsed-request owner is gone.
        assert_field(fields, "body", "中文🙂\0正文", body_buffer);
        assert_field(fields, "method", "POST", method_buffer);
        assert_field(fields, "path", "/echo", path_buffer);
        assert_map_entry(fields, "query", "term", "中文🙂\0查询", query_buffers);
        assert_map_entry(fields, "headers", "x-title", "中文🙂", header_buffers);
        assert_map_entry(fields, "params", "id", "参数🙂", param_buffers);
        drop(value);
    }

    fn http_test_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn http_test_limits() -> HttpServerRuntimeLimits {
        HttpServerRuntimeLimits {
            read_header_timeout: Duration::from_secs(1),
            read_body_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_millis(100),
            idle_timeout: Duration::from_secs(1),
            handler_timeout: Duration::from_millis(100),
            max_header_bytes: 1024,
            max_body_bytes: 1024,
            max_connections: 1,
            max_active_requests: 1,
            max_pending_requests: 1,
        }
    }

    fn http_bounded_test_child(test: &str, variable: &str) -> bool {
        if std::env::var_os(variable).is_some() {
            return true;
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(variable, "1");
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("HTTP transport test must terminate, including join failures");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    fn http_reactor_client(address: std::net::SocketAddr, request: &[u8]) -> TcpStream {
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(request).unwrap();
        client
    }

    fn http_assert_closed(client: &mut TcpStream) {
        match client.read(&mut [0; 1]) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                        | std::io::ErrorKind::BrokenPipe
                ) => {}
            other => panic!("reactor socket did not close: {other:?}"),
        }
    }

    #[test]
    fn interpreter_http_reactor_reuses_slots_and_retires_all_socket_owners() {
        if !http_bounded_test_child(
            "runtime::interpreter::finally_tests::interpreter_http_reactor_reuses_slots_and_retires_all_socket_owners",
            "KU_HTTP_REACTOR_OWNER_CHILD",
        ) {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut limits = http_test_limits();
        limits.handler_timeout = Duration::from_secs(1);
        let mut io = http_io::HttpIo::start(listener, limits).unwrap();
        let mut first = http_reactor_client(address, b"GET /a HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let first_job = io.next_job(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(first_job.request.path, "/a");
        let mut expired = Vec::new();
        first.read_to_end(&mut expired).unwrap();
        assert!(expired.starts_with(b"HTTP/1.1 504 "), "{expired:?}");

        let mut second =
            http_reactor_client(address, b"GET /b HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let second_job = io.next_job(Duration::from_secs(2)).unwrap().unwrap();
        // There is exactly one active slot. Its distinct token therefore proves
        // a new generation, rather than accidentally selecting another slot.
        assert_ne!(first_job.token, second_job.token);
        first_job
            .reply
            .respond(first_job.token, status_response(200, "stale界🙂\0"))
            .unwrap();
        // Exceed several transport write chunks, preserving every UTF-8/NUL
        // byte across the separate head and incremental body writes.
        let body = "current界🙂\0".repeat(4096);
        second_job
            .reply
            .respond(second_job.token, status_response(200, &body))
            .unwrap();
        let mut received = Vec::new();
        second.read_to_end(&mut received).unwrap();
        assert!(received.starts_with(b"HTTP/1.1 200 "), "{received:?}");
        let body_start = received
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(&received[body_start..], body.as_bytes());
        assert!(!received
            .windows(b"stale".len())
            .any(|bytes| bytes == b"stale"));
        drop(io);

        // A real unanswered handler must not prevent other activity slots from
        // reading, classifying malformed input, expiring a partial body or
        // writing their responses. No Ku handler is run by this test thread.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        limits.max_active_requests = 3;
        limits.max_pending_requests = 2;
        limits.max_connections = 4;
        limits.handler_timeout = Duration::from_secs(5);
        let mut io = http_io::HttpIo::start(listener, limits).unwrap();
        let mut held =
            http_reactor_client(address, b"GET /held HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let held_job = io.next_job(Duration::from_secs(2)).unwrap().unwrap();
        let mut malformed = http_reactor_client(address, b"GET / HTTP/1.1\n");
        let mut response = Vec::new();
        malformed.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 400 "), "{response:?}");
        let mut partial = http_reactor_client(
            address,
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\na",
        );
        response.clear();
        partial.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 408 "), "{response:?}");
        assert!(io.next_job(Duration::from_millis(1)).unwrap().is_none());
        drop(io);
        http_assert_closed(&mut held);
        held_job
            .reply
            .respond(held_job.token, status_response(200, "closed界🙂\0"))
            .unwrap();

        // No connection gives the poll no D. Dropping still has to wake and
        // join it, and release the actual bound listener address.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(http_io::HttpIo::start(listener, limits).unwrap());
        drop(TcpListener::bind(address).expect("idle reactor released its listener"));

        // Establish active + pending admission with an observed 503, then
        // shutdown with both real sockets held. Pending input is intentionally
        // unread; platforms may close it with EOF or a connection reset.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        limits.max_active_requests = 1;
        limits.max_pending_requests = 1;
        limits.max_connections = 2;
        limits.read_body_timeout = Duration::from_secs(5);
        let io = http_io::HttpIo::start(listener, limits).unwrap();
        let mut active = http_reactor_client(
            address,
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\na",
        );
        let mut pending =
            http_reactor_client(address, b"GET /pending HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut excess = http_reactor_client(address, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut response = Vec::new();
        excess.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 503 "), "{response:?}");
        drop(io);
        http_assert_closed(&mut active);
        http_assert_closed(&mut pending);
    }

    #[test]
    fn interpreter_http_netpoll_handles_batches_deadlines_and_shared_wakes() {
        if !http_bounded_test_child(
            "runtime::interpreter::finally_tests::interpreter_http_netpoll_handles_batches_deadlines_and_shared_wakes",
            "KU_HTTP_NETPOLL_CHILD",
        ) {
            return;
        }
        let (_first_peer, first) = http_test_pair();
        let (mut second_peer, mut second) = http_test_pair();
        first.set_nonblocking(true).unwrap();
        second.set_nonblocking(true).unwrap();
        let wake = netpoll::WakePair::new().unwrap();
        let wake_sender = wake.sender.clone();
        let body = "界🙂\0".as_bytes();
        second_peer.write_all(body).unwrap();
        second_peer.shutdown(Shutdown::Write).unwrap();
        let interests = [
            netpoll::Interest {
                socket: netpoll::Socket::Stream(&wake.reader),
                read: true,
                write: false,
            },
            netpoll::Interest {
                socket: netpoll::Socket::Stream(&first),
                read: true,
                write: false,
            },
            netpoll::Interest {
                socket: netpoll::Socket::Stream(&second),
                read: true,
                write: false,
            },
        ];
        let deadline = http_deadline(Duration::from_secs(2));
        let ready = netpoll::wait(&interests, Some(deadline)).unwrap();
        assert!(ready.iter().any(|event| event.index == 2 && event.readable));
        assert!(ready.iter().all(|event| event.index == 2));
        let mut bytes = Vec::new();
        let mut eof = false;
        for _ in 0..32 {
            assert!(
                Instant::now() < deadline,
                "FIN drain retained its original D"
            );
            let mut buffer = [0; 64];
            match second.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let interests = [netpoll::Interest {
                        socket: netpoll::Socket::Stream(&second),
                        read: true,
                        write: false,
                    }];
                    assert!(!netpoll::wait(&interests, Some(deadline))
                        .unwrap()
                        .is_empty());
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("FIN drain failed: {error}"),
            }
        }
        assert!(eof, "peer FIN must be observed after its data");
        assert_eq!(bytes, body);

        let idle = [netpoll::Interest {
            socket: netpoll::Socket::Stream(&first),
            read: true,
            write: false,
        }];
        let deadline = http_deadline(Duration::from_millis(20));
        assert!(netpoll::wait(&idle, Some(deadline)).unwrap().is_empty());
        assert!(Instant::now() >= deadline);
        assert!(netpoll::wait(&idle, Some(deadline)).unwrap().is_empty());

        let wake_interest = [netpoll::Interest {
            socket: netpoll::Socket::Stream(&wake.reader),
            read: true,
            write: false,
        }];
        wake_sender.notify().unwrap();
        let ready =
            netpoll::wait(&wake_interest, Some(http_deadline(Duration::from_secs(2)))).unwrap();
        assert!(ready.iter().any(|event| event.index == 0 && event.readable));
        wake.drain().unwrap();
        assert!(netpoll::wait(
            &wake_interest,
            Some(http_deadline(Duration::from_millis(20)))
        )
        .unwrap()
        .is_empty());
        wake_sender.notify().unwrap();
        assert!(
            !netpoll::wait(&wake_interest, Some(http_deadline(Duration::from_secs(2))))
                .unwrap()
                .is_empty()
        );
        wake.drain().unwrap();
        wake_sender.shutdown().unwrap();
        let ready =
            netpoll::wait(&wake_interest, Some(http_deadline(Duration::from_secs(2)))).unwrap();
        assert!(ready.iter().any(|event| event.index == 0 && event.readable));
        assert_eq!(
            wake.drain().unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );

        assert!(netpoll::wait(&[], None).unwrap().is_empty());
        let no_interest = [netpoll::Interest {
            socket: netpoll::Socket::Stream(&first),
            read: false,
            write: false,
        }];
        assert_eq!(
            netpoll::wait(&no_interest, None).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let too_many = (0..4099)
            .map(|_| netpoll::Interest {
                socket: netpoll::Socket::Stream(&first),
                read: true,
                write: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            netpoll::wait(&too_many, None).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn interpreter_http_full_handler_queue_rejects_without_waiting_for_capacity() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let _io = http_io::HttpIo::start(listener, http_test_limits()).unwrap();
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut first = TcpStream::connect(address).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        first.write_all(request).unwrap();
        // Leave the real job queued. Its socket times out independently of the
        // interpreter, releasing the sole activity/connection slot while the
        // bounded job queue still contains the expired request.
        let mut expired = Vec::new();
        first.read_to_end(&mut expired).unwrap();
        assert!(expired.starts_with(b"HTTP/1.1 504 "), "{expired:?}");
        let mut second = TcpStream::connect(address).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        second.write_all(request).unwrap();
        let mut response = Vec::new();
        second.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 503 "), "{response:?}");
    }

    #[test]
    fn interpreter_http_rejection_yields_for_fragments_and_returns_its_permit() {
        let (mut client, server) = http_test_pair();
        let count = Arc::new(AtomicUsize::new(0));
        let permit = HttpConnectionPermit::try_acquire(Arc::clone(&count), 1).unwrap();
        let mut rejection = HttpRejection::new(server, Some(permit), http_test_limits()).unwrap();
        rejection.drain_deadline = Instant::now() + Duration::from_secs(60);
        assert!(!rejection.poll());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let prefix = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r";
        client.write_all(prefix).unwrap();
        let watchdog = Instant::now() + Duration::from_secs(2);
        while rejection.seen < prefix.len() {
            assert!(!rejection.poll());
            assert!(Instant::now() < watchdog);
            thread::yield_now();
        }
        assert!(rejection.write_deadline.is_none());
        client.write_all(b"\n").unwrap();
        while !rejection.poll() {
            assert!(Instant::now() < watchdog);
            thread::yield_now();
        }
        drop(rejection);
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert_eq!(response, HttpRejection::RESPONSE);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn interpreter_http_rejection_expired_write_budget_is_not_restarted() {
        let (_client, server) = http_test_pair();
        let count = Arc::new(AtomicUsize::new(0));
        let permit = HttpConnectionPermit::try_acquire(Arc::clone(&count), 1).unwrap();
        let mut rejection = HttpRejection::new(server, Some(permit), http_test_limits()).unwrap();
        rejection.drain_deadline = Instant::now() - Duration::from_secs(1);
        let deadline = rejection.drain_deadline + rejection.write_timeout;
        assert!(rejection.poll());
        assert_eq!(rejection.write_deadline, Some(deadline));
        assert!(rejection.poll());
        assert_eq!(rejection.write_deadline, Some(deadline));
        assert_eq!(rejection.sent, 0);
        drop(rejection);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn interpreter_finally_http_timeout_cleanup_is_bounded() {
        const CHILD: &str = "KU_INTERPRETER_FINALLY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "runtime::interpreter::finally_tests::interpreter_finally_http_timeout_cleanup_is_bounded", "--nocapture"])
                .env(CHILD, "1");
            let output = finally_bounded_process::run_bounded(
                &mut command,
                Duration::from_secs(15),
                finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
            )
            .expect("infinite finally must not hang the test process");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        for (source, expected) in [
            (
                r#"
                fn helper(value: str): str { return value }
                try { while (true) {} }
                catch (err) { marker = 9 }
                finally {
                    try { text = helper("owned")\nmarker = 1 }
                    finally { marker = marker * 10 + 2 }
                }
            "#,
                12,
            ),
            (
                r#"
                try { fail "caught" } catch (err) { while (true) {} }
                finally { marker = 1 }
            "#,
                1,
            ),
            (
                r#"
                try { try { while (true) {} } finally { marker = 1 } }
                finally { marker = marker * 10 + 2 }
            "#,
                12,
            ),
            (
                r#"
                try { while (true) {} }
                finally { marker = 1\nreturn "cannot swallow timeout" }
            "#,
                1,
            ),
            (
                r#"
                try { while (true) {} }
                finally { marker = 1\nfail "cannot swallow timeout" }
            "#,
                1,
            ),
            (
                r#"
                try { try { while (true) {} } finally { marker = 1\nwhile (true) {} } }
                finally { marker = 9 }
            "#,
                1,
            ),
            // A timeout first encountered in an ordinary finally still exits
            // that block; this patch does not resume its remaining statements.
            (
                r#"
                try { marker = 1 }
                finally { marker = 2\nwhile (true) {}\nmarker = 9 }
            "#,
                2,
            ),
        ] {
            let source = source.replace("\\n", "\n");
            let captures = marker_env();
            let mut interpreter = Interpreter::new();
            let value = handler(&source, &captures);
            let error = interpreter
                .call_http_handler(
                    value,
                    Value::Null,
                    Span::default(),
                    Instant::now() + Duration::from_millis(200),
                )
                .unwrap_err();
            assert_eq!(error.message, HTTP_HANDLER_TIMEOUT_MESSAGE, "{source}");
            assert_eq!(marker(&captures), expected, "{source}");
            assert!(interpreter.execution_deadline.is_none());
            assert!(interpreter.pending_fail.is_none());
            assert_eq!(interpreter.call_depth, 0);
            let healthy = handler("return 42", &captures);
            assert_eq!(
                interpreter
                    .call_http_handler(
                        healthy,
                        Value::Null,
                        Span::default(),
                        Instant::now() + Duration::from_secs(5)
                    )
                    .unwrap(),
                Value::Int(42)
            );
        }
    }
}

fn assignment_root(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.clone()),
        _ => None,
    }
}

fn assignment_target_root(target: &AssignTarget) -> Option<String> {
    match target {
        AssignTarget::Variable(name) => Some(name.clone()),
        AssignTarget::Index { target, .. } | AssignTarget::Field { target, .. } => {
            assignment_expr_root(target)
        }
    }
}

fn assignment_expr_root(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.clone()),
        ExprKind::Index { target, .. } | ExprKind::Field { target, .. } => {
            assignment_expr_root(target)
        }
        _ => None,
    }
}

fn explicit_borrow_modes(function: &FnDecl) -> Vec<ParamMode> {
    if function
        .params
        .iter()
        .any(|param| param.mode == ParamMode::View)
    {
        function.params.iter().map(|param| param.mode).collect()
    } else {
        // Missing modes mean Owned in eval_call_arguments. Ordinary functions
        // must not allocate a parallel mode vector on every invocation.
        Vec::new()
    }
}

fn readonly_length(value: &Value, name: &str) -> Option<Value> {
    match (value, name) {
        (Value::Array(values), "len") => Some(Value::Int(values.len() as i64)),
        (Value::Array(values), "is_empty") => Some(Value::Bool(values.is_empty())),
        (Value::String(text), "len") => Some(Value::Int(text.chars().count() as i64)),
        (Value::String(text), "byte_len") => Some(Value::Int(text.len() as i64)),
        _ => None,
    }
}

fn readonly_dotted_name(module: &str, method: &str) -> Option<&'static str> {
    Some(match (module, method) {
        ("string", "len") => "string.len",
        ("string", "byte_len") => "string.byte_len",
        ("string", "contains") => "string.contains",
        ("string", "starts_with") => "string.starts_with",
        ("string", "ends_with") => "string.ends_with",
        ("array", "len") => "array.len",
        ("array", "is_empty") => "array.is_empty",
        ("json", "stringify") => "json.stringify",
        ("json", "encode") => "json.encode",
        _ => return None,
    })
}

fn readonly_string_predicate(
    method: &str,
    text: &str,
    pattern: &Value,
    span: Span,
) -> KuResult<Value> {
    let Value::String(pattern) = pattern else {
        return Err(KuError::runtime(
            "string read operation expects str arguments",
            span,
        ));
    };
    Ok(Value::Bool(match method {
        "contains" => text.contains(pattern),
        "starts_with" => text.starts_with(pattern),
        "ends_with" => text.ends_with(pattern),
        _ => unreachable!("readonly string predicate metadata"),
    }))
}

fn eval_readonly_builtin(name: &str, values: &[Value], span: Span) -> KuResult<Option<Value>> {
    let count = if matches!(
        name,
        "string.contains" | "string.starts_with" | "string.ends_with"
    ) {
        2
    } else {
        1
    };
    expect_runtime_arg_count(name, values.len(), count, span)?;
    values[0].with_read(span, |first| {
        if let Some(method) = name.strip_prefix("string.") {
            if count == 2 {
                return values[1].with_read(span, |second| {
                    let Value::String(text) = first else {
                        return Err(KuError::runtime(
                            "string read operation expects str arguments",
                            span,
                        ));
                    };
                    readonly_string_predicate(method, text, second, span).map(Some)
                });
            }
            return stdlib::string::eval(method, std::slice::from_ref(first), span);
        }
        if let Some(method) = name.strip_prefix("array.") {
            return stdlib::array::eval(method, std::slice::from_ref(first), span);
        }
        if matches!(name, "json.stringify" | "json.encode") {
            return stdlib::json::eval("stringify", std::slice::from_ref(first), span);
        }
        if name == "print" {
            print!("{first}");
            std::io::stdout()
                .flush()
                .map_err(|err| KuError::runtime(format!("failed to flush stdout: {err}"), span))?;
            return Ok(Some(Value::Null));
        }
        stdlib::eval_builtin(name, std::slice::from_ref(first), span)
    })
}

/// `KuValue.as_int()` / `.as_str()`: convert a dynamic value read from an object
/// to a concrete type, returning `T!` (Err on a tag mismatch).
fn eval_kuvalue_method(name: &str, values: &[Value]) -> Option<Value> {
    let value = values.first()?;
    match name {
        "as_int" => Some(match value {
            Value::Int(i) => stdlib::errors::ok(Value::Int(*i)),
            other => stdlib::errors::err(
                "value",
                "type_mismatch",
                format!("expected int value, got {}", other.type_name()),
            ),
        }),
        "as_str" => Some(match value {
            Value::String(s) => stdlib::errors::ok(Value::String(s.clone())),
            other => stdlib::errors::err(
                "value",
                "type_mismatch",
                format!("expected str value, got {}", other.type_name()),
            ),
        }),
        _ => None,
    }
}

fn eval_index_value(
    target: &Value,
    index: Value,
    span: Span,
    optional_object: bool,
) -> KuResult<Value> {
    if let Value::Borrowed(view) = target {
        return view.with_read(span, |target| {
            let projection = index.with_read(span, |index| match (target, index) {
                (Value::Array(values), Value::Int(index))
                    if *index >= 0 && (*index as usize) < values.len() =>
                {
                    Ok(Some(BorrowProjection::Index(*index as usize)))
                }
                (Value::Object(fields), Value::String(name)) if fields.contains_key(name) => {
                    Ok(Some(BorrowProjection::Field(name.clone())))
                }
                _ => Ok(None),
            })?;
            if let Some(projection) = projection {
                let value = view.project(projection, span)?;
                if optional_object {
                    value.require_owned_root(span)?;
                    return Ok(stdlib::errors::ok(value));
                }
                return Ok(value);
            }
            // Missing keys, bounds errors and string character reads preserve
            // the original structured errors without cloning a valid element.
            index.with_read(span, |index| {
                eval_index_value(target, index.clone(), span, optional_object)
            })
        });
    }
    if optional_object {
        // Under `?`, the checker has classified the target as KuValue. The
        // index type selects the tagged native operation: str -> object lookup,
        // int -> array lookup. Keep interpreter errors recoverable and identical
        // to those helpers even when the runtime tag is String/Object/Array/etc.
        match (target, &index) {
            (Value::Object(_), Value::String(_)) | (Value::Array(_), Value::Int(_)) => {}
            (_, Value::String(_)) => {
                return Ok(crate::stdlib::errors::err(
                    "object",
                    "type_unsupported",
                    "expected object value",
                ));
            }
            (_, Value::Int(_)) => {
                return Ok(crate::stdlib::errors::err(
                    "array",
                    "not_an_array",
                    "expected array value",
                ));
            }
            _ => {}
        }
    }
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
            // A KuValue array element read `arr[i]?` (optional_object) yields a
            // Result: Ok(element) in bounds, Err{domain:"array",
            // code:"index_out_of_bounds"} otherwise, so `?` propagates a
            // recoverable error. A static `nums[i]` (no `?`) still hard-panics
            // out of bounds — its non-Result element type is rejected by `?` in
            // the checker, so it never reaches here with optional_object set.
            if index < 0 || index as usize >= values.len() {
                if optional_object {
                    return Ok(crate::stdlib::errors::err(
                        "array",
                        "index_out_of_bounds",
                        format!("array index out of bounds: {index}"),
                    ));
                }
                return Err(KuError::runtime("array index out of bounds", span));
            }
            let element = values[index as usize].clone();
            if optional_object {
                Ok(crate::stdlib::errors::ok(element))
            } else {
                Ok(element)
            }
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
            // With `?` (optional_object) object indexing yields a Result:
            // Ok(value) when present, Err{domain:"object", code:"missing_key"}
            // when absent — so `?` propagates a recoverable error instead of
            // returning null. Without `?`, a missing key is a hard error.
            match fields.get(&key).cloned() {
                Some(value) if optional_object => Ok(crate::stdlib::errors::ok(value)),
                Some(value) => Ok(value),
                None if optional_object => Ok(crate::stdlib::errors::err(
                    "object",
                    "missing_key",
                    format!("missing object key: {key}"),
                )),
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

fn index_value_mut(target: &mut Value, index: Value, span: Span) -> KuResult<&mut Value> {
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
            Ok(&mut values[index as usize])
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
            fields
                .get_mut(&key)
                .ok_or_else(|| KuError::runtime(format!("object has no key '{key}'"), span))
        }
        other => Err(KuError::runtime(
            format!("type error: cannot index {}", other.type_name()),
            span,
        )),
    }
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

fn field_value(target: &Value, name: &str, span: Span) -> KuResult<Value> {
    match target {
        Value::Borrowed(view) => view.project(BorrowProjection::Field(name.to_string()), span),
        Value::Struct {
            name: struct_name,
            fields,
        } => fields.get(name).cloned().ok_or_else(|| {
            KuError::runtime(
                format!("struct '{struct_name}' has no field '{name}'"),
                span,
            )
        }),
        Value::Object(fields) => fields
            .get(name)
            .cloned()
            .ok_or_else(|| KuError::runtime(format!("object has no field '{name}'"), span)),
        other => Err(KuError::runtime(
            format!("type error: {} has no fields", other.type_name()),
            span,
        )),
    }
}

fn field_value_mut<'a>(target: &'a mut Value, name: &str, span: Span) -> KuResult<&'a mut Value> {
    match target {
        Value::Struct {
            name: struct_name,
            fields,
        } => fields.get_mut(name).ok_or_else(|| {
            KuError::runtime(
                format!("struct '{struct_name}' has no field '{name}'"),
                span,
            )
        }),
        Value::Object(fields) => fields
            .get_mut(name)
            .ok_or_else(|| KuError::runtime(format!("object has no field '{name}'"), span)),
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
            Value::String(http_route_method(method).to_string()),
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

fn http_route_method(method: &str) -> &'static str {
    match method {
        "get" => "GET",
        "post" => "POST",
        "put" => "PUT",
        "del" => "DELETE",
        _ => unreachable!("checked route methods are normalized before route append"),
    }
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
        (
            HTTP_LISTENER_LEASE_FIELD.to_string(),
            Value::HttpListenerLease(HttpListenerLease::new(listener_id)),
        ),
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
    let id = http_listener_registry::insert(listener)?;
    Ok((id, local.to_string()))
}

fn take_http_listener(id: i64, span: Span) -> KuResult<TcpListener> {
    http_listener_registry::take(id, span)
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
    http_listener_registry::close(id, span)
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
    if method.is_empty()
        || method.len() > HTTP_MAX_METHOD_BYTES
        || !method.bytes().all(is_http_token_byte)
    {
        return Err(KuError::runtime(
            "http route method must be a valid HTTP token of at most 32 bytes",
            span,
        ));
    }
    if !path.starts_with('/') {
        return Err(KuError::runtime(
            "http route path must start with '/'",
            span,
        ));
    }
    if path.len() > stdlib::http::MAX_REQUEST_TARGET_BYTES {
        return Err(KuError::runtime(
            format!(
                "http route path must be at most {} bytes",
                stdlib::http::MAX_REQUEST_TARGET_BYTES
            ),
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
            if !is_valid_uri_pchar_sequence(segment.as_bytes()) {
                return Err(KuError::runtime(
                    format!("invalid http route segment '{segment}'"),
                    span,
                ));
            }
            segments.push(segment.to_string());
        }
        if segments.len() > stdlib::http::MAX_REQUEST_PATH_SEGMENTS {
            return Err(KuError::runtime(
                format!(
                    "http route path must contain at most {} segments",
                    stdlib::http::MAX_REQUEST_PATH_SEGMENTS
                ),
                span,
            ));
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
    fn from_service(service: &Value, span: Span) -> KuResult<Self> {
        Ok(Self {
            read_header_timeout: service_duration(service, "read_header_timeout_ms", 5_000, span)?,
            read_body_timeout: service_duration(service, "read_body_timeout_ms", 10_000, span)?,
            write_timeout: service_duration(service, "write_timeout_ms", 10_000, span)?,
            idle_timeout: service_duration(service, "idle_timeout_ms", 5_000, span)?,
            handler_timeout: service_duration(service, "handler_timeout_ms", 15_000, span)?,
            max_header_bytes: bounded_service_int(
                service,
                "max_header_bytes",
                16 * 1024,
                stdlib::http::MAX_HEADER_BYTES,
                span,
            )?,
            max_body_bytes: bounded_service_int(
                service,
                "max_body_bytes",
                1_000_000,
                stdlib::http::MAX_BODY_BYTES,
                span,
            )?,
            max_connections: bounded_service_int(
                service,
                "max_connections",
                1024,
                stdlib::http::MAX_CONNECTIONS,
                span,
            )?,
            max_active_requests: bounded_service_int(
                service,
                "max_active_requests",
                256,
                stdlib::http::MAX_ACTIVE_REQUESTS,
                span,
            )?,
            max_pending_requests: bounded_service_int(
                service,
                "max_pending_requests",
                1024,
                stdlib::http::MAX_PENDING_REQUESTS,
                span,
            )?,
        })
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

// Exactly one over-limit socket may remain on the acceptor. Each poll performs
// at most one bounded read and one write; it never parks the interpreter while
// admitted requests are waiting to execute. Dropping this owner closes the
// socket and returns a pending-queue permit, if it held one.
struct HttpRejection {
    stream: TcpStream,
    _permit: Option<HttpConnectionPermit>,
    drain_deadline: Instant,
    write_deadline: Option<Instant>,
    write_timeout: Duration,
    remaining_header: usize,
    tail: [u8; 4],
    seen: usize,
    sent: usize,
}

impl HttpRejection {
    const RESPONSE: &'static [u8] = b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: 19\r\nconnection: close\r\n\r\nService Unavailable";

    fn new(
        stream: TcpStream,
        permit: Option<HttpConnectionPermit>,
        limits: HttpServerRuntimeLimits,
    ) -> Option<Self> {
        if stream.set_nonblocking(true).is_err() {
            return None;
        }
        Some(Self {
            stream,
            _permit: permit,
            drain_deadline: http_deadline(Duration::from_millis(50)),
            write_deadline: None,
            write_timeout: limits.write_timeout,
            remaining_header: limits.max_header_bytes,
            tail: [0; 4],
            seen: 0,
            sent: 0,
        })
    }

    fn begin_write(&mut self, now: Instant) {
        let start = now.min(self.drain_deadline);
        self.write_deadline = Some(start.checked_add(self.write_timeout).unwrap_or(start));
    }

    fn poll(&mut self) -> bool {
        let now = Instant::now();
        if self.write_deadline.is_none() {
            if now >= self.drain_deadline || self.remaining_header == 0 {
                self.begin_write(now);
            } else {
                let mut buffer = [0; 1024];
                let count = buffer.len().min(self.remaining_header);
                match self.stream.read(&mut buffer[..count]) {
                    Ok(0) => self.begin_write(Instant::now()),
                    Ok(read) => {
                        self.remaining_header -= read;
                        for byte in &buffer[..read] {
                            self.tail.rotate_left(1);
                            self.tail[3] = *byte;
                            self.seen = self.seen.saturating_add(1);
                            if (self.seen >= 4 && self.tail == *b"\r\n\r\n")
                                || (self.seen >= 2 && self.tail[2..] == *b"\n\n")
                            {
                                self.begin_write(Instant::now());
                                break;
                            }
                        }
                        if self.remaining_header == 0 && self.write_deadline.is_none() {
                            self.begin_write(Instant::now());
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        return false
                    }
                    Err(_) => return true,
                }
            }
        }
        let Some(deadline) = self.write_deadline else {
            return false;
        };
        if Instant::now() >= deadline {
            return true;
        }
        match self.stream.write(&Self::RESPONSE[self.sent..]) {
            Ok(0) => true,
            Ok(written) => {
                self.sent += written;
                if self.sent == Self::RESPONSE.len() {
                    let _ = self.stream.shutdown(Shutdown::Write);
                    true
                } else {
                    false
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                false
            }
            Err(_) => true,
        }
    }
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

// Shared by the real incremental I/O state machine: transport framing is
// separate from the one HTTP grammar and request conversion implementation.
struct ParsedHttpHead {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    content_length: usize,
}

fn parse_http_head(
    header: Vec<u8>,
    limits: HttpServerRuntimeLimits,
    span: Span,
) -> Result<ParsedHttpHead, HttpWireResponse> {
    if header.is_empty() {
        return Err(status_response(400, "Bad Request"));
    }
    let header_text = String::from_utf8(header).map_err(|_| status_response(400, "Bad Request"))?;
    let mut lines = header_text.split("\r\n");
    let Some(first_line) = lines.next() else {
        return Err(status_response(400, "Bad Request"));
    };
    let mut parts = first_line.split(' ');
    let Some(method) = parts.next() else {
        return Err(status_response(400, "Bad Request"));
    };
    let Some(target) = parts.next() else {
        return Err(status_response(400, "Bad Request"));
    };
    let Some(version) = parts.next() else {
        return Err(status_response(400, "Bad Request"));
    };
    if parts.next().is_some()
        || method.is_empty()
        || method.len() > HTTP_MAX_METHOD_BYTES
        || !method.bytes().all(is_http_token_byte)
        || version != "HTTP/1.1"
        || !is_valid_request_target(target)
    {
        return Err(status_response(400, "Bad Request"));
    }
    // Checked before the target is copied or routed: an over-long target is 414,
    // never truncated. Truncating would let two distinct paths that share a long
    // prefix resolve to the same route.
    if target.len() > stdlib::http::MAX_REQUEST_TARGET_BYTES {
        return Err(status_response(414, "URI Too Long"));
    }
    let method = method.to_string();
    let target = target.to_string();
    let mut headers = HashMap::new();
    let mut saw_host = false;
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(status_response(400, "Bad Request"));
        }
        let Some(colon) = line.find(':') else {
            return Err(status_response(400, "Bad Request"));
        };
        let name = &line[..colon];
        let raw_value = &line[colon + 1..];
        if name.is_empty()
            || !name.bytes().all(is_http_token_byte)
            || !is_safe_http_field_value(raw_value)
        {
            return Err(status_response(400, "Bad Request"));
        }
        let name = name.to_ascii_lowercase();
        let value = raw_value.trim_matches([' ', '\t']).to_string();
        match name.as_str() {
            "host" => {
                if saw_host || value.is_empty() {
                    return Err(status_response(400, "Bad Request"));
                }
                saw_host = true;
            }
            "transfer-encoding" => return Err(status_response(400, "Bad Request")),
            "expect" => return Err(status_response(417, "Expectation Failed")),
            "content-length" => {
                if content_length.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(status_response(400, "Bad Request"));
                }
                content_length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| status_response(400, "Bad Request"))?,
                );
            }
            _ => {}
        }
        headers.insert(name, value);
    }
    if !saw_host {
        return Err(status_response(400, "Bad Request"));
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > limits.max_body_bytes {
        return Err(status_response(413, "Content Too Large"));
    }
    let (path, query) =
        split_path_query(&target, span).map_err(|_| status_response(400, "Bad Request"))?;
    if path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
        > stdlib::http::MAX_REQUEST_PATH_SEGMENTS
    {
        return Err(status_response(414, "URI Too Long"));
    }
    Ok(ParsedHttpHead {
        method,
        path,
        query,
        headers,
        content_length,
    })
}

fn finish_http_request(
    head: ParsedHttpHead,
    body: Vec<u8>,
) -> Result<ParsedHttpRequest, HttpWireResponse> {
    let body = String::from_utf8(body).map_err(|_| status_response(400, "Bad Request"))?;
    Ok(ParsedHttpRequest {
        method: head.method,
        path: head.path,
        query: head.query,
        headers: head.headers,
        body,
    })
}

fn http_deadline(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout).unwrap_or(now)
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_valid_request_target(target: &str) -> bool {
    let bytes = target.as_bytes();
    if bytes.first() != Some(&b'/') {
        return false;
    }
    let mut in_query = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'?' && !in_query {
            in_query = true;
            index += 1;
            continue;
        }
        if byte == b'/' || (in_query && byte == b'?') {
            index += 1;
            continue;
        }
        if byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
            continue;
        }
        if !is_uri_pchar_byte(byte) {
            return false;
        }
        index += 1;
    }
    true
}

fn is_valid_uri_pchar_sequence(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else if is_uri_pchar_byte(bytes[index]) {
            index += 1;
        } else {
            return false;
        }
    }
    true
}

fn is_uri_pchar_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

fn is_safe_http_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f))
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

fn http_request_value(request: ParsedHttpRequest, params: HashMap<String, String>) -> Value {
    Value::Object(HashMap::from([
        ("method".to_string(), Value::String(request.method)),
        ("path".to_string(), Value::String(request.path)),
        ("params".to_string(), string_map_value(params)),
        ("query".to_string(), string_map_value(request.query)),
        ("headers".to_string(), string_map_value(request.headers)),
        ("body".to_string(), Value::String(request.body)),
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
    if let Value::Result { ok, value } = value {
        if ok {
            return value_to_http_response(*value);
        }
        return Some(status_response(500, "Internal Server Error"));
    }
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

// Encode once before selecting the original total write deadline. Header and
// body remain separate owners so a large body is not copied into a second wire
// buffer; partial writes retain their offsets in the I/O connection state.
fn encode_http_response(response: HttpWireResponse) -> (String, String) {
    let response = prepare_http_response(response);
    let reason = stdlib::http::status_text(response.status);
    let mut headers = response.headers;
    if !((100..200).contains(&response.status) || matches!(response.status, 204 | 304)) {
        headers.insert(
            "content-length".to_string(),
            response.body.len().to_string(),
        );
    }
    headers.insert("connection".to_string(), "close".to_string());
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason);
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    (head, response.body)
}

fn prepare_http_response(mut response: HttpWireResponse) -> HttpWireResponse {
    if !(100..=599).contains(&response.status) {
        return status_response(500, "Internal Server Error");
    }
    let mut headers = HashMap::new();
    for (name, value) in response.headers {
        if name.is_empty()
            || !name.bytes().all(is_http_token_byte)
            || !is_safe_http_field_value(&value)
        {
            return status_response(500, "Internal Server Error");
        }
        let name = name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "connection"
                | "content-length"
                | "keep-alive"
                | "proxy-connection"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) {
            continue;
        }
        if headers.insert(name, value).is_some() {
            return status_response(500, "Internal Server Error");
        }
    }
    if (100..200).contains(&response.status) || matches!(response.status, 204 | 304) {
        response.body.clear();
    }
    response.headers = headers;
    response
}

fn result_from_listener_operation(result: KuResult<()>, code: &str) -> KuResult<Value> {
    match result {
        Ok(()) => Ok(Value::Result {
            ok: true,
            value: Box::new(Value::Null),
        }),
        Err(err) if err.runtime_termination().is_some() => Err(err),
        Err(err) => Ok(http_error_result(code, err.message)),
    }
}

fn bounded_service_int(
    service: &Value,
    name: &str,
    default: usize,
    maximum: usize,
    span: Span,
) -> KuResult<usize> {
    let Value::Object(fields) = service else {
        return Err(KuError::runtime("http service must be an object", span));
    };
    let value = match fields.get(name) {
        Some(Value::Int(value)) => usize::try_from(*value).ok(),
        Some(Value::Null) | None => return Ok(default),
        Some(other) => {
            return Err(KuError::runtime(
                format!(
                    "type error: http config field '{name}' must be int but got {}",
                    other.type_name()
                ),
                span,
            ))
        }
    };
    match value {
        Some(value) if value > 0 && value <= maximum => Ok(value),
        Some(0) => Err(KuError::runtime(
            format!("http config field '{name}' must be a positive int"),
            span,
        )),
        _ => Err(KuError::runtime(
            format!("http config field '{name}' must be at most {maximum}"),
            span,
        )),
    }
}

fn service_duration(
    service: &Value,
    name: &str,
    default_ms: usize,
    span: Span,
) -> KuResult<Duration> {
    Ok(Duration::from_millis(bounded_service_int(
        service,
        name,
        default_ms,
        stdlib::http::MAX_TIMEOUT_MS as usize,
        span,
    )? as u64))
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

struct MatchBinding {
    name: String,
    value: Value,
    task_path: Option<Vec<ValueProjection>>,
}

type TaskMatchBindings = Vec<(String, Vec<ValueProjection>)>;

fn match_pattern(
    pattern: &MatchPattern,
    value: &Value,
    env: &mut Env,
    span: Span,
) -> KuResult<Option<TaskMatchBindings>> {
    let mut bindings = Vec::new();
    if !collect_match_bindings(pattern, value, &mut bindings, &mut Vec::new(), span)? {
        return Ok(None);
    }
    let mut task_bindings = Vec::new();
    for binding in bindings {
        if let Some(path) = binding.task_path {
            env.define_task_match_probe(binding.name.clone(), binding.value, span)?;
            task_bindings.push((binding.name, path));
        } else {
            env.define_owned(binding.name, binding.value, false, span)?;
        }
    }
    Ok(Some(task_bindings))
}

const MATCH_SCRATCH_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
enum MatchInput<'a> {
    Direct(&'a Value),
    View {
        root: &'a BorrowedValue,
        path_base: usize,
    },
}

enum MatchFields<'a> {
    Direct(&'a [Value]),
    View {
        root: &'a BorrowedValue,
        path_base: usize,
    },
}

struct MatchFrame<'a> {
    patterns: &'a [MatchPattern],
    fields: MatchFields<'a>,
    next: usize,
    path_len: usize,
}

// The two scratch buffers coexist. A replacement also coexists with its old
// backing until its iterators move; account for both visible capacities before
// accepting it. Output bindings remain their existing separately owned output.
fn reserve_match_scratch<T>(values: &mut Vec<T>, other_bytes: usize, span: Span) -> KuResult<()> {
    let error = || KuError::runtime("match traversal scratch budget exhausted", span);
    let bytes = std::mem::size_of::<T>();
    let available_bytes = MATCH_SCRATCH_BYTES
        .checked_sub(other_bytes)
        .ok_or_else(error)?;
    let limit = available_bytes / bytes;
    let required = values.len().checked_add(1).ok_or_else(error)?;
    if values.capacity() > limit || required > limit {
        return Err(error());
    }
    if required <= values.capacity() {
        return Ok(());
    }
    let available = limit - values.capacity();
    if required > available {
        return Err(error());
    }
    let target = values
        .capacity()
        .saturating_mul(2)
        .max(required)
        .min(available);
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(target)
        .map_err(|_| KuError::runtime("match traversal scratch out of memory", span))?;
    if replacement.capacity() > available {
        return Err(error());
    }
    replacement.append(values);
    *values = replacement;
    Ok(())
}

fn match_scratch_bytes<T>(values: &Vec<T>, span: Span) -> KuResult<usize> {
    values
        .capacity()
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| KuError::runtime("match traversal scratch budget exhausted", span))
}

fn collect_match_bindings(
    pattern: &MatchPattern,
    value: &Value,
    bindings: &mut Vec<MatchBinding>,
    path: &mut Vec<ValueProjection>,
    span: Span,
) -> KuResult<bool> {
    let path_base = path.len();
    let snapshot = bindings.len();
    let result = collect_match_bindings_inner(pattern, value, bindings, path, span);
    // False rolls back this whole pattern; Err retains earlier bindings, just
    // like the recursive calls did. Neither outcome may leak its path suffix.
    path.truncate(path_base);
    if matches!(result, Ok(false)) {
        bindings.truncate(snapshot);
    }
    result
}

fn collect_match_bindings_inner<'a>(
    pattern: &'a MatchPattern,
    value: &'a Value,
    bindings: &mut Vec<MatchBinding>,
    path: &mut Vec<ValueProjection>,
    span: Span,
) -> KuResult<bool> {
    let mut ancestors: Vec<MatchFrame<'a>> = Vec::new();
    let mut next = Some((pattern, MatchInput::Direct(value)));
    loop {
        if let Some((pattern, input)) = next.take() {
            let input = match input {
                MatchInput::Direct(Value::Borrowed(root)) => MatchInput::View {
                    root,
                    path_base: path.len(),
                },
                input => input,
            };
            let frame = match (pattern, input) {
                (MatchPattern::Wildcard, MatchInput::Direct(_)) => None,
                (MatchPattern::Binding(name), MatchInput::Direct(value)) => {
                    bindings.push(MatchBinding {
                        name: name.clone(),
                        value: value.clone(),
                        task_path: value.contains_owned_task(span)?.then(|| path.clone()),
                    });
                    None
                }
                (MatchPattern::Literal(literal), MatchInput::Direct(value)) => {
                    if value != &value_from_literal(literal) {
                        return Ok(false);
                    }
                    None
                }
                (
                    MatchPattern::EnumVariant {
                        enum_name,
                        variant,
                        fields: patterns,
                    },
                    MatchInput::Direct(value),
                ) => {
                    let Value::Enum {
                        name,
                        variant: actual,
                        fields,
                    } = value
                    else {
                        return Ok(false);
                    };
                    if name != enum_name || actual != variant {
                        return Ok(false);
                    }
                    if patterns.len() != fields.len() {
                        return Err(KuError::runtime(
                            format!("match pattern '{enum_name}.{variant}' expects {} fields but got {}", fields.len(), patterns.len()),
                            span,
                        ));
                    }
                    Some(MatchFrame {
                        patterns,
                        fields: MatchFields::Direct(fields),
                        next: 0,
                        path_len: path.len(),
                    })
                }
                (pattern, MatchInput::View { root, path_base }) => {
                    let projected = path.len() > path_base;
                    match pattern {
                        MatchPattern::Wildcard => {
                            if projected {
                                root.with_projection_read(&path[path_base..], span, |_| Ok(()))?;
                            }
                            None
                        }
                        MatchPattern::Binding(name) => {
                            let copied = if projected {
                                root.with_projection_read(&path[path_base..], span, |value| {
                                    Ok(value.copy_value())
                                })?
                            } else {
                                None
                            };
                            let Some(value) = copied else {
                                return Err(KuError::runtime("binding an owned borrowed match payload is not supported; clone to create an owned value", span));
                            };
                            bindings.push(MatchBinding {
                                name: name.clone(),
                                value,
                                task_path: None,
                            });
                            None
                        }
                        MatchPattern::Literal(literal) => {
                            let matched =
                                root.with_projection_read(&path[path_base..], span, |value| {
                                    Ok(value == &value_from_literal(literal))
                                });
                            let matched = if projected {
                                matched?
                            } else {
                                matched.unwrap_or(false)
                            };
                            if !matched {
                                return Ok(false);
                            }
                            None
                        }
                        MatchPattern::EnumVariant {
                            enum_name,
                            variant,
                            fields: patterns,
                        } => {
                            let matched =
                                root.with_projection_read(&path[path_base..], span, |value| {
                                    match value {
                                        Value::Enum {
                                            name,
                                            variant: actual,
                                            fields,
                                        } if name == enum_name && actual == variant => {
                                            if fields.len() != patterns.len() {
                                                return Err(KuError::runtime(
                                                    "match pattern field count mismatch",
                                                    span,
                                                ));
                                            }
                                            Ok(true)
                                        }
                                        _ => Ok(false),
                                    }
                                })?;
                            if !matched {
                                return Ok(false);
                            }
                            Some(MatchFrame {
                                patterns,
                                fields: MatchFields::View { root, path_base },
                                next: 0,
                                path_len: path.len(),
                            })
                        }
                    }
                }
            };
            if let Some(frame) = frame {
                if !frame.patterns.is_empty() {
                    let path_bytes = match_scratch_bytes(path, span)?;
                    reserve_match_scratch(&mut ancestors, path_bytes, span)?;
                    ancestors.push(frame);
                }
            }
        }
        loop {
            let Some(frame) = ancestors.last_mut() else {
                return Ok(true);
            };
            path.truncate(frame.path_len);
            let index = frame.next;
            if let Some(pattern) = frame.patterns.get(index) {
                frame.next += 1;
                let input = match frame.fields {
                    MatchFields::Direct(fields) => MatchInput::Direct(&fields[index]),
                    MatchFields::View { root, path_base } => MatchInput::View { root, path_base },
                };
                let frame_bytes = match_scratch_bytes(&ancestors, span)?;
                reserve_match_scratch(path, frame_bytes, span)?;
                path.push(ValueProjection::EnumField(index));
                next = Some((pattern, input));
                break;
            }
            ancestors.pop();
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

fn value_contains_task(value: &Value, span: Span) -> KuResult<bool> {
    value.contains_owned_task(span)
}

fn std_module_object_value(module: &str, span: Span) -> KuResult<Value> {
    match module {
        "http" => Ok(Value::Object(HashMap::from([
            ("status".to_string(), stdlib::http::status_object_value()),
            ("code".to_string(), stdlib::http::code_object_value()),
        ]))),
        _ => Err(KuError::runtime(
            format!(
                "std module '{module}' cannot be used as an object value yet; access functions with '{module}.name(...)'"
            ),
            span,
        )),
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
    if matches!(left, Value::Borrowed(_)) || matches!(right, Value::Borrowed(_)) {
        return left.with_read(span, |left| {
            right.with_read(span, |right| {
                match op {
                    BinaryOp::Equal => return Ok(Value::Bool(left == right)),
                    BinaryOp::NotEqual => return Ok(Value::Bool(left != right)),
                    BinaryOp::Add => {
                        if let (Value::String(left), Value::String(right)) = (left, right) {
                            let mut text = String::new();
                            let length = left.len().checked_add(right.len()).ok_or_else(|| {
                                KuError::runtime("string concat out of memory", span)
                            })?;
                            text.try_reserve_exact(length).map_err(|_| {
                                KuError::runtime("string concat out of memory", span)
                            })?;
                            text.push_str(left);
                            text.push_str(right);
                            return Ok(Value::String(text));
                        }
                    }
                    _ => {}
                }
                if let (Some(left), Some(right)) = (left.copy_value(), right.copy_value()) {
                    return eval_binary(op, left, right, span);
                }
                Err(KuError::runtime(
                    format!(
                        "type error: cannot apply operator to {} and {}",
                        left.type_name(),
                        right.type_name()
                    ),
                    span,
                ))
            })
        });
    }
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
        BinaryOp::Less => compare(left, right, span, |a, b| a < b, |a, b| a < b),
        BinaryOp::LessEqual => compare(left, right, span, |a, b| a <= b, |a, b| a <= b),
        BinaryOp::Greater => compare(left, right, span, |a, b| a > b, |a, b| a > b),
        BinaryOp::GreaterEqual => compare(left, right, span, |a, b| a >= b, |a, b| a >= b),
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

fn compare(
    left: Value,
    right: Value,
    span: Span,
    int_op: fn(i64, i64) -> bool,
    float_op: fn(f64, f64) -> bool,
) -> KuResult<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Bool(int_op(a, b))),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Bool(float_op(a, b))),
        (Value::Int(a), Value::Float(b)) => Ok(Value::Bool(float_op(a as f64, b))),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Bool(float_op(a, b as f64))),
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
        | Stmt::CompoundAssign { span, .. }
        | Stmt::DestructureAssign { span, .. }
        | Stmt::ObjectDestructureAssign { span, .. }
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

fn stress_usize_arg(name: &str, value: &Value, span: Span) -> KuResult<usize> {
    let Value::Int(value) = value else {
        return Err(KuError::runtime(
            format!("task.stress {name} must be int"),
            span,
        ));
    };
    usize::try_from(*value)
        .map_err(|_| KuError::runtime(format!("task.stress {name} must be non-negative"), span))
}

fn stress_u64_arg(name: &str, value: &Value, span: Span) -> KuResult<u64> {
    let Value::Int(value) = value else {
        return Err(KuError::runtime(
            format!("task.stress {name} must be int"),
            span,
        ));
    };
    u64::try_from(*value)
        .map_err(|_| KuError::runtime(format!("task.stress {name} must be non-negative"), span))
}

fn task_runtime_snapshot_value(snapshot: TaskRuntimeSnapshot) -> Value {
    Value::Object(HashMap::from([
        (
            "active_tasks".to_string(),
            usize_value(snapshot.active_tasks),
        ),
        (
            "registered_tasks".to_string(),
            usize_value(snapshot.registered_tasks),
        ),
        (
            "queued_tasks".to_string(),
            usize_value(snapshot.queued_tasks),
        ),
        ("wait_edges".to_string(), usize_value(snapshot.wait_edges)),
        (
            "queued_blocking_jobs".to_string(),
            usize_value(snapshot.queued_blocking_jobs),
        ),
        (
            "running_blocking_jobs".to_string(),
            usize_value(snapshot.running_blocking_jobs),
        ),
        (
            "resident_blocking_jobs".to_string(),
            usize_value(snapshot.resident_blocking_jobs),
        ),
        (
            "peak_resident_blocking_jobs".to_string(),
            usize_value(snapshot.peak_resident_blocking_jobs),
        ),
        (
            "peak_queued_blocking_jobs".to_string(),
            usize_value(snapshot.peak_queued_blocking_jobs),
        ),
        (
            "peak_running_blocking_jobs".to_string(),
            usize_value(snapshot.peak_running_blocking_jobs),
        ),
        (
            "accepted_blocking_jobs".to_string(),
            usize_value(snapshot.accepted_blocking_jobs),
        ),
        (
            "rejected_blocking_jobs".to_string(),
            usize_value(snapshot.rejected_blocking_jobs),
        ),
        (
            "rejected_blocking_queue".to_string(),
            usize_value(snapshot.rejected_blocking_queue),
        ),
        (
            "finished_blocking_jobs".to_string(),
            usize_value(snapshot.finished_blocking_jobs),
        ),
        (
            "task_workers".to_string(),
            usize_value(snapshot.task_workers),
        ),
        (
            "blocking_workers".to_string(),
            usize_value(snapshot.blocking_workers),
        ),
        (
            "total_submissions".to_string(),
            usize_value(snapshot.total_submissions),
        ),
        (
            "accepted_submissions".to_string(),
            usize_value(snapshot.accepted_submissions),
        ),
        (
            "rejected_task_limit".to_string(),
            usize_value(snapshot.rejected_task_limit),
        ),
        (
            "rejected_task_queue".to_string(),
            usize_value(snapshot.rejected_task_queue),
        ),
        (
            "rejected_task_internal".to_string(),
            usize_value(snapshot.rejected_task_internal),
        ),
        (
            "finished_tasks".to_string(),
            usize_value(snapshot.finished_tasks),
        ),
        (
            "finished_cancelled_tasks".to_string(),
            usize_value(snapshot.finished_cancelled_tasks),
        ),
        (
            "finished_timed_out_tasks".to_string(),
            usize_value(snapshot.finished_timed_out_tasks),
        ),
        (
            "suppressed_cleanup_outcomes".to_string(),
            usize_value(snapshot.suppressed_cleanup_outcomes),
        ),
        (
            "cleanup_timeouts".to_string(),
            usize_value(snapshot.cleanup_timeouts),
        ),
        (
            "cleanup_unfinished_tasks".to_string(),
            usize_value(snapshot.cleanup_unfinished_tasks),
        ),
    ]))
}

fn task_stress_report_value(report: TaskStressReport) -> Value {
    Value::Object(HashMap::from([
        ("demand".to_string(), usize_value(report.demand)),
        ("producers".to_string(), usize_value(report.producers)),
        ("hold_ms".to_string(), u64_value(report.hold_ms)),
        ("peak_active".to_string(), usize_value(report.peak_active)),
        ("accepted".to_string(), usize_value(report.accepted)),
        (
            "rejected_limit".to_string(),
            usize_value(report.rejected_limit),
        ),
        (
            "rejected_queue".to_string(),
            usize_value(report.rejected_queue),
        ),
        (
            "rejected_internal".to_string(),
            usize_value(report.rejected_internal),
        ),
        ("finished".to_string(), usize_value(report.finished)),
        ("submit_ms".to_string(), u128_value(report.submit_ms)),
        ("total_ms".to_string(), u128_value(report.total_ms)),
        ("task_workers".to_string(), usize_value(report.task_workers)),
        (
            "blocking_workers".to_string(),
            usize_value(report.blocking_workers),
        ),
    ]))
}

fn usize_value(value: usize) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn u64_value(value: u64) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn u128_value(value: u128) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
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
        (
            "fs",
            "read" | "try_read" | "write" | "try_write" | "exists" | "read_dir",
        ) | ("config", "env" | "env_file" | "yaml")
            | ("time", "sleep")
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

// Template interpolation is parsed independently of its enclosing function.
// Keep one budget across that reparsing and the surrounding capture walk; the
// parser's per-expression nesting guard alone cannot bound this traversal.
const MAX_CAPTURE_SCAN_DEPTH: usize = 128;
const MAX_CAPTURE_SCAN_WORK: usize = 262_144;
const MAX_CAPTURE_TEMPLATE_BYTES: usize = 1_048_576;
const MAX_CAPTURE_TEMPLATE_TOKENS: usize = 1_024;

struct CaptureScanBudget {
    remaining_work: usize,
    remaining_template_bytes: usize,
}

impl CaptureScanBudget {
    fn new() -> Self {
        Self {
            remaining_work: MAX_CAPTURE_SCAN_WORK,
            remaining_template_bytes: MAX_CAPTURE_TEMPLATE_BYTES,
        }
    }

    fn spend(&mut self, amount: usize, span: Span) -> KuResult<()> {
        self.remaining_work = self.remaining_work.checked_sub(amount).ok_or_else(|| {
            KuError::runtime("capture analysis limit: work budget exhausted", span)
        })?;
        Ok(())
    }

    fn enter(&mut self, depth: usize, span: Span) -> KuResult<()> {
        if depth >= MAX_CAPTURE_SCAN_DEPTH {
            return Err(KuError::runtime(
                "capture analysis limit: nesting depth exceeded",
                span,
            ));
        }
        self.spend(1, span)
    }

    fn clone_scope(&mut self, bound: &HashSet<String>, span: Span) -> KuResult<HashSet<String>> {
        self.spend(bound.len(), span)?;
        Ok(bound.clone())
    }

    fn template(&mut self, raw: &str, span: Span) -> KuResult<()> {
        self.remaining_template_bytes = self
            .remaining_template_bytes
            .checked_sub(raw.len())
            .ok_or_else(|| {
                KuError::runtime(
                    "capture analysis limit: template byte budget exhausted",
                    span,
                )
            })?;
        Ok(())
    }
}

pub(crate) fn function_capture_names(function: &FnDecl) -> KuResult<HashSet<String>> {
    let mut budget = CaptureScanBudget::new();
    budget.spend(function.params.len(), function.span)?;
    let mut bound = HashSet::new();
    bound.insert(function.name.clone());
    for param in &function.params {
        bound.insert(param.name.clone());
    }
    let mut free = HashSet::new();
    collect_free_block(&function.body, &mut bound, &mut free, &mut budget, 0)?;
    Ok(free)
}

pub(crate) fn closure_capture_names(
    params: &[FunctionParam],
    body: &[Stmt],
) -> KuResult<HashSet<String>> {
    let mut budget = CaptureScanBudget::new();
    budget.spend(
        params.len(),
        params
            .first()
            .map_or_else(Span::default, |param| param.span),
    )?;
    let mut bound = HashSet::new();
    for param in params {
        bound.insert(param.name.clone());
    }
    let mut free = HashSet::new();
    collect_free_block(body, &mut bound, &mut free, &mut budget, 0)?;
    Ok(free)
}

fn collect_free_block(
    body: &[Stmt],
    bound: &mut HashSet<String>,
    free: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    depth: usize,
) -> KuResult<()> {
    for stmt in body {
        collect_free_stmt(stmt, bound, free, budget, depth)?;
    }
    Ok(())
}

fn collect_free_stmt(
    stmt: &Stmt,
    bound: &mut HashSet<String>,
    free: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    depth: usize,
) -> KuResult<()> {
    let span = stmt_span(stmt);
    budget.enter(depth, span)?;
    let depth = depth + 1;
    match stmt {
        Stmt::VarDecl { name, value, .. } => {
            collect_free_expr(value, bound, free, budget, depth)?;
            bound.insert(name.clone());
        }
        Stmt::Assign { name, value, .. } => {
            collect_free_expr(value, bound, free, budget, depth)?;
            collect_free_assignment_name(name, bound, free, budget, span)?;
        }
        Stmt::AssignTarget { target, value, .. } => {
            collect_free_assign_target(target, bound, free, budget, depth, span)?;
            collect_free_expr(value, bound, free, budget, depth)?;
        }
        Stmt::CompoundAssign { target, value, .. } => {
            collect_free_assign_target(target, bound, free, budget, depth, span)?;
            collect_free_expr(value, bound, free, budget, depth)?;
        }
        Stmt::DestructureAssign { names, values, .. } => {
            for value in values {
                collect_free_expr(value, bound, free, budget, depth)?;
            }
            for name in names.iter().flatten() {
                collect_free_assignment_name(name, bound, free, budget, span)?;
            }
        }
        Stmt::ObjectDestructureAssign {
            bindings,
            rest,
            value,
            ..
        } => {
            collect_free_expr(value, bound, free, budget, depth)?;
            for binding in bindings {
                budget.spend(1, span)?;
                if let Some(default) = &binding.default {
                    collect_free_expr(default, bound, free, budget, depth)?;
                }
                if let Some(local) = &binding.local {
                    collect_free_assignment_name(local, bound, free, budget, span)?;
                }
            }
            if let Some(local) = rest.as_ref().and_then(|rest| rest.local.as_ref()) {
                collect_free_assignment_name(local, bound, free, budget, span)?;
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_free_expr(condition, bound, free, budget, depth)?;
            let mut then_bound = budget.clone_scope(bound, span)?;
            collect_free_block(then_branch, &mut then_bound, free, budget, depth)?;
            let mut else_bound = budget.clone_scope(bound, span)?;
            collect_free_block(else_branch, &mut else_bound, free, budget, depth)?;
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_free_expr(condition, bound, free, budget, depth)?;
            let mut scoped = budget.clone_scope(bound, span)?;
            collect_free_block(body, &mut scoped, free, budget, depth)?;
        }
        Stmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            collect_free_expr(iterable, bound, free, budget, depth)?;
            let mut scoped = budget.clone_scope(bound, span)?;
            scoped.insert(name.clone());
            collect_free_block(body, &mut scoped, free, budget, depth)?;
        }
        Stmt::Function(function) => {
            let mut nested = budget.clone_scope(bound, span)?;
            budget.spend(function.params.len(), span)?;
            nested.insert(function.name.clone());
            nested.extend(function.params.iter().map(|param| param.name.clone()));
            collect_free_block(&function.body, &mut nested, free, budget, depth)?;
            bound.insert(function.name.clone());
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            finally_body,
            ..
        } => {
            let mut try_bound = budget.clone_scope(bound, span)?;
            collect_free_block(body, &mut try_bound, free, budget, depth)?;
            let mut catch_bound = budget.clone_scope(bound, span)?;
            if let Some(name) = catch_name {
                catch_bound.insert(name.clone());
            }
            collect_free_block(catch_body, &mut catch_bound, free, budget, depth)?;
            let mut finally_bound = budget.clone_scope(bound, span)?;
            collect_free_block(finally_body, &mut finally_bound, free, budget, depth)?;
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            collect_free_expr(value, bound, free, budget, depth)?;
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_free_expr(value, bound, free, budget, depth)?;
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Expr { expr, .. } => collect_free_expr(expr, bound, free, budget, depth)?,
    }
    Ok(())
}

fn collect_free_assignment_name(
    name: &str,
    bound: &mut HashSet<String>,
    free: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    span: Span,
) -> KuResult<()> {
    budget.spend(1, span)?;
    if !bound.contains(name) {
        free.insert(name.to_string());
    }
    bound.insert(name.to_string());
    Ok(())
}

fn collect_free_assign_target(
    target: &AssignTarget,
    bound: &HashSet<String>,
    free: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    depth: usize,
    span: Span,
) -> KuResult<()> {
    budget.enter(depth, span)?;
    let depth = depth + 1;
    match target {
        AssignTarget::Variable(name) => {
            if !bound.contains(name) {
                free.insert(name.clone());
            }
        }
        AssignTarget::Index { target, index } => {
            collect_free_expr(target, bound, free, budget, depth)?;
            collect_free_expr(index, bound, free, budget, depth)?;
        }
        AssignTarget::Field { target, .. } => {
            collect_free_expr(target, bound, free, budget, depth)?
        }
    }
    Ok(())
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

fn collect_free_expr(
    expr: &Expr,
    bound: &HashSet<String>,
    free: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    depth: usize,
) -> KuResult<()> {
    budget.enter(depth, expr.span)?;
    let depth = depth + 1;
    match &expr.kind {
        ExprKind::Variable(name) => {
            if !bound.contains(name) {
                free.insert(name.clone());
            }
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_free_expr(expr, bound, free, budget, depth)?;
        }
        ExprKind::Binary { left, right, .. } => {
            collect_free_expr(left, bound, free, budget, depth)?;
            collect_free_expr(right, bound, free, budget, depth)?;
        }
        ExprKind::Call { callee, args } => {
            collect_free_expr(callee, bound, free, budget, depth)?;
            for arg in args {
                collect_free_expr(arg, bound, free, budget, depth)?;
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_free_expr(value, bound, free, budget, depth)?;
            }
        }
        ExprKind::Index { target, index } => {
            collect_free_expr(target, bound, free, budget, depth)?;
            collect_free_expr(index, bound, free, budget, depth)?;
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_free_expr(target, bound, free, budget, depth)?
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_free_expr(value, bound, free, budget, depth)?;
            }
        }
        ExprKind::Match { value, arms } => {
            collect_free_expr(value, bound, free, budget, depth)?;
            for arm in arms {
                let mut arm_bound = budget.clone_scope(bound, expr.span)?;
                bind_match_pattern_names(&arm.pattern, &mut arm_bound, budget, depth, expr.span)?;
                if let Some(guard) = &arm.guard {
                    collect_free_expr(guard, &arm_bound, free, budget, depth)?;
                }
                collect_free_expr(&arm.value, &arm_bound, free, budget, depth)?;
            }
        }
        ExprKind::Function { params, body, .. } => {
            let mut nested = budget.clone_scope(bound, expr.span)?;
            budget.spend(params.len(), expr.span)?;
            for param in params {
                nested.insert(param.name.clone());
            }
            collect_free_block(body, &mut nested, free, budget, depth)?;
        }
        ExprKind::Literal(Literal::TemplateString(raw)) => {
            collect_free_template(raw, expr.span, bound, free, budget, depth)?;
        }
        ExprKind::Literal(_) => {}
    }
    Ok(())
}

/// Visit actual interpolation expressions in source order, without retaining
/// every parsed AST at once. Validate the complete generated expression before
/// invoking the caller, including nested templates and their shared budget.
/// An error must never be interpreted as an empty set of captures/effects.
pub(crate) fn visit_template_capture_expressions(
    raw: &str,
    span: Span,
    mut visitor: impl FnMut(&Expr) -> KuResult<()>,
) -> KuResult<()> {
    let bound = HashSet::new();
    let mut validation_captures = HashSet::new();
    visit_template_capture_expressions_with_budget(
        raw,
        span,
        &mut CaptureScanBudget::new(),
        0,
        &mut |expression, budget, depth| {
            collect_free_expr(expression, &bound, &mut validation_captures, budget, depth)?;
            visitor(expression)
        },
    )
}

fn collect_free_template(
    raw: &str,
    span: Span,
    bound: &HashSet<String>,
    free: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    depth: usize,
) -> KuResult<()> {
    visit_template_capture_expressions_with_budget(
        raw,
        span,
        budget,
        depth,
        &mut |expression, budget, depth| collect_free_expr(expression, bound, free, budget, depth),
    )
}

fn visit_template_capture_expressions_with_budget(
    raw: &str,
    span: Span,
    budget: &mut CaptureScanBudget,
    depth: usize,
    visitor: &mut impl FnMut(&Expr, &mut CaptureScanBudget, usize) -> KuResult<()>,
) -> KuResult<()> {
    budget.enter(depth, span)?;
    budget.template(raw, span)?;
    // Match eval_template's streaming splitting and escaping exactly. In
    // particular the first unescaped '}' ends an interpolation; this scan must
    // not accept a different nested-brace grammar from actual evaluation.
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            chars.next();
            continue;
        }
        if ch != '{' {
            continue;
        }
        let mut source = String::new();
        let mut found_end = false;
        while let Some(inner) = chars.next() {
            if inner == '\\' {
                if let Some(next) = chars.next() {
                    source.push('\\');
                    source.push(next);
                }
                continue;
            }
            if inner == '}' {
                found_end = true;
                break;
            }
            source.push(inner);
        }
        if !found_end {
            return Err(KuError::runtime(
                "unterminated template interpolation",
                span,
            ));
        }
        if source.trim().is_empty() {
            return Err(KuError::runtime("empty template interpolation", span));
        }
        let tokens = Lexer::new(&source).tokenize()?;
        // Long flat binary expressions are constructed iteratively by Parser,
        // but their AST drops recursively. Bound the newly owned AST before
        // parsing, including the failure path after a capture-depth rejection.
        if tokens.len() > MAX_CAPTURE_TEMPLATE_TOKENS {
            return Err(KuError::parse(
                "capture analysis limit: template token limit exceeded (too many tokens)",
                span,
            ));
        }
        budget.spend(tokens.len(), span)?;
        let expression = Parser::new(tokens).parse_expression_only()?;
        visitor(&expression, budget, depth + 1)?;
    }
    Ok(())
}

fn bind_match_pattern_names(
    pattern: &MatchPattern,
    bound: &mut HashSet<String>,
    budget: &mut CaptureScanBudget,
    depth: usize,
    span: Span,
) -> KuResult<()> {
    let mut ancestors: Vec<(std::slice::Iter<'_, MatchPattern>, usize)> = Vec::new();
    let mut next = Some((pattern, depth));
    loop {
        if let Some((pattern, depth)) = next.take() {
            // Preserve the old preorder work charge and exact semantic depth.
            budget.enter(depth, span)?;
            match pattern {
                MatchPattern::Binding(name) => {
                    bound.insert(name.clone());
                }
                MatchPattern::EnumVariant { fields, .. } if !fields.is_empty() => {
                    reserve_match_scratch(&mut ancestors, 0, span)?;
                    ancestors.push((fields.iter(), depth + 1));
                }
                _ => {}
            }
        }
        loop {
            let Some((children, depth)) = ancestors.last_mut() else {
                return Ok(());
            };
            if let Some(child) = children.next() {
                next = Some((child, *depth));
                break;
            }
            ancestors.pop();
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod template_capture_tests {
    use super::*;

    fn expression(source: &str) -> Expr {
        Parser::new(Lexer::new(source).tokenize().expect("lex fixture"))
            .parse_expression_only()
            .expect("parse fixture")
    }

    fn function(source: &str) -> FnDecl {
        let program = Parser::new(Lexer::new(source).tokenize().expect("lex fixture"))
            .parse_program()
            .expect("parse fixture");
        let Item::Function(function) = program.items.into_iter().next().expect("one function")
        else {
            panic!("expected function fixture");
        };
        function
    }

    #[test]
    fn named_function_template_capture_respects_self_parameters_and_locals() {
        let function = function(
            "fn Render(arg: int): str { local: int = 3; return `{helper(arg + local)} {outer} {Render(arg)}` }",
        );
        assert_eq!(
            function_capture_names(&function).expect("capture analysis"),
            HashSet::from(["helper".to_string(), "outer".to_string()]),
        );
    }

    #[test]
    fn closure_template_capture_uses_expressions_and_ignores_escaped_braces() {
        let expression = expression(
            r#"fn(value: int) { local: str = "owned"; return `\{escaped\} {helper(value)} {local.clone()} {outer}` }"#,
        );
        let ExprKind::Function { params, body, .. } = expression.kind else {
            panic!("expected closure fixture");
        };
        assert_eq!(
            closure_capture_names(&params, &body).expect("capture analysis"),
            HashSet::from(["helper".to_string(), "outer".to_string()]),
        );
    }

    #[test]
    fn template_nested_arrow_capture_excludes_its_parameter() {
        let expression = expression("fn() { return `{((local: int) => local + outer)(1)}` }");
        let ExprKind::Function { params, body, .. } = expression.kind else {
            panic!("expected closure fixture");
        };
        assert_eq!(
            closure_capture_names(&params, &body).expect("capture analysis"),
            HashSet::from(["outer".to_string()]),
        );
    }

    #[test]
    fn runtime_named_and_anonymous_functions_retain_template_only_capture() {
        let span = Span::default();
        for named in [false, true] {
            let mut interpreter = Interpreter::new();
            let mut env = Env::new();
            env.define_owned("count".into(), Value::Int(42), true, span)
                .expect("define capture");
            if named {
                interpreter
                    .exec_stmt(
                        &Stmt::Function(function("fn Render(): str { return `{count}` }")),
                        &mut env,
                        0,
                    )
                    .expect("create named function");
            } else {
                let closure = interpreter
                    .eval(&expression("fn() { return `{count}` }"), &mut env, 0)
                    .expect("create closure");
                env.define_owned("Render".into(), closure, true, span)
                    .expect("define closure");
            }
            let value = interpreter
                .eval(&expression("Render()"), &mut env, 0)
                .expect("invoke template closure");
            assert!(matches!(value, Value::String(text) if text == "42"));
        }
    }

    #[test]
    fn malformed_templates_fail_capture_analysis_instead_of_returning_empty() {
        for raw in ["{", "{}", "{name +}", "{name other}", "{@}"] {
            let span = Span::default();
            let body = [Stmt::Return {
                value: Some(Expr::new(
                    ExprKind::Literal(Literal::TemplateString(raw.into())),
                    span,
                )),
                span,
            }];
            assert!(closure_capture_names(&[], &body).is_err(), "{raw}");
            let mut calls = 0;
            assert!(visit_template_capture_expressions(raw, span, |_| {
                calls += 1;
                Ok(())
            })
            .is_err());
            assert_eq!(calls, 0, "invalid interpolation must not reach visitor");
        }
    }

    #[test]
    fn template_capture_visitor_preserves_order_and_propagates_visitor_failure() {
        let mut names = Vec::new();
        visit_template_capture_expressions("{first}{second}", Span::default(), |expr| {
            let ExprKind::Variable(name) = &expr.kind else {
                panic!("expected variable interpolation");
            };
            names.push(name.clone());
            Ok(())
        })
        .expect("visit both interpolations");
        assert_eq!(names, ["first", "second"]);

        let mut calls = 0;
        let error = visit_template_capture_expressions("{first}{second}", Span::default(), |_| {
            calls += 1;
            Err(KuError::runtime("visitor failure", Span::default()))
        })
        .expect_err("visitor failure must propagate");
        assert_eq!(calls, 1);
        assert_eq!(error.message, "visitor failure");
    }

    #[test]
    fn template_capture_visitor_rejects_deep_generated_ast_before_callback() {
        // Flat sums now fail during construction, before capture validation.
        // Keep that source and require the earlier, specific parse error.
        let raw = format!("{{{}0}}", "0+".repeat(MAX_CAPTURE_SCAN_DEPTH + 1));
        let mut calls = 0;
        let error = visit_template_capture_expressions(&raw, Span::default(), |_| {
            calls += 1;
            Ok(())
        })
        .expect_err("deep generated AST must be rejected");
        assert_eq!(error.kind, crate::error::KuErrorKind::Parse);
        assert_eq!(
            error.message,
            "maximum expression tree depth exceeded; expression is too deeply nested"
        );
        assert_eq!(calls, 0);

        // Parentheses split local binary loops. Exactly 128 structural nodes
        // fit Parser; interpolation's additional capture depth must still
        // reject before the callback using the unchanged capture limit.
        let mut expression = "0".to_owned();
        for additions in [32, 32, 32, 31] {
            expression = format!("({expression}){}", "+0".repeat(additions));
        }
        let raw = format!("{{{expression}}}");
        let error = visit_template_capture_expressions(&raw, Span::default(), |_| {
            calls += 1;
            Ok(())
        })
        .expect_err("capture depth remains bounded after parser admission");
        assert_eq!(error.kind, crate::error::KuErrorKind::Runtime);
        assert_eq!(
            error.message,
            "capture analysis limit: nesting depth exceeded"
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn template_capture_token_limit_precedes_new_ast_construction() {
        let raw = format!("{{{}0}}", "0+".repeat(MAX_CAPTURE_TEMPLATE_TOKENS));
        let mut calls = 0;
        let error = visit_template_capture_expressions(&raw, Span::default(), |_| {
            calls += 1;
            Ok(())
        })
        .expect_err("oversized token stream must be rejected");
        assert!(error.message.contains("template token limit exceeded"));
        assert_eq!(calls, 0);
    }

    #[test]
    fn template_capture_work_is_shared_between_interpolations() {
        let mut budget = CaptureScanBudget {
            remaining_work: 5,
            remaining_template_bytes: 64,
        };
        let error = collect_free_template(
            "{first}{second}",
            Span::default(),
            &HashSet::new(),
            &mut HashSet::new(),
            &mut budget,
            0,
        )
        .expect_err("second interpolation must not reset work budget");
        assert!(error.message.contains("work budget exhausted"));
    }

    #[test]
    fn template_capture_byte_budget_is_checked_before_lexing() {
        let mut budget = CaptureScanBudget {
            remaining_work: MAX_CAPTURE_SCAN_WORK,
            remaining_template_bytes: 5,
        };
        let error = collect_free_template(
            "{name}",
            Span::default(),
            &HashSet::new(),
            &mut HashSet::new(),
            &mut budget,
            0,
        )
        .expect_err("template must fit cumulative byte budget");
        assert!(error.message.contains("template byte budget exhausted"));
    }
}

#[cfg(test)]
mod collection_tests {
    use super::*;

    #[test]
    fn failed_pure_append_argument_leaves_the_receiver_unchanged() {
        let span = Span::default();
        let integer = |value| Expr::new(ExprKind::Literal(Literal::Int(value)), span);
        let variable = |name: &str| Expr::new(ExprKind::Variable(name.into()), span);
        let failures = [
            Expr::new(
                ExprKind::Binary {
                    left: Box::new(integer(1)),
                    op: BinaryOp::Divide,
                    right: Box::new(integer(0)),
                },
                span,
            ),
            Expr::new(
                ExprKind::Index {
                    target: Box::new(variable("other")),
                    index: Box::new(integer(9)),
                },
                span,
            ),
        ];
        for piece in failures {
            let mut env = Env::new();
            env.define_owned(
                "values".into(),
                Value::Array(vec![Value::Int(42)]),
                true,
                span,
            )
            .unwrap();
            env.define_owned("other".into(), Value::Array(Vec::new()), true, span)
                .unwrap();
            let assignment = Stmt::Assign {
                name: "values".into(),
                value: Expr::new(
                    ExprKind::Call {
                        callee: Box::new(Expr::new(
                            ExprKind::Field {
                                target: Box::new(variable("values")),
                                name: "push".into(),
                            },
                            span,
                        )),
                        args: vec![piece],
                    },
                    span,
                ),
                span,
            };
            assert!(Interpreter::new()
                .exec_stmt(&assignment, &mut env, 0)
                .is_err());
            env.with_value("values", span, |value| {
                assert!(matches!(value, Value::Array(values) if values.len() == 1 && matches!(values[0], Value::Int(42))));
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn borrowed_children_are_rejected_at_each_container_insertion_boundary() {
        let span = Span::default();
        for source in [
            "[value]",
            "{ item: value }",
            "other.push(value)",
            "object.get_or({}, \"missing\", value)",
            "ok(value)",
        ] {
            let mut env = Env::new();
            let (value, _lease) = BorrowLease::temporary(Value::String("borrowed".into()));
            env.define_parameter("value".into(), value, false, span)
                .unwrap();
            env.define_owned("other".into(), Value::Array(Vec::new()), true, span)
                .unwrap();
            let expression = Parser::new(Lexer::new(source).tokenize().unwrap())
                .parse_expression_only()
                .unwrap();
            let error = Interpreter::new()
                .eval(&expression, &mut env, 0)
                .expect_err(source);
            assert!(
                error.message.contains("borrowed value"),
                "{source}: {error:?}"
            );
            env.with_value("other", span, |value| {
                assert!(matches!(value, Value::Array(values) if values.is_empty()));
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn ordinary_function_modes_and_readonly_temporaries_create_no_borrow_storage() {
        let program = Parser::new(
            Lexer::new("fn ordinary(a: int, b: str) {} fn inspect(&value: str) {}")
                .tokenize()
                .unwrap(),
        )
        .parse_program()
        .unwrap();
        let Item::Function(ordinary) = &program.items[0] else {
            panic!("expected function")
        };
        assert_eq!(explicit_borrow_modes(ordinary).capacity(), 0);
        let Item::Function(inspect) = &program.items[1] else {
            panic!("expected function")
        };
        assert_eq!(explicit_borrow_modes(inspect), [ParamMode::View]);

        let mut env = Env::new();
        env.define_owned(
            "ch".into(),
            Value::String("5".into()),
            true,
            Span::default(),
        )
        .unwrap();
        for source in [
            r#"str("Ku")"#,
            r#""Ku".contains("K")"#,
            "[1, 2, 3].len()",
            r#""0123456789".contains(ch)"#,
        ] {
            let expression = Parser::new(Lexer::new(source).tokenize().unwrap())
                .parse_expression_only()
                .unwrap();
            let before = crate::value::borrow_roots_created();
            for _ in 0..64 {
                Interpreter::new().eval(&expression, &mut env, 0).unwrap();
            }
            assert_eq!(
                crate::value::borrow_roots_created(),
                before,
                "readonly temporary/character helper must not create Arc loan roots: {source}"
            );
        }
    }

    #[test]
    fn readonly_existing_binding_uses_one_guard_and_keeps_large_storage() {
        let span = Span::default();
        let text = "Ku".repeat(500_000);
        let original = text.as_ptr();
        let mut env = Env::new();
        env.define_owned("text".into(), Value::String(text), true, span)
            .unwrap();
        let expression = Parser::new(Lexer::new(r#"text.contains("K")"#).tokenize().unwrap())
            .parse_expression_only()
            .unwrap();
        let before = crate::value::borrow_roots_created();
        for _ in 0..64 {
            assert_eq!(
                Interpreter::new().eval(&expression, &mut env, 0).unwrap(),
                Value::Bool(true)
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
        assert_eq!(
            crate::value::borrow_roots_created() - before,
            64,
            "borrow existing source once; keep literal needle owned without a loan"
        );
    }

    #[test]
    fn readonly_runtime_routes_match_the_borrow_metadata() {
        for module in ["string", "array", "json"] {
            for method in [
                "len",
                "byte_len",
                "is_empty",
                "contains",
                "starts_with",
                "ends_with",
                "stringify",
                "encode",
                "push",
            ] {
                assert_eq!(
                    readonly_dotted_name(module, method).is_some(),
                    stdlib::metadata::supports_borrowed_call(&format!("{module}.{method}"))
                );
            }
        }
    }
}
