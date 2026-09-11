use std::{
    cell::Cell,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{
    error::{KuError, KuResult, RuntimeTermination, TerminationReason},
    span::Span,
};

pub(crate) type CancellationContext = RuntimeTermination;

thread_local! {
    static CLEANUP_CONTEXT: Cell<Option<CancellationContext>> = const { Cell::new(None) };
    static EXECUTION_TERMINATION: Cell<Option<CancellationContext>> = const { Cell::new(None) };
}

impl RuntimeTermination {
    pub(crate) fn new(reason: TerminationReason) -> Self {
        let now = Instant::now();
        Self::with_deadline(
            reason,
            now.checked_add(Duration::from_secs(1)).unwrap_or(now),
        )
    }

    pub(crate) fn with_deadline(reason: TerminationReason, cleanup_deadline: Instant) -> Self {
        if let Some(inherited) = current_cleanup_context()
            .or_else(current_execution_termination)
            .or_else(super::current_task_cancellation)
        {
            Self {
                reason: inherited.reason,
                cleanup_deadline: inherited.cleanup_deadline.min(cleanup_deadline),
            }
        } else {
            Self {
                reason,
                cleanup_deadline,
            }
        }
    }
}

pub(crate) fn current_cleanup_context() -> Option<CancellationContext> {
    CLEANUP_CONTEXT.with(Cell::get)
}

pub(crate) fn current_execution_termination() -> Option<CancellationContext> {
    EXECUTION_TERMINATION.with(Cell::get)
}

pub(crate) fn set_execution_termination(context: CancellationContext) {
    EXECUTION_TERMINATION.with(|slot| {
        slot.set(Some(match slot.get() {
            Some(previous) => CancellationContext {
                reason: previous.reason,
                cleanup_deadline: previous.cleanup_deadline.min(context.cleanup_deadline),
            },
            None => context,
        }));
    });
}

/// A synchronous execution boundary, including HTTP handlers without a Task.
/// Entering is not cancellation: operations remain allowed until a termination
/// is latched. Nested scheduler help gets its own context and restores its caller.
pub(crate) struct ExecutionTerminationGuard {
    previous: Option<CancellationContext>,
    _same_thread: PhantomData<Rc<()>>,
}

impl ExecutionTerminationGuard {
    pub(crate) fn enter() -> Self {
        Self {
            previous: EXECUTION_TERMINATION.with(|slot| slot.replace(None)),
            _same_thread: PhantomData,
        }
    }
}

impl Drop for ExecutionTerminationGuard {
    fn drop(&mut self) {
        EXECUTION_TERMINATION.with(|slot| slot.set(self.previous));
    }
}

/// The guard only marks synchronous cleanup execution; it does not create a
/// task, wait, reset a deadline, or run user code from a destructor.
pub(crate) struct CleanupGuard {
    previous: Option<CancellationContext>,
    _same_thread: PhantomData<Rc<()>>,
}

impl CleanupGuard {
    pub(crate) fn enter(context: CancellationContext) -> Self {
        let previous = CLEANUP_CONTEXT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(match previous {
                Some(previous) => CancellationContext {
                    reason: previous.reason,
                    cleanup_deadline: previous.cleanup_deadline.min(context.cleanup_deadline),
                },
                None => context,
            }));
            previous
        });
        Self {
            previous,
            _same_thread: PhantomData,
        }
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        CLEANUP_CONTEXT.with(|slot| {
            let previous = self.previous.map(|mut previous| {
                if let Some(current) = slot.get() {
                    previous.cleanup_deadline =
                        previous.cleanup_deadline.min(current.cleanup_deadline);
                }
                previous
            });
            slot.set(previous);
        });
    }
}

pub(crate) fn ensure_task_operations_allowed(span: Span) -> KuResult<()> {
    match current_cleanup_context()
        .or_else(current_execution_termination)
        .or_else(super::current_task_cancellation)
    {
        Some(context) => Err(KuError::termination(context, span)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_guard_isolates_nested_reason_and_restores_outer_context() {
        let _outer = ExecutionTerminationGuard::enter();
        assert!(current_execution_termination().is_none());
        ensure_task_operations_allowed(Span::default()).unwrap();
        let now = Instant::now();
        let parent = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now + Duration::from_secs(1),
        };
        set_execution_termination(parent);
        {
            let _child = ExecutionTerminationGuard::enter();
            assert!(current_execution_termination().is_none());
            ensure_task_operations_allowed(Span::default()).unwrap();
            set_execution_termination(CancellationContext {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now,
            });
            assert_eq!(
                current_execution_termination().unwrap().reason,
                TerminationReason::Cancelled
            );
        }
        assert_eq!(current_execution_termination(), Some(parent));
        assert_eq!(
            ensure_task_operations_allowed(Span::default())
                .unwrap_err()
                .runtime_termination(),
            Some(parent)
        );
    }

    #[test]
    fn execution_termination_keeps_first_cause_and_shortest_absolute_deadline() {
        let _execution = ExecutionTerminationGuard::enter();
        let now = Instant::now();
        set_execution_termination(CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now + Duration::from_secs(1),
        });
        set_execution_termination(CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now,
        });
        set_execution_termination(CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now + Duration::from_secs(5),
        });
        let expected = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now,
        };
        assert_eq!(current_execution_termination(), Some(expected));
        assert_eq!(
            CancellationContext::new(TerminationReason::Cancelled),
            expected
        );
        assert!(current_cleanup_context().is_none());
    }
}
