//! Claimant-local execution beneath the existing Task job lifecycle.
//!
//! More is an immediate same-thread continuation, not a scheduler suspension.
//! Submission, helper depth, cancellation, completion and queue retirement
//! retain their existing production arbitration and panic boundaries.

use super::{KuResult, Value};
use std::{marker::PhantomData, num::NonZeroUsize, ops::ControlFlow, rc::Rc};

pub(crate) trait LocalTaskRunner {
    fn dispatch(&mut self, budget: NonZeroUsize) -> ControlFlow<KuResult<Value>>;
}

pub(super) fn drive_local(mut runner: impl LocalTaskRunner) -> KuResult<Value> {
    let budget = NonZeroUsize::new(256).expect("positive internal dispatch chunk budget");
    loop {
        match runner.dispatch(budget) {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(result) => return result,
        }
    }
    // The local runner drops before returning to execute_task_job's original
    // callback catch, while its Task identity and execution context are live.
}

pub(crate) struct OnceTaskRunner<F> {
    run: Option<F>,
    _same_thread: PhantomData<Rc<()>>,
}

impl<F> OnceTaskRunner<F> {
    pub(crate) fn new(run: F) -> Self {
        Self {
            run: Some(run),
            _same_thread: PhantomData,
        }
    }
}

impl<F: FnOnce() -> KuResult<Value>> LocalTaskRunner for OnceTaskRunner<F> {
    fn dispatch(&mut self, _budget: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        let run = self.run.take().expect("one-shot task runner executes once");
        ControlFlow::Break(run())
    }
}
