use super::*;

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

    pub(super) fn finish_owned_scope<T: ScopeExit>(
        &mut self,
        env: &mut Env,
        mut result: KuResult<T>,
        span: Span,
        pop: bool,
    ) -> KuResult<T> {
        self.observe_termination(&result);
        let carried = match &result {
            Ok(flow) => flow.cleanup_context(),
            Err(error) => error.scope_cleanup(),
        };
        let mut cleanup_context = self
            .termination
            .or(carried)
            .or_else(current_task_cancellation)
            .or_else(current_cleanup_context);
        let handles = env.current_scope_owned_tasks(span);
        let cleanup = match handles {
            Ok(handles) if !handles.is_empty() => {
                let context = cleanup_context
                    .unwrap_or_else(|| CancellationContext::new(TerminationReason::Cancelled));
                cleanup_context = Some(context);
                match &mut result {
                    Ok(flow) => flow.remember_cleanup(context),
                    Err(error) => error.remember_scope_cleanup(context),
                }
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
                for handle in &handles {
                    handle.release_scope_owner(context);
                }
                settled
            }
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        if pop {
            env.pop_scope();
        }
        if matches!(&result, Err(error) if error.runtime_termination().is_some()) {
            if cleanup.is_err() {
                self.record_suppressed_cleanup();
            }
            return result;
        }
        if let Err(error) = cleanup {
            if let Some(context) = cleanup_context {
                self.discard_flow(result, context, span);
            }
            if error.runtime_termination().is_some() {
                let mut error = KuError::structured(
                    crate::error::KuErrorKind::Runtime,
                    "task",
                    "shutdown_timeout",
                    "owned child tasks did not finish before the bounded cleanup deadline",
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
