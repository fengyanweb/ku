//! One-shot await ownership and observation shared by synchronous adapters.
//!
//! Pending is an observation, not a scheduler suspension. The synchronous
//! driver still uses the existing helper and bounded Condvar waits. Result
//! locking and cancellation's actual queued-capture Drop can still block.

use super::*;
use std::{marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwaitPoll {
    Pending,
    Ready,
}

/// A selected String-result await receives only this unused part of its
/// caller's existing value budget. The caller keeps its input String
/// reservation: every admitted success buffer originates in that input pool.
#[derive(Clone, Copy)]
pub(crate) struct AwaitPayloadPolicy {
    pub(crate) max_count: usize,
    pub(crate) max_bytes: usize,
    pub(crate) input_string_capacity: usize,
    pub(crate) span: Span,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AwaitPayloadCharge {
    pub(crate) count: usize,
    pub(crate) bytes: usize,
    pub(crate) box_count: usize,
    pub(crate) box_bytes: usize,
    // Actual public Map capacity: one Value slot per Map entry. String key
    // headers are included in slot bytes, not counted as extra Value slots.
    pub(crate) error_leaf_count: usize,
    // Map slot backing plus all three actual key and value String buffers.
    pub(crate) error_leaf_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwaitPayloadRejection {
    ArithmeticOverflow,
    FrameLimit,
    StorageLimit,
    UnclassifiedPayload,
}

#[derive(Clone, Copy)]
pub(super) enum AwaitObservationError {
    Poisoned,
    Rejected(AwaitPayloadRejection),
}

impl AwaitPayloadRejection {
    fn into_error(self, span: Span) -> KuError {
        // This is called only after the result lock has been released. The
        // locked observation returns primitive metadata, never an allocation.
        KuError::runtime(
            format!("internal execution storage rejected: {self:?}"),
            span,
        )
    }
}

impl AwaitPayloadCharge {
    fn boxed(
        error_leaf_count: usize,
        error_leaf_bytes: usize,
    ) -> Result<Self, AwaitPayloadRejection> {
        let box_bytes = std::mem::size_of::<Value>();
        Ok(Self {
            count: error_leaf_count
                .checked_add(1)
                .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?,
            bytes: error_leaf_bytes
                .checked_add(box_bytes)
                .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?,
            box_count: 1,
            box_bytes,
            error_leaf_count,
            error_leaf_bytes,
        })
    }

    fn error_object(value: &Value) -> Result<Self, AwaitPayloadRejection> {
        let Value::Object(fields) = value else {
            return Err(AwaitPayloadRejection::UnclassifiedPayload);
        };
        let names = ["domain", "code", "message"];
        if fields.len() != names.len() {
            return Err(AwaitPayloadRejection::UnclassifiedPayload);
        }
        // Charge the actual public HashMap capacity, including spare slots,
        // and every actual key/value String buffer. Opaque raw hash-table
        // bucket/control layout and allocator overhead are not exposed by
        // this API and are not represented as an RSS measurement.
        let count = fields.capacity();
        let mut bytes = count
            .checked_mul(std::mem::size_of::<(String, Value)>())
            .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?;
        // Three fixed lookups avoid an O(capacity) iteration through a sparse
        // Map. No clone, allocation, payload Drop or caller callback runs here.
        for name in names {
            let Some((key, Value::String(text))) = fields.get_key_value(name) else {
                return Err(AwaitPayloadRejection::UnclassifiedPayload);
            };
            bytes = bytes
                .checked_add(key.capacity())
                .and_then(|bytes| bytes.checked_add(text.capacity()))
                .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?;
        }
        Self::boxed(count, bytes)
    }

    /// Describe a real selected leaf without allocating or cloning. A failure
    /// Object contributes its public Map capacity as Value slots, the tuple
    /// backing for those slots, and every actual key/value String capacity.
    /// A success String remains covered by the caller's input reservation.
    pub(crate) fn for_value(
        value: &Value,
        input_string_capacity: usize,
    ) -> Result<Self, AwaitPayloadRejection> {
        match value {
            Value::Result { ok: true, value } => match value.as_ref() {
                Value::String(text) if text.capacity() <= input_string_capacity => {
                    Self::boxed(0, 0)
                }
                Value::String(_) => Err(AwaitPayloadRejection::StorageLimit),
                _ => Err(AwaitPayloadRejection::UnclassifiedPayload),
            },
            Value::Result { ok: false, value } => Self::error_object(value),
            // A checked source returns Result, but retain the existing null
            // fallthrough of direct Interpreter callers that bypass checking.
            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => Ok(Self::default()),
            _ => Err(AwaitPayloadRejection::UnclassifiedPayload),
        }
    }
}

impl AwaitPayloadPolicy {
    fn check_charge(
        self,
        charge: AwaitPayloadCharge,
    ) -> Result<AwaitPayloadCharge, AwaitPayloadRejection> {
        if charge.count > self.max_count {
            return Err(AwaitPayloadRejection::FrameLimit);
        }
        if charge.bytes > self.max_bytes {
            return Err(AwaitPayloadRejection::StorageLimit);
        }
        Ok(charge)
    }

    pub(super) fn admit(
        self,
        result: &KuResult<Value>,
    ) -> Result<AwaitPayloadCharge, AwaitPayloadRejection> {
        let charge = match result {
            Ok(value) => AwaitPayloadCharge::for_value(value, self.input_string_capacity)?,
            Err(_) => AwaitPayloadCharge::default(),
        };
        self.check_charge(charge)
    }

    fn planned_task_error(
        self,
        code: &str,
        message_capacity: usize,
    ) -> Result<(), AwaitPayloadRejection> {
        let names = ["domain", "code", "message"];
        let mut bytes = names
            .len()
            .checked_mul(std::mem::size_of::<(String, Value)>())
            .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?;
        for name in names {
            bytes = bytes
                .checked_add(name.len())
                .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?;
        }
        bytes = bytes
            .checked_add("task".len())
            .and_then(|bytes| bytes.checked_add(code.len()))
            .and_then(|bytes| bytes.checked_add(message_capacity))
            .ok_or(AwaitPayloadRejection::ArithmeticOverflow)?;
        self.check_charge(AwaitPayloadCharge::boxed(names.len(), bytes)?)?;
        Ok(())
    }
}

pub(super) enum AwaitMode {
    Ordinary,
    Diagnostic(Duration),
    #[cfg(test)]
    AlreadyClaimed {
        deadline: Option<Instant>,
        consume: bool,
    },
}

trait AwaitTarget {
    const OWNS_HANDLE: bool;

    fn handle(&self) -> &TaskHandle;
}

impl AwaitTarget for TaskHandle {
    const OWNS_HANDLE: bool = true;

    fn handle(&self) -> &TaskHandle {
        self
    }
}

impl AwaitTarget for &TaskHandle {
    const OWNS_HANDLE: bool = false;

    fn handle(&self) -> &TaskHandle {
        self
    }
}

fn tighten(saved: &mut Option<CancellationContext>, incoming: Option<CancellationContext>) {
    if let Some(incoming) = incoming {
        match saved {
            Some(saved) => {
                saved.cleanup_deadline = saved.cleanup_deadline.min(incoming.cleanup_deadline);
            }
            None => *saved = Some(incoming),
        }
    }
}

fn visible_context() -> Option<CancellationContext> {
    current_cleanup_context()
        .or_else(current_execution_termination)
        .or_else(current_task_cancellation)
}

struct AwaitCore<H: AwaitTarget> {
    runtime: TaskRuntime,
    target: Option<H>,
    wait: Option<TaskWaitGuard>,
    current: i64,
    deadline: Option<Instant>,
    consume: bool,
    aborted: bool,
    // Ready stays latched until the sole result is transferred by finish.
    outcome: Option<KuResult<Value>>,
    // This policy travels with the Ticket, including LocalDriver's direct
    // polls outside the Machine. Charge describes only its actual outcome.
    payload_policy: Option<AwaitPayloadPolicy>,
    payload_charge: AwaitPayloadCharge,
    // Actual resource-release cause is independent of the result's metadata.
    // Only its deadline tightens; no TLS guard survives a poll boundary.
    release_context: Option<CancellationContext>,
    _same_thread: PhantomData<Rc<()>>,
}

impl<H: AwaitTarget> AwaitCore<H> {
    fn begin(runtime: TaskRuntime, target: H, mode: AwaitMode) -> Self {
        Self::begin_with_policy(runtime, target, mode, None)
    }

    fn begin_with_policy(
        runtime: TaskRuntime,
        target: H,
        mode: AwaitMode,
        payload_policy: Option<AwaitPayloadPolicy>,
    ) -> Self {
        // Install the owner before admission: every failure still reaches the
        // observer before an owned target is released.
        let mut core = Self {
            runtime,
            target: Some(target),
            wait: None,
            current: current_task_id(),
            deadline: None,
            consume: true,
            aborted: false,
            outcome: None,
            payload_policy,
            payload_charge: AwaitPayloadCharge::default(),
            release_context: visible_context(),
            _same_thread: PhantomData,
        };
        let admission = core.admit(mode);
        if let Err(error) = admission {
            core.ready(Err(error));
        }
        core
    }

    fn target(&self) -> &TaskHandle {
        self.target
            .as_ref()
            .expect("await owns its target")
            .handle()
    }

    fn admit(&mut self, mode: AwaitMode) -> KuResult<()> {
        match mode {
            AwaitMode::Ordinary => {
                ensure_task_operations_allowed(Span::default())?;
                if !self.target().state.claim_await() {
                    self.ready_task_error(
                        "already_awaited",
                        format!("task {} has already been awaited", self.target().id),
                    );
                    return Ok(());
                }
            }
            AwaitMode::Diagnostic(timeout) => {
                ensure_task_operations_allowed(Span::default())?;
                self.deadline = Some(Instant::now().checked_add(timeout).ok_or_else(|| {
                    KuError::runtime(
                        "task timeout is too large for this platform",
                        Span::default(),
                    )
                })?);
                // Preserve the existing internal non-consuming peek contract.
                self.consume = false;
            }
            #[cfg(test)]
            AwaitMode::AlreadyClaimed { deadline, consume } => {
                self.deadline = deadline;
                self.consume = consume;
            }
        }
        if self.current == self.target().id {
            self.ready_task_error(
                "self_await",
                format!("task {} cannot await itself", self.target().id),
            );
            return Ok(());
        }
        let state = if self.current == 0 {
            None
        } else {
            self.runtime.state(self.current)?
        };
        if self.current != 0 {
            if let Some(code) = self.runtime.register_wait(self.current, self.target().id)? {
                self.ready_task_error(
                    code,
                    format!(
                        "task {} cannot await task {}",
                        self.current,
                        self.target().id
                    ),
                );
                return Ok(());
            }
        }
        // Own retirement immediately after successful registration. This Arc
        // observes the caller state; it is not a logical Task owner lease.
        self.wait = Some(TaskWaitGuard {
            runtime: self.runtime.clone(),
            current: self.current,
            state,
        });
        if let Some(state) = &self.wait.as_ref().expect("registered wait").state {
            state.set_status(TASK_WAITING);
        }
        Ok(())
    }

    fn remember_release(&mut self) {
        if !H::OWNS_HANDLE {
            // Borrowed public admission must not inspect an unobserved target
            // or install owner-release context. Its caller retains the handle.
            return;
        }
        tighten(&mut self.release_context, visible_context());
        // A cancellation destructor may have tightened its Task's context
        // before unwinding out of the nested guard. Preserve the deadline,
        // without replacing this ticket's earlier resource-release cause.
        let cancellation = self.target().state.cancellation();
        tighten(&mut self.release_context, cancellation);
    }

    fn ready(&mut self, result: KuResult<Value>) {
        let charge = match self.payload_policy {
            Some(policy) => match policy.admit(&result) {
                Ok(charge) => charge,
                Err(rejection) => {
                    self.reject_payload(result, rejection, policy.span);
                    return;
                }
            },
            None => AwaitPayloadCharge::default(),
        };
        self.ready_admitted(result, charge);
    }

    fn ready_admitted(&mut self, result: KuResult<Value>, charge: AwaitPayloadCharge) {
        assert!(self.outcome.is_none(), "await outcome transfers once");
        self.remember_release();
        if let Err(error) = &result {
            tighten(&mut self.release_context, error.scope_cleanup());
        }
        self.payload_charge = charge;
        self.outcome = Some(result);
        // Original await_task_until retired this before returning to the
        // Interpreter observer. Keep target and outcome owned after retirement.
        drop(self.wait.take());
    }

    fn reject_payload<T>(&mut self, payload: T, rejection: AwaitPayloadRejection, span: Span) {
        self.remember_release();
        let guard = self.release_context.map(CleanupGuard::enter);
        drop(payload);
        self.remember_release();
        drop(guard);
        self.ready_admitted(
            Err(rejection.into_error(span)),
            AwaitPayloadCharge::default(),
        );
    }

    fn ready_task_error(&mut self, code: &str, message: String) {
        let Some(policy) = self.payload_policy else {
            self.ready(Ok(task_error(code, message)));
            return;
        };
        // Check the requested three-slot schema before constructing it, then
        // check its real allocator-reported capacities before constructing its
        // Box. This mirrors candidate-capacity admission for other backing.
        if let Err(rejection) = policy.planned_task_error(code, message.capacity()) {
            self.reject_payload(message, rejection, policy.span);
            return;
        }
        let object = errors::error_object("task", code, message);
        let charge = match AwaitPayloadCharge::error_object(&object)
            .and_then(|charge| policy.check_charge(charge))
        {
            Ok(charge) => charge,
            Err(rejection) => {
                self.reject_payload(object, rejection, policy.span);
                return;
            }
        };
        self.ready_admitted(
            Ok(Value::Result {
                ok: false,
                value: Box::new(object),
            }),
            charge,
        );
    }

    fn poll(&mut self) -> AwaitPoll {
        assert!(
            !self.aborted,
            "a native-panicked await ticket cannot resume"
        );
        assert_eq!(
            current_task_id(),
            self.current,
            "await resumes on its caller Task"
        );
        if self.outcome.is_some() {
            return AwaitPoll::Ready;
        }
        if let Some(context) = current_task_cancellation() {
            tighten(
                &mut self.release_context,
                visible_context().or(Some(context)),
            );
            // A short phase guard lets nested native drops feed back a shorter
            // deadline even on panic. There is no guard stored in the ticket.
            let guard = if H::OWNS_HANDLE {
                self.release_context.map(CleanupGuard::enter)
            } else {
                None
            };
            let cancellation = catch_unwind(AssertUnwindSafe(|| {
                self.target().request_cancel_with(context);
            }));
            self.remember_release();
            drop(guard);
            if let Err(panic) = cancellation {
                self.abort_native(panic);
            }
            self.ready(Err(KuError::termination(context, Span::default())));
            return AwaitPoll::Ready;
        }
        // Payload and terminal are observed under the same result lock. This
        // is a move for ordinary await, never a peek clone of an Owned value.
        let observed = match self.payload_policy {
            Some(policy) => match self.target().state.observe_result_admitted(policy) {
                Ok(observed) => Ok(observed),
                Err(AwaitObservationError::Poisoned) => Err(KuError::runtime(
                    "async task state is poisoned",
                    Span::default(),
                )),
                Err(AwaitObservationError::Rejected(rejection)) => {
                    Err(rejection.into_error(policy.span))
                }
            },
            None => self
                .target()
                .state
                .observe_result(self.consume)
                .map(|(result, terminal)| (result, terminal, AwaitPayloadCharge::default())),
        };
        match observed {
            Err(error) => self.ready(Err(error)),
            Ok((Some(result), _, charge)) => self.ready_admitted(result, charge),
            Ok((None, true, _)) => self.ready_task_error(
                "already_awaited",
                format!("task {} has already been awaited", self.target().id),
            ),
            Ok((None, false, _)) => {
                if self
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    self.ready_task_error(
                        "timeout",
                        format!("timed out waiting for task {}", self.target().id),
                    );
                } else if await_help_depth() >= MAX_AWAIT_DEPTH {
                    self.ready_task_error(
                        "await_depth",
                        format!(
                            "task {} exceeded await depth {MAX_AWAIT_DEPTH}",
                            self.current
                        ),
                    );
                } else {
                    self.remember_release();
                    return AwaitPoll::Pending;
                }
            }
        }
        AwaitPoll::Ready
    }

    fn abort_native(&mut self, panic: CleanupPanic) -> ! {
        self.remember_release();
        // Original synchronous await retired its wait when a real effect or
        // helper cleanup unwound. A retained private ticket is drop-only after
        // that native panic; it cannot resume without its wait edge, and does
        // not manufacture a normal Ready outcome from the panic.
        self.aborted = true;
        drop(self.wait.take());
        std::panic::resume_unwind(panic);
    }

    fn wait_sync_once(&mut self) {
        let helped = catch_unwind(AssertUnwindSafe(|| self.runtime.help_one_bounded()));
        let helped = match helped {
            Ok(helped) => helped,
            Err(panic) => {
                self.abort_native(panic);
            }
        };
        match helped {
            Ok(true) => return,
            Err(error) => {
                self.ready(Err(error));
                return;
            }
            Ok(false) => {}
        }
        self.wait_target_once();
    }

    fn wait_target_once(&mut self) {
        let wait = self
            .deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(2))
            })
            .unwrap_or(Duration::from_millis(2));
        if !wait.is_zero() {
            if let Err(error) = self.target().state.wait(wait) {
                self.ready(Err(error));
            }
        }
    }

    fn finish_with(mut self, observe: impl FnOnce(&KuResult<Value>)) -> KuResult<Value> {
        assert!(
            !self.aborted,
            "a native-panicked await ticket cannot finish"
        );
        assert!(self.outcome.is_some(), "only a Ready await can finish");
        assert!(
            self.wait.is_none(),
            "Ready retired its wait before observation"
        );
        if !H::OWNS_HANDLE {
            // The public adapters retain their original synchronous return.
            // No owned target is being released, so a new cleanup window here
            // could tighten ambient D even when admission was rejected.
            observe(self.outcome.as_ref().expect("sole await outcome"));
            drop(self.target.take());
            return self.outcome.take().expect("one borrowed await transfer");
        }
        self.remember_release();
        // Declare the window before the payload: unwind drops every actual
        // payload field while this same absolute context is still installed.
        let guard = self.release_context.map(CleanupGuard::enter);
        let mut payload = AwaitPayload {
            target: self.target.take(),
            outcome: self.outcome.take(),
            release_context: self.release_context,
        };
        let observation = catch_unwind(AssertUnwindSafe(|| {
            observe(payload.outcome.as_ref().expect("sole await outcome"));
        }));
        payload.remember_release();
        if let Err(panic) = observation {
            // Resume the original panic, so ordinary Rust drop glue runs the
            // payload under the still-live window. A second native destructor
            // panic during unwind retains Rust's abort boundary.
            std::panic::resume_unwind(panic);
        }
        // Interpreter observation is complete before the unique target owner
        // can start actual resource release. Result remains protected if that
        // release itself panics, and it is moved out only after release returns.
        drop(payload.target.take());
        payload.remember_release();
        if H::OWNS_HANDLE {
            if let (Some(Err(error)), Some(context)) =
                (&mut payload.outcome, payload.release_context)
            {
                // Keep result metadata's original cause, while carrying any
                // shorter deadline established by observer or actual owner Drop
                // to the caller's later cleanup. Borrowed diagnostic adapters
                // retain their original result metadata unchanged.
                error.remember_scope_cleanup(context);
            }
        }
        let result = payload.outcome.take().expect("one await result transfer");
        drop(payload);
        drop(guard);
        result
    }
}

impl<H: AwaitTarget> Drop for AwaitCore<H> {
    fn drop(&mut self) {
        if self.target.is_none() {
            return;
        }
        if !H::OWNS_HANDLE {
            drop(self.wait.take());
            drop((self.target.take(), self.outcome.take()));
            return;
        }
        self.remember_release();
        drop(self.wait.take());
        let _guard = self.release_context.map(CleanupGuard::enter);
        // Move both owners before either native destructor executes. The
        // remaining tuple field's unwind Drop is still inside this window.
        drop((self.target.take(), self.outcome.take()));
    }
}

struct AwaitPayload<H: AwaitTarget> {
    target: Option<H>,
    outcome: Option<KuResult<Value>>,
    release_context: Option<CancellationContext>,
}

impl<H: AwaitTarget> AwaitPayload<H> {
    fn remember_release(&mut self) {
        tighten(&mut self.release_context, visible_context());
        if let Some(target) = &self.target {
            tighten(
                &mut self.release_context,
                target.handle().state.cancellation(),
            );
        }
    }
}

impl<H: AwaitTarget> Drop for AwaitPayload<H> {
    fn drop(&mut self) {
        self.remember_release();
        let _guard = self.release_context.map(CleanupGuard::enter);
        drop((self.target.take(), self.outcome.take()));
    }
}

pub(crate) struct AwaitTicket {
    core: AwaitCore<TaskHandle>,
}

impl AwaitTicket {
    pub(super) fn runtime(&self) -> &TaskRuntime {
        &self.core.runtime
    }

    pub(super) fn wait_once(&mut self) {
        self.core.wait_target_once();
    }

    pub(super) fn helper_error(&mut self, error: KuError) {
        self.core.ready(Err(error));
    }

    pub(super) fn abort_native(&mut self, panic: CleanupPanic) -> ! {
        self.core.abort_native(panic)
    }

    pub(crate) fn begin(handle: TaskHandle) -> Self {
        let runtime = handle.runtime.clone();
        Self {
            core: AwaitCore::begin(runtime, handle, AwaitMode::Ordinary),
        }
    }

    pub(crate) fn begin_with_payload_policy(
        handle: TaskHandle,
        policy: AwaitPayloadPolicy,
    ) -> Self {
        let runtime = handle.runtime.clone();
        Self {
            core: AwaitCore::begin_with_policy(runtime, handle, AwaitMode::Ordinary, Some(policy)),
        }
    }

    pub(crate) fn retained_payload_charge(&self) -> AwaitPayloadCharge {
        if self.core.outcome.is_some() {
            self.core.payload_charge
        } else {
            AwaitPayloadCharge::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn test_outcome(&self) -> Option<&KuResult<Value>> {
        self.core.outcome.as_ref()
    }

    pub(crate) fn poll(&mut self) -> AwaitPoll {
        self.core.poll()
    }

    pub(crate) fn drive_sync(&mut self) {
        // Owned production and borrowed public adapters share this one loop.
        drive_sync(self);
    }

    pub(crate) fn finish_with(self, observe: impl FnOnce(&KuResult<Value>)) -> KuResult<Value> {
        self.core.finish_with(observe)
    }
}

pub(super) fn await_borrowed(
    runtime: TaskRuntime,
    handle: &TaskHandle,
    mode: AwaitMode,
) -> KuResult<Value> {
    let mut core = AwaitCore::begin(runtime, handle, mode);
    drive_sync(&mut core);
    core.finish_with(|_| {})
}

trait AwaitDriver {
    fn poll(&mut self) -> AwaitPoll;
    fn wait_sync_once(&mut self);
}

fn drive_sync(driver: &mut impl AwaitDriver) {
    while driver.poll() == AwaitPoll::Pending {
        driver.wait_sync_once();
    }
}

impl<H: AwaitTarget> AwaitDriver for AwaitCore<H> {
    fn poll(&mut self) -> AwaitPoll {
        AwaitCore::poll(self)
    }

    fn wait_sync_once(&mut self) {
        AwaitCore::wait_sync_once(self);
    }
}

impl AwaitDriver for AwaitTicket {
    fn poll(&mut self) -> AwaitPoll {
        AwaitTicket::poll(self)
    }

    fn wait_sync_once(&mut self) {
        self.core.wait_sync_once();
    }
}
