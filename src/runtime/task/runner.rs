//! Claimant-local execution beneath the existing Task job lifecycle.
//!
//! More is an immediate same-thread continuation, not a scheduler suspension.
//! Submission, helper depth, cancellation, completion and queue retirement
//! retain their existing production arbitration and panic boundaries.

use super::{AwaitTicket, KuResult, Value};
use std::{marker::PhantomData, num::NonZeroUsize, ops::ControlFlow, rc::Rc};

pub(crate) trait LocalTaskRunner {
    fn dispatch(&mut self, budget: NonZeroUsize) -> ControlFlow<KuResult<Value>>;

    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        None
    }
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
