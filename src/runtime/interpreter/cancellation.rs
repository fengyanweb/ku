use super::*;

#[derive(Clone, Copy)]
pub(super) enum FinallyMode {
    Skip,
    Ordinary,
    Protected(RuntimeTermination),
}

pub(super) trait ScopeExit {
    fn cleanup_context(&self) -> Option<RuntimeTermination> {
        None
    }
    fn remember_cleanup(&mut self, _context: RuntimeTermination) {}
    fn collect_tasks(
        &self,
        _tasks: &mut Vec<crate::runtime::task::TaskHandle>,
        _span: Span,
    ) -> KuResult<()> {
        Ok(())
    }
}

impl ScopeExit for Flow {
    fn cleanup_context(&self) -> Option<RuntimeTermination> {
        match self {
            Flow::Return { cleanup, .. }
            | Flow::Fail { cleanup, .. }
            | Flow::Break(cleanup)
            | Flow::LoopContinue(cleanup) => *cleanup,
            Flow::Continue => None,
        }
    }
    fn remember_cleanup(&mut self, context: RuntimeTermination) {
        match self {
            Flow::Return { cleanup, .. }
            | Flow::Fail { cleanup, .. }
            | Flow::Break(cleanup)
            | Flow::LoopContinue(cleanup) => {
                *cleanup = Some(match *cleanup {
                    Some(mut previous) => {
                        previous.cleanup_deadline =
                            previous.cleanup_deadline.min(context.cleanup_deadline);
                        previous
                    }
                    None => context,
                });
            }
            Flow::Continue => {}
        }
    }
    fn collect_tasks(
        &self,
        tasks: &mut Vec<crate::runtime::task::TaskHandle>,
        span: Span,
    ) -> KuResult<()> {
        if let Flow::Return { value, .. } | Flow::Fail { value, .. } = self {
            value.collect_owned_tasks(tasks, span)?;
        }
        Ok(())
    }
}

impl Interpreter {
    /// Arbitration shared by the recursive evaluator and the flat driver.
    /// This prepares cleanup only; the caller executes the actual finally body.
    pub(super) fn prepare_finally(
        &mut self,
        env: &Env,
        result: &KuResult<Flow>,
        span: Span,
    ) -> FinallyMode {
        self.observe_termination(result);
        let terminating = matches!(result, Err(error) if error.runtime_termination().is_some());
        let cleanup_failure = result.is_err() && current_cleanup_context().is_some();
        if result.is_err() && !terminating && !cleanup_failure {
            // Ordinary fatal errors retain their existing boundary.
            return FinallyMode::Skip;
        }
        if terminating || cleanup_failure {
            let context = self
                .termination
                .or_else(current_cleanup_context)
                .expect("terminating flow has a cancellation context");
            if cleanup_failure && !terminating {
                self.record_suppressed_cleanup();
            }
            self.cancel_visible_children(env, context, span);
            FinallyMode::Protected(context)
        } else {
            FinallyMode::Ordinary
        }
    }

    /// The caller retains its synchronous CleanupGuard through this arbitration.
    /// Neither helper evaluates syntax or starts a nested execution machine.
    pub(super) fn finish_finally(
        &mut self,
        result: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        match mode {
            FinallyMode::Skip => unreachable!("skipped finally has no replacement"),
            FinallyMode::Protected(context) => {
                if !matches!(replacement, Ok(Flow::Continue)) {
                    self.record_suppressed_cleanup();
                }
                self.discard_flow(replacement, context, span);
                // No cleanup return, recoverable failure or panic may overwrite
                // the original cancellation/timeout cause.
                Err(self.termination_error(context, span))
            }
            FinallyMode::Ordinary => match replacement {
                Ok(Flow::Continue) => result,
                mut replacement => {
                    self.observe_termination(&replacement);
                    let context = self
                        .termination
                        .or_else(|| match &result {
                            Ok(flow) => ScopeExit::cleanup_context(flow),
                            Err(error) => error.scope_cleanup(),
                        })
                        .unwrap_or_else(|| CancellationContext::new(TerminationReason::Cancelled));
                    match &mut replacement {
                        Ok(flow) => ScopeExit::remember_cleanup(flow, context),
                        Err(error) => error.remember_scope_cleanup(context),
                    }
                    self.discard_flow(result, context, span);
                    replacement
                }
            },
        }
    }

    pub(super) fn observe_termination<T>(&mut self, result: &KuResult<T>) {
        if let Err(error) = result {
            if let Some(context) = error.runtime_termination() {
                self.latch_termination(context);
            }
        }
    }

    fn latch_termination(&mut self, context: RuntimeTermination) {
        crate::runtime::task::request_current_task_cancel_with(context);
        let context = current_task_cancellation().unwrap_or(context);
        match &mut self.termination {
            Some(original) => {
                original.cleanup_deadline = original.cleanup_deadline.min(context.cleanup_deadline);
            }
            None => self.termination = Some(context),
        }
        if let Some(context) = self.termination {
            set_execution_termination(context);
        }
    }

    pub(super) fn termination_error(&self, context: RuntimeTermination, span: Span) -> KuError {
        let mut error = KuError::termination(context, span);
        if context.reason == TerminationReason::TimedOut
            && self
                .execution_deadline
                .as_ref()
                .is_some_and(|state| state.timed_out)
        {
            // Keep the existing request-boundary mapping to HTTP 504. Internal
            // unwinding uses the typed marker, not this diagnostic string.
            error.message = HTTP_HANDLER_TIMEOUT_MESSAGE.into();
        }
        error
    }

    pub(super) fn poll_termination(&mut self, span: Span) -> KuResult<()> {
        let now = Instant::now();
        if let Some(context) = current_task_cancellation() {
            self.latch_termination(context);
        }
        if let Some(state) = &mut self.execution_deadline {
            if state.poll(now) {
                let cleanup_deadline = *state
                    .cleanup_deadline
                    .get_or_insert(now + HTTP_HANDLER_CLEANUP_GRACE);
                self.latch_termination(RuntimeTermination {
                    reason: TerminationReason::TimedOut,
                    cleanup_deadline,
                });
            }
        }
        let Some(mut context) = self.termination else {
            return Ok(());
        };
        if let Some(cleanup) = current_cleanup_context() {
            context.cleanup_deadline = context.cleanup_deadline.min(cleanup.cleanup_deadline);
            self.latch_termination(context);
            if now < context.cleanup_deadline {
                return Ok(());
            }
        }
        if now >= context.cleanup_deadline && !self.cleanup_timeout_recorded {
            self.cleanup_timeout_recorded = true;
            if let Some(runtime) = &self.task_runtime {
                runtime.record_cleanup_timeout();
            }
        }
        Err(self.termination_error(context, span))
    }

    pub(super) fn reject_cleanup_submission(&self, span: Span) -> KuResult<()> {
        if let Some(context) = current_cleanup_context().or(self.termination) {
            return Err(self.termination_error(context, span));
        }
        Ok(())
    }

    pub(super) fn record_suppressed_cleanup(&self) {
        if let Some(runtime) = &self.task_runtime {
            runtime.record_cleanup_suppressed();
        }
    }

    pub(super) fn cancel_visible_children(
        &self,
        env: &Env,
        context: RuntimeTermination,
        span: Span,
    ) {
        let mut handles = Vec::new();
        match env.all_owned_tasks(span) {
            Ok(owned) => handles.extend(owned),
            Err(_) => self.record_suppressed_cleanup(),
        }
        for observer in &self.caller_owners {
            if observer.collect_owned_tasks(&mut handles, span).is_err() {
                self.record_suppressed_cleanup();
            }
        }
        let mut requested = HashSet::new();
        for handle in &handles {
            if requested.insert(handle.id()) {
                handle.request_cancel_with(context);
            }
        }
    }

    fn scope_cleanup_context(
        &self,
        carried: Option<RuntimeTermination>,
    ) -> Option<RuntimeTermination> {
        // Preserve the existing cause precedence; every present context can
        // tighten the budget, even when an earlier context supplied the cause.
        [
            self.termination,
            carried,
            current_task_cancellation(),
            current_cleanup_context(),
            crate::runtime::task::current_execution_termination(),
        ]
        .into_iter()
        .flatten()
        .reduce(|mut first, next| {
            first.cleanup_deadline = first.cleanup_deadline.min(next.cleanup_deadline);
            first
        })
    }

    pub(super) fn finish_owned_scope<T: ScopeExit>(
        &mut self,
        env: &mut Env,
        result: KuResult<T>,
        span: Span,
        pop: bool,
    ) -> KuResult<T> {
        self.observe_termination(&result);
        let (handles, collected) = match env.current_scope_owned_tasks(span) {
            Ok(handles) => (handles, Ok(())),
            Err(error) => (Vec::new(), Err(error)),
        };
        self.finish_owned_resources(env, result, span, handles, collected, |env| {
            if pop {
                env.pop_scope();
            }
        })
    }

    pub(super) fn finish_call_scope(
        &mut self,
        mut env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        self.observe_termination(&result);
        let (handles, collected) = call_owned_tasks(Some(&env), &leases, &[], span);
        self.finish_owned_resources(&mut env, result, span, handles, collected, |env| {
            env.clear_call_frame();
            drop(leases);
        })
    }

    pub(super) fn finish_call_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        self.observe_termination(&result);
        let (handles, collected) = call_owned_tasks(None, &leases, &values, span);
        self.finish_owned_resources(env, result, span, handles, collected, |_| {
            // Restoring leases only restore the caller. No caller Env is dropped.
            drop(values);
            drop(leases);
        })
    }

    fn finish_owned_resources<T: ScopeExit>(
        &mut self,
        env: &mut Env,
        mut result: KuResult<T>,
        span: Span,
        handles: Vec<crate::runtime::task::TaskHandle>,
        collected: KuResult<()>,
        teardown: impl FnOnce(&mut Env),
    ) -> KuResult<T> {
        self.observe_termination(&result);
        let carried = match &result {
            Ok(flow) => flow.cleanup_context(),
            Err(error) => error.scope_cleanup(),
        };
        let mut cleanup_context = self.scope_cleanup_context(carried);
        if cleanup_context.is_none() && !handles.is_empty() {
            cleanup_context = Some(CancellationContext::new(TerminationReason::Cancelled));
        }
        // A local cleanup is not parent cancellation. The temporary guard also
        // covers nested owner/result drops and is restored on every return.
        let _cleanup_guard = cleanup_context.map(CleanupGuard::enter);
        let expired_before =
            cleanup_context.is_some_and(|context| Instant::now() >= context.cleanup_deadline);
        if let Some(context) = cleanup_context {
            match &mut result {
                Ok(flow) => flow.remember_cleanup(context),
                Err(error) => error.remember_scope_cleanup(context),
            }
        }
        let settled = if !handles.is_empty() {
            let context = cleanup_context.expect("owned scope cleanup has a context");
            // Request all visible children before waiting for any child.
            // Cancelling this scope is not cancellation of its continuing
            // parent; do not latch this context into the parent interpreter.
            if self.termination.is_some() {
                self.cancel_visible_children(env, context, span);
            }
            let settled = match &self.task_runtime {
                Some(runtime) => runtime
                    .cancel_handles_and_wait(&handles, context)
                    .map(|_| ()),
                None => {
                    for handle in &handles {
                        handle.request_cancel_with(context);
                    }
                    Ok(())
                }
            };
            // A readonly closure can retain a shared binding cell without
            // inheriting this scope's cleanup ownership. Release the logical
            // owner now; such observers may retain the ID, not the payload.
            let context = self
                .scope_cleanup_context(Some(context))
                .expect("scope cleanup retains its context");
            let _release_guard = CleanupGuard::enter(context);
            for handle in &handles {
                handle.release_scope_owner(context);
            }
            settled
        } else {
            Ok(())
        };
        // A later collection failure cannot skip the already-known owners.
        // Preserve the first cleanup error, as in the existing scope policy.
        if collected.is_err() && settled.is_err() {
            self.record_suppressed_cleanup();
        }
        let mut cleanup = collected.and(settled);
        // Even the final Task proxy/owner drop belongs to the cleanup window.
        // Function parameters would otherwise drop after the local guard.
        drop(handles);
        cleanup_context = self.scope_cleanup_context(cleanup_context);
        {
            let _pop_guard = cleanup_context.map(CleanupGuard::enter);
            teardown(env);
        }
        // A terminal Task or empty Task set does not prove its enclosing Value
        // cleanup finished in time. Do not skip necessary drops on expiry.
        cleanup_context = self.scope_cleanup_context(cleanup_context);
        if let Some(context) = cleanup_context {
            match &mut result {
                Ok(flow) => flow.remember_cleanup(context),
                Err(error) => error.remember_scope_cleanup(context),
            }
            if cleanup.is_ok() && (expired_before || Instant::now() >= context.cleanup_deadline) {
                cleanup = Err(KuError::termination(context, span));
            }
        }
        if matches!(&result, Err(error) if error.runtime_termination().is_some()) {
            if cleanup.is_err() {
                self.record_suppressed_cleanup();
            }
            return result;
        }
        if let Err(mut error) = cleanup {
            if let Some(context) = cleanup_context {
                self.discard_flow(result, context, span);
                cleanup_context = self.scope_cleanup_context(Some(context));
            }
            if let Some(context) = cleanup_context {
                error.remember_scope_cleanup(context);
            }
            if error.runtime_termination().is_some() {
                let mut error = KuError::structured(
                    crate::error::KuErrorKind::Runtime,
                    "task",
                    "shutdown_timeout",
                    "scope cleanup did not finish before the bounded cleanup deadline",
                    span,
                );
                if let Some(context) = cleanup_context {
                    error.remember_scope_cleanup(context);
                }
                return Err(error);
            }
            return Err(error);
        }
        result
    }

    pub(super) fn discard_flow<T: ScopeExit>(
        &self,
        result: KuResult<T>,
        context: RuntimeTermination,
        span: Span,
    ) {
        let guard = CleanupGuard::enter(context);
        if let Ok(flow) = &result {
            let mut tasks = Vec::new();
            if flow.collect_tasks(&mut tasks, span).is_ok() {
                if let Some(runtime) = &self.task_runtime {
                    if runtime.cancel_handles_and_wait(&tasks, context).is_err() {
                        self.record_suppressed_cleanup();
                    }
                } else {
                    for task in &tasks {
                        task.request_cancel_with(context);
                    }
                }
            } else {
                self.record_suppressed_cleanup();
            }
        }
        drop(result);
        drop(guard);
    }

    pub(super) fn take_task_field(&self, expr: &Expr, env: &Env) -> KuResult<Option<Value>> {
        // Static field paths have no index/call effects to reorder. Indexed
        // owned moves keep the checker's existing rejection contract.
        let mut path = Vec::new();
        let mut target = expr;
        while let ExprKind::Field {
            target: parent,
            name,
        } = &target.kind
        {
            path.push(ValueProjection::Field(name.clone()));
            target = parent;
        }
        if let ExprKind::Variable(root) = &target.kind {
            if env.contains(root) {
                path.reverse();
                return env.take_task_projection(root, &path, expr.span);
            }
        }
        Ok(None)
    }
}

fn call_owned_tasks(
    env: Option<&Env>,
    leases: &[BorrowLease],
    values: &[Value],
    span: Span,
) -> (Vec<crate::runtime::task::TaskHandle>, KuResult<()>) {
    let mut handles = Vec::new();
    let collected = (|| {
        if let Some(env) = env {
            handles.extend(env.current_scope_owned_tasks(span)?);
        }
        for lease in leases {
            lease.collect_temporary_owned_tasks(&mut handles, span)?;
        }
        for value in values {
            value.collect_owned_tasks(&mut handles, span)?;
        }
        Ok(())
    })();
    handles.sort_unstable_by_key(crate::runtime::task::TaskHandle::id);
    handles.dedup_by_key(|handle| handle.id());
    (handles, collected)
}
