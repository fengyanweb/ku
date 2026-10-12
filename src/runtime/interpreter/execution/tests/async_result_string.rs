use super::*;
use crate::runtime::task::{LocalTaskRunner, RetirementBatchPoll, TaskHandle, TaskRetirementBatch};
use std::ops::ControlFlow;
use std::sync::mpsc;

const WAIT: Duration = Duration::from_secs(3);

const TEXT: &str = "\u{4fdd}\u{7559}\0async";
const SIMPLE: &str = "async fn Entry(text: str): str! { return ok(text) }";
const RELAY: &str = r#"
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return Relay(text, n - 1)
}
async fn Entry(text: str, n: int): str! { return Relay(text, n) }
"#;

#[derive(Clone, Copy, Debug)]
struct Buffer {
    pointer: usize,
    capacity: usize,
    text: &'static str,
}

fn string_input(text: &'static str, capacity: usize) -> (Value, Buffer) {
    let mut value = String::with_capacity(capacity);
    value.push_str(text);
    let buffer = Buffer {
        pointer: value.as_ptr() as usize,
        capacity: value.capacity(),
        text,
    };
    (Value::String(value), buffer)
}

fn result_buffer(value: &Value, buffers: &[Buffer]) -> (usize, usize) {
    let Value::Result { ok: true, value } = value else {
        panic!("the admitted outcome must be a real success Result: {value:?}");
    };
    let Value::String(text) = value.as_ref() else {
        panic!("the real Result leaf must retain an input String");
    };
    let index = buffers
        .iter()
        .position(|buffer| buffer.pointer == text.as_ptr() as usize)
        .expect("the actual input allocation must survive");
    assert_eq!(text.capacity(), buffers[index].capacity);
    assert_eq!(text, buffers[index].text);
    (index, value.as_ref() as *const Value as usize)
}

fn observe(machine: &Machine, buffers: &[Buffer], boxes: &mut [Option<usize>]) -> usize {
    let mut pointers = Vec::new();
    let mut observe = |value: &Value| {
        if matches!(value, Value::Result { .. }) {
            let (index, pointer) = result_buffer(value, buffers);
            if let Some(previous) = boxes[index] {
                assert_eq!(pointer, previous, "an actual saved Box must move intact");
            } else {
                boxes[index] = Some(pointer);
            }
            assert!(
                !pointers.contains(&pointer),
                "one actual owning location per Box"
            );
            pointers.push(pointer);
        }
    };
    for value in machine.values.as_slice() {
        observe(value);
    }
    if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &machine.pending {
        observe(value);
    }
    for exit in machine.exits.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) =
            exit.state.outcome()
        {
            observe(value);
        }
    }
    for frame in machine.finally.frames.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &frame.primary {
            observe(value);
        }
    }
    let count = pointers.len();
    assert_eq!(machine.result_wrapper_capacity, count);
    assert_eq!(
        machine.result_wrapper_bytes,
        count * std::mem::size_of::<Value>()
    );
    assert_eq!(machine.metrics.retained_result_wrapper_capacity, count);
    assert_eq!(
        machine.metrics.retained_result_wrapper_bytes,
        count * std::mem::size_of::<Value>()
    );
    count
}

fn drained(machine: &Machine) {
    assert_drained(&machine.metrics, 0);
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert_eq!(
        (
            machine.result_wrapper_capacity,
            machine.result_wrapper_bytes
        ),
        (0, 0)
    );
    assert_eq!(machine.string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_result_wrapper_capacity, 0);
    assert_eq!(machine.metrics.retained_result_wrapper_bytes, 0);
    assert_eq!((machine.lease_capacity, machine.lease_root_bytes), (0, 0));
    assert!(machine.entry_args.is_none());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    None,
    Running,
    Produced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    WrapperCount,
    WrapperBytes,
    NativeBox,
}

#[derive(Clone, Copy)]
struct Plan {
    chunk: usize,
    pause: Pause,
    fault: Fault,
    selection_only: bool,
}

impl Plan {
    fn normal(chunk: usize) -> Self {
        Self {
            chunk,
            pause: Pause::None,
            fault: Fault::None,
            selection_only: false,
        }
    }
}

#[derive(Debug)]
struct Finished {
    steps: usize,
    calls: usize,
    peak_calls: usize,
    finally: usize,
    peak_boxes: usize,
    peak_box_bytes: usize,
    peak_strings: usize,
    peak_args: usize,
    outgoing: Option<(usize, usize)>,
}

#[derive(Debug)]
enum Witness {
    Entered {
        task: i64,
        selected: bool,
        legacy: bool,
        pending_error: bool,
        steps: usize,
        calls: usize,
    },
    Paused {
        stage: Pause,
        steps: usize,
        outgoing: Option<(usize, usize)>,
    },
    Wrap {
        steps: usize,
        input: Buffer,
    },
    Native {
        input: Buffer,
        boxed: usize,
    },
    Finished(Finished),
}

struct SourceRunner {
    inner: ClaimedNamedTask,
    plan: Plan,
    buffers: Vec<Buffer>,
    boxes: Vec<Option<usize>>,
    witness: mpsc::Sender<Witness>,
    release: Option<mpsc::Receiver<()>>,
    paused: bool,
    faulted: bool,
    dispatches: usize,
    thread: std::thread::ThreadId,
}

impl SourceRunner {
    fn pause(&mut self, stage: Pause, outgoing: Option<(usize, usize)>) {
        self.paused = true;
        self.witness
            .send(Witness::Paused {
                stage,
                steps: self.inner.interpreter.steps,
                outgoing,
            })
            .unwrap();
        self.release
            .take()
            .expect("the controlled release receiver")
            .recv_timeout(WAIT)
            .expect("the bounded fixture must release its actual claimant");
    }
}

impl LocalTaskRunner for SourceRunner {
    fn dispatch(&mut self, _budget: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        self.dispatches += 1;
        assert!(
            self.dispatches <= 100_000,
            "the finite source must make progress"
        );
        assert_eq!(std::thread::current().id(), self.thread);
        if self.plan.selection_only {
            return ControlFlow::Break(Ok(Value::Null));
        }
        if !self.faulted {
            if let Some(machine) = self.inner.machine.as_mut() {
                let at_wrap = machine.pending.is_none()
                    && machine.exits.len() == 0
                    && matches!(
                        machine
                            .work
                            .len()
                            .checked_sub(1)
                            .and_then(|i| machine.work.get(i)),
                        Some(Work::WrapTerminalOk(_, true))
                    );
                if at_wrap && matches!(self.plan.fault, Fault::WrapperCount | Fault::WrapperBytes) {
                    let Value::String(text) = machine.values.get(machine.values.len() - 1).unwrap()
                    else {
                        panic!("the actual operand has already moved before wrapper admission")
                    };
                    let input = self.buffers[0];
                    assert_eq!(
                        (text.as_ptr() as usize, text.capacity()),
                        (input.pointer, input.capacity)
                    );
                    assert_eq!(machine.result_wrapper_capacity, 0);
                    assert!(self.inner.interpreter.steps > 0);
                    self.witness
                        .send(Witness::Wrap {
                            steps: self.inner.interpreter.steps,
                            input,
                        })
                        .unwrap();
                    if self.plan.fault == Fault::WrapperCount {
                        machine.value_limits.max_frames = machine.values.capacity()
                            + machine.argument_capacity
                            + machine.exit_handle_capacity
                            + machine.retirement_capacity
                            + machine.lease_capacity
                            + machine.lease_root_bytes / std::mem::size_of::<Value>()
                            + machine.result_wrapper_capacity;
                    } else {
                        machine.value_limits.max_storage_bytes =
                            machine.values.charged_storage_bytes()
                                + machine.argument_bytes
                                + machine.exit_handle_bytes
                                + machine.retirement_bytes
                                + machine.string_payload_bytes
                                + machine.lease_bytes
                                + machine.lease_root_bytes
                                + machine.result_wrapper_bytes
                                + std::mem::size_of::<Value>()
                                - 1;
                    }
                    self.faulted = true;
                }
                if self.plan.fault == Fault::NativeBox && machine.result_wrapper_capacity > 0 {
                    let count = observe(machine, &self.buffers, &mut self.boxes);
                    assert_eq!(count, 1);
                    self.witness
                        .send(Witness::Native {
                            input: self.buffers[0],
                            boxed: self.boxes[0].unwrap(),
                        })
                        .unwrap();
                    // A real native runner abandonment, not a source destructor
                    // or an invented Task retirement acknowledgement.
                    let _cleanup =
                        CleanupGuard::enter(CancellationContext::new(TerminationReason::Cancelled));
                    let _abandoned = self.inner.machine.take().unwrap();
                    panic!("actual async String Result kernel abandonment");
                }
            }
        }
        let result = self
            .inner
            .dispatch(NonZeroUsize::new(self.plan.chunk).unwrap());
        if let Some(machine) = self.inner.machine.as_ref() {
            observe(machine, &self.buffers, &mut self.boxes);
        }
        if let ControlFlow::Break(outcome) = &result {
            let outgoing = outcome.as_ref().ok().and_then(|value| {
                if matches!(value, Value::Result { ok: true, .. }) {
                    let outgoing = result_buffer(value, &self.buffers);
                    if let Some(previous) = self.boxes[outgoing.0] {
                        assert_eq!(
                            outgoing.1, previous,
                            "the outgoing child Box must be the real owner"
                        );
                    }
                    Some(outgoing)
                } else {
                    None
                }
            });
            let machine = self.inner.machine.as_ref();
            if let Some(machine) = machine {
                drained(machine);
            }
            assert_eq!(self.inner.interpreter.call_depth, 0);
            self.witness
                .send(Witness::Finished(Finished {
                    steps: self.inner.interpreter.steps,
                    calls: machine.map_or(0, |m| m.metrics.entered_calls),
                    peak_calls: machine.map_or(0, |m| m.metrics.peak_calls),
                    finally: machine.map_or(0, |m| m.metrics.entered_finally_frames),
                    peak_boxes: machine.map_or(0, |m| m.metrics.peak_result_wrapper_capacity),
                    peak_box_bytes: machine.map_or(0, |m| m.metrics.peak_result_wrapper_bytes),
                    peak_strings: machine.map_or(0, |m| m.metrics.peak_string_payload_bytes),
                    peak_args: machine.map_or(0, |m| m.peak_argument_bytes),
                    outgoing,
                }))
                .unwrap();
            if self.plan.pause == Pause::Produced && !self.paused {
                self.pause(Pause::Produced, outgoing);
            }
        } else if self.plan.pause == Pause::Running
            && !self.paused
            && self.inner.interpreter.steps > 0
        {
            self.pause(Pause::Running, None);
        }
        result
    }

    fn waiting(&mut self) -> Option<&mut crate::runtime::task::AwaitTicket> {
        self.inner.waiting()
    }
    fn cleanup_wait_once(&mut self) {
        self.inner.cleanup_wait_once();
    }
}

fn spawn_source(
    runtime: &TaskRuntime,
    source: &str,
    args: Vec<Value>,
    buffers: Vec<Buffer>,
    plan: Plan,
) -> (TaskHandle, mpsc::Receiver<Witness>, mpsc::Sender<()>) {
    let mut producer = production_interpreter(source);
    producer.task_runtime = Some(runtime.clone());
    let function = producer.functions["Entry"].clone();
    let child_runtime = runtime.clone();
    let (tx, rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            let template = producer.template();
            move || {
                assert!(crate::runtime::task::current_cleanup_context().is_none());
                assert!(crate::runtime::task::current_execution_termination().is_none());
                let mut host = Interpreter::from_template(template, child_runtime);
                host.async_execution = true;
                let inner = ClaimedNamedTask::new(host, function, args, Span::default());
                let selected = inner.machine.is_some();
                if let Some((_, args)) = &inner.legacy {
                    let mut strings = args.iter().filter_map(|value| {
                        if let Value::String(text) = value {
                            Some(text)
                        } else {
                            None
                        }
                    });
                    for buffer in &buffers {
                        let text = strings
                            .next()
                            .expect("unsupported selection retains each original input");
                        assert_eq!(
                            (text.as_ptr() as usize, text.capacity()),
                            (buffer.pointer, buffer.capacity)
                        );
                        assert_eq!(text, buffer.text);
                    }
                    assert!(strings.next().is_none());
                }
                tx.send(Witness::Entered {
                    task: crate::runtime::task::current_task_id(),
                    selected,
                    legacy: inner.legacy.is_some(),
                    pending_error: inner
                        .machine
                        .as_ref()
                        .is_some_and(|m| matches!(&m.pending, Some(Err(_))))
                        || inner.outcome.as_ref().is_some_and(Result::is_err),
                    steps: inner.interpreter.steps,
                    calls: inner
                        .machine
                        .as_ref()
                        .map_or(0, |m| m.metrics.entered_calls),
                })
                .unwrap();
                SourceRunner {
                    inner,
                    plan,
                    boxes: vec![None; buffers.len()],
                    buffers,
                    witness: tx,
                    release: Some(release_rx),
                    paused: false,
                    faulted: false,
                    dispatches: 0,
                    thread: std::thread::current().id(),
                }
            }
        })
        .unwrap();
    (task, rx, release_tx)
}

fn entered(task: &TaskHandle, rx: &mpsc::Receiver<Witness>, selected: bool, pending: bool) {
    let Witness::Entered {
        task: id,
        selected: actual,
        legacy,
        pending_error,
        steps,
        calls,
    } = rx.recv_timeout(WAIT).unwrap()
    else {
        panic!("the actual claimant must report its constructor")
    };
    assert_ne!(id, 0);
    assert_eq!(id, task.id());
    assert_eq!(actual, selected);
    assert_eq!(legacy, !selected);
    assert_eq!(pending_error, pending);
    assert_eq!(steps, 0);
    assert_eq!(calls, usize::from(selected && !pending));
}

fn finished(rx: &mpsc::Receiver<Witness>) -> Finished {
    let Witness::Finished(finished) = rx.recv_timeout(WAIT).unwrap() else {
        panic!("the actual runner must finish and drain its source machine")
    };
    finished
}

fn retired(mut batch: TaskRetirementBatch) {
    let end = Instant::now() + WAIT;
    loop {
        match batch.poll(Instant::now()) {
            RetirementBatchPoll::Retired => return,
            RetirementBatchPoll::TimedOut(context) => {
                panic!("the actual G1/G2 receipts missed their original D: {context:?}")
            }
            RetirementBatchPoll::Pending { .. } => {
                assert!(Instant::now() < end, "finite actual retirement progress");
                std::thread::yield_now();
            }
        }
    }
}

fn release_task(task: TaskHandle, context: CancellationContext) {
    let witness = task.retirement_witness();
    drop(task);
    retired(TaskRetirementBatch::from_reserved(vec![witness], context));
}

fn close(runtime: &TaskRuntime) {
    runtime.cancel_all_and_wait(WAIT).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
}

#[test]
fn production_named_async_string_result_returns_the_original_buffer_through_real_task() {
    // Safe shallow production RED: no symbols added by this async slice.
    let source = "async fn Entry(text: str): str! { return ok(text) }";
    let runtime = TaskRuntime::new();
    let mut interpreter = production_interpreter(source);
    interpreter.task_runtime = Some(runtime.clone());
    let mut text = String::with_capacity(4096);
    text.push_str("\u{4fdd}\u{7559}\0async");
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    let Value::Task(task) = interpreter
        .call_function("Entry", vec![Value::String(text)], Span::default(), 0)
        .unwrap()
    else {
        panic!("a production async call must return a real Task");
    };
    let result = task.await_result();
    drop(task);
    runtime.cancel_all_and_wait(Duration::from_secs(3)).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    let Value::Result { ok: true, value } = result.unwrap() else {
        panic!("the actual child must return its success Result");
    };
    let Value::String(text) = *value else {
        panic!("the actual task result must retain the input String")
    };
    assert_eq!(
        text.as_ptr(),
        pointer,
        "legacy async ok cloned the original String buffer"
    );
    assert_eq!(text.capacity(), capacity);
    assert_eq!(text, "\u{4fdd}\u{7559}\0async");
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn actual_async_claimant_moves_one_box_and_string_across_chunks_and_512_calls() {
    assert_eq!(MAX_CALL_DEPTH, 512);
    for chunk in [1, 7, 256] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(TEXT, 4096);
        let (task, rx, _release) = spawn_source(
            &runtime,
            RELAY,
            vec![text, Value::Int(510)],
            vec![buffer],
            Plan::normal(chunk),
        );
        entered(&task, &rx, true, false);
        let proof = finished(&rx);
        let result = task.await_result().unwrap();
        assert_eq!(result_buffer(&result, &[buffer]), proof.outgoing.unwrap());
        assert_eq!(proof.calls, 512);
        assert_eq!(proof.peak_calls, 512);
        assert!(proof.steps > 0);
        assert_eq!(proof.peak_boxes, 1);
        assert_eq!(proof.peak_box_bytes, std::mem::size_of::<Value>());
        assert_eq!(proof.peak_strings, buffer.capacity);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

#[test]
fn production_async_root_and_sync_helpers_keep_first_513_error_and_healthy_next_task() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) = spawn_source(
        &runtime,
        RELAY,
        vec![text, Value::Int(511)],
        vec![buffer],
        Plan::normal(7),
    );
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    let error = task.await_result().unwrap_err();
    assert_eq!(error.domain.as_deref(), Some("runtime"));
    assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
    assert_eq!((proof.calls, proof.peak_calls), (512, 512));
    assert_eq!(proof.peak_boxes, 0);
    assert!(proof.outgoing.is_none());
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) =
        spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], Plan::normal(1));
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    assert_eq!(
        result_buffer(&task.await_result().unwrap(), &[buffer]),
        proof.outgoing.unwrap()
    );
    assert_eq!((proof.calls, proof.peak_boxes), (1, 1));
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
    // The public constructor uses the same real source path, including the
    // async root in the existing logical call limit.
    for n in [510, 511] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut host = production_interpreter(RELAY);
        host.task_runtime = Some(runtime.clone());
        let (text, buffer) = string_input(TEXT, 4096);
        let Value::Task(task) = host
            .call_function("Entry", vec![text, Value::Int(n)], Span::default(), 0)
            .unwrap()
        else {
            panic!("real Task")
        };
        let result = task.await_result();
        if n == 510 {
            result_buffer(&result.unwrap(), &[buffer]);
        } else {
            assert_eq!(
                result.unwrap_err().code.as_deref(),
                Some("call_depth_exceeded")
            );
        }
        assert_eq!(host.call_depth, 0);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

#[test]
fn actual_async_input_capacity_rejects_before_body_and_wrong_arity_keeps_legacy_first_cause() {
    let limits = Limits::default();
    // The third case admits and takes the original Vec, then fails the real
    // operand backing reservation before any parameter/body can enter.
    for stage in 0..3 {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(
            TEXT,
            if stage != 0 {
                4096
            } else {
                limits.values.max_storage_bytes + 1
            },
        );
        let mut args = Vec::with_capacity(match stage {
            0 => 1,
            1 => limits.values.max_frames + 1,
            2 => limits.values.max_frames,
            _ => unreachable!(),
        });
        args.push(text);
        let (task, rx, _release) =
            spawn_source(&runtime, SIMPLE, args, vec![buffer], Plan::normal(1));
        entered(&task, &rx, true, true);
        let proof = finished(&rx);
        let error = task.await_result().unwrap_err();
        assert!(
            error.message.contains(if stage != 0 {
                "FrameLimit"
            } else {
                "StorageLimit"
            }),
            "{error:?}"
        );
        assert_eq!((proof.steps, proof.calls, proof.peak_boxes), (0, 0, 0));
        assert_eq!(
            proof.peak_args,
            if stage == 2 {
                limits.values.max_frames * std::mem::size_of::<Value>()
            } else {
                0
            }
        );
        assert_eq!(
            proof.peak_strings,
            if stage == 2 { buffer.capacity } else { 0 }
        );
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
    for missing in [false, true] {
        let source = if missing {
            "async fn Entry(text: str, other: str): str! { return ok(text) }"
        } else {
            SIMPLE
        };
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(TEXT, limits.values.max_storage_bytes + 1);
        let mut args = vec![text];
        if !missing {
            args.push(Value::Int(1));
        }
        let (task, rx, _release) =
            spawn_source(&runtime, source, args, vec![buffer], Plan::normal(1));
        entered(&task, &rx, false, false);
        let proof = finished(&rx);
        let error = task.await_result().unwrap_err();
        assert_eq!(
            error.message,
            if missing {
                "function 'Entry' expects 2 arguments but got 1"
            } else {
                "function 'Entry' expects 1 arguments but got 2"
            }
        );
        assert_eq!((proof.steps, proof.calls, proof.peak_boxes), (0, 0, 0));
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
        // Also assert the public producer/call/ordinary-await path, rather
        // than inferring production behavior from the instrumented claimant.
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut host = production_interpreter(source);
        host.task_runtime = Some(runtime.clone());
        let (text, _) = string_input(TEXT, limits.values.max_storage_bytes + 1);
        let args = if missing {
            vec![text]
        } else {
            vec![text, Value::Int(1)]
        };
        let Value::Task(task) = host
            .call_function("Entry", args, Span::default(), 0)
            .unwrap()
        else {
            panic!("real wrong-arity Task")
        };
        assert_eq!(task.await_result().unwrap_err().message, error.message);
        assert_eq!(host.steps, 0);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

#[test]
fn actual_async_wrapper_count_and_bytes_reject_after_move_before_real_box_allocation() {
    for fault in [Fault::WrapperCount, Fault::WrapperBytes] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(TEXT, 4096);
        let mut plan = Plan::normal(1);
        plan.fault = fault;
        let (task, rx, _release) = spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], plan);
        entered(&task, &rx, true, false);
        let Witness::Wrap { steps, input } = rx.recv_timeout(WAIT).unwrap() else {
            panic!("real pre-Box checkpoint")
        };
        assert!(steps > 0);
        assert_eq!(
            (input.pointer, input.capacity),
            (buffer.pointer, buffer.capacity)
        );
        let proof = finished(&rx);
        let error = task.await_result().unwrap_err();
        assert!(
            error.message.contains(if fault == Fault::WrapperCount {
                "FrameLimit"
            } else {
                "StorageLimit"
            }),
            "{error:?}"
        );
        assert_eq!(
            (proof.calls, proof.peak_boxes, proof.peak_box_bytes),
            (1, 0, 0)
        );
        assert_eq!(proof.peak_strings, buffer.capacity);
        assert!(proof.outgoing.is_none());
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

#[test]
fn actual_async_finally_keeps_two_real_boxes_and_later_argument_error_drains_without_replay() {
    const REPLACE: &str = r#"
async fn Entry(first: str, second: str, fallback: str): str! {
    try { return ok(first) } finally { return ok(second) }
    return ok(fallback)
}
"#;
    for chunk in [1, 7, 256] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (first, a) = string_input("\u{4e2d}\0first", 1024);
        let (second, b) = string_input(TEXT, 4096);
        let (fallback, c) = string_input("\0fallback", 512);
        let buffers = vec![a, b, c];
        let (task, rx, _release) = spawn_source(
            &runtime,
            REPLACE,
            vec![first, second, fallback],
            buffers.clone(),
            Plan::normal(chunk),
        );
        entered(&task, &rx, true, false);
        let proof = finished(&rx);
        assert_eq!(
            result_buffer(&task.await_result().unwrap(), &buffers),
            proof.outgoing.unwrap()
        );
        assert_eq!(proof.outgoing.unwrap().0, 1);
        assert_eq!(proof.finally, 1);
        assert_eq!(proof.peak_boxes, 2);
        assert_eq!(proof.peak_box_bytes, 2 * std::mem::size_of::<Value>());
        assert_eq!(proof.peak_strings, a.capacity + b.capacity + c.capacity);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
    const BROKEN: &str = r#"
fn Keep(text: str, n: int): str { return text }
async fn Entry(text: str): str! { return ok(Keep(text, 1 / 0)) }
"#;
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) =
        spawn_source(&runtime, BROKEN, vec![text], vec![buffer], Plan::normal(1));
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    assert!(task
        .await_result()
        .unwrap_err()
        .message
        .contains("division by zero"));
    assert_eq!((proof.calls, proof.peak_boxes), (1, 0));
    assert!(proof.steps > 0);
    assert_eq!(proof.peak_strings, buffer.capacity);
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
}

#[test]
fn actual_async_peek_retains_old_clone_contract_and_second_ordinary_await_is_rejected() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) =
        spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], Plan::normal(7));
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    let end = Instant::now() + WAIT;
    while task.status() != "completed" {
        assert!(Instant::now() < end, "finite real completion readiness");
        std::thread::yield_now();
    }
    let Value::Result { ok: true, value } = task.await_timeout(Duration::ZERO).unwrap() else {
        panic!("diagnostic peek")
    };
    let Value::String(peek) = value.as_ref() else {
        panic!("peek String")
    };
    assert_eq!(peek, TEXT);
    assert_ne!(
        peek.as_ptr() as usize,
        buffer.pointer,
        "the original nonconsuming peek intentionally clones"
    );
    assert_ne!(
        value.as_ref() as *const Value as usize,
        proof.outgoing.unwrap().1
    );
    let result = task.await_result().unwrap();
    assert_eq!(result_buffer(&result, &[buffer]), proof.outgoing.unwrap());
    let Value::Result { ok: false, value } = task.await_result().unwrap() else {
        panic!("second ordinary await must return its existing error Result")
    };
    let Value::Object(error) = *value else {
        panic!("existing task error object")
    };
    assert_eq!(
        error.get("code"),
        Some(&Value::String("already_awaited".to_owned()))
    );
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
}

#[test]
fn actual_async_queued_running_and_uncommitted_result_cancellation_retire_real_g1_g2() {
    // Keep the sole worker occupied so this target's original factory and
    // String remain in the actual queue, then retire its cancelled G2 node.
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (held_tx, held_rx) = mpsc::channel();
    let held = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            held_rx.recv_timeout(WAIT).expect("finite held claimant");
            Ok(Value::Null)
        })
        .unwrap();
    ready_rx.recv_timeout(WAIT).unwrap();
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) =
        spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], Plan::normal(1));
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 1);
    let context = CancellationContext::new(TerminationReason::TimedOut);
    assert!(task.request_cancel_with(context));
    assert!(
        matches!(
            rx.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "the unclaimed actual factory must be released"
    );
    assert_eq!(
        task.await_result().unwrap_err().runtime_termination(),
        Some(context)
    );
    release_task(task, context);
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 0);
    held_tx.send(()).unwrap();
    assert_eq!(held.await_result().unwrap(), Value::Null);
    release_task(held, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);

    // The produced checkpoint holds the real completed Result only on the
    // runner's Rust stack: TaskState has not accepted the incoming payload.
    for stage in [Pause::Running, Pause::Produced] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(TEXT, 4096);
        let mut plan = Plan::normal(1);
        plan.pause = stage;
        let (task, rx, release) = spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], plan);
        entered(&task, &rx, true, false);
        let before = if stage == Pause::Produced {
            Some(finished(&rx))
        } else {
            None
        };
        let Witness::Paused {
            stage: actual,
            steps,
            outgoing,
        } = rx.recv_timeout(WAIT).unwrap()
        else {
            panic!("controlled actual claimant checkpoint")
        };
        assert_eq!(actual, stage);
        assert!(steps > 0);
        if let Some(before) = before {
            assert_eq!(before.peak_boxes, 1);
            assert_eq!(outgoing, before.outgoing);
            assert!(outgoing.is_some());
        } else {
            assert!(outgoing.is_none());
        }
        let context = CancellationContext::new(TerminationReason::TimedOut);
        assert!(
            task.request_cancel_with(context),
            "cancellation must win before real result commitment"
        );
        release.send(()).unwrap();
        assert_eq!(
            task.await_result().unwrap_err().runtime_termination(),
            Some(context)
        );
        if stage == Pause::Running {
            let proof = finished(&rx);
            assert_eq!(proof.peak_boxes, 0);
            assert!(proof.outgoing.is_none());
        }
        let witness = task.retirement_witness();
        drop(task);
        let batch = TaskRetirementBatch::from_reserved(vec![witness], context);
        assert_eq!(
            batch.context(),
            context,
            "retirement keeps the same absolute D and first cause"
        );
        retired(batch);
        close(&runtime);
    }
}

#[test]
fn actual_async_owner_release_before_late_result_needs_completion_and_queue_receipts() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    plan.pause = Pause::Produced;
    let (task, rx, release) = spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], plan);
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    assert_eq!(proof.peak_boxes, 1);
    let Witness::Paused {
        stage: Pause::Produced,
        outgoing,
        ..
    } = rx.recv_timeout(WAIT).unwrap()
    else {
        panic!("actual uncommitted result")
    };
    assert_eq!(outgoing, proof.outgoing);
    let witness = task.retirement_witness();
    drop(task); // The real last owner, without retaining a Task clone.
    let mut batch = TaskRetirementBatch::from_reserved(
        vec![witness],
        CancellationContext::new(TerminationReason::Cancelled),
    );
    assert!(matches!(batch.poll(Instant::now()), RetirementBatchPoll::Pending { .. }),
        "owner release cannot acknowledge the actual still-held incoming Result or running queue node");
    release.send(()).unwrap();
    retired(batch);
    close(&runtime);
}

#[test]
fn actual_async_native_box_abandonment_releases_real_task_and_restores_claimant_tls() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    plan.fault = Fault::NativeBox;
    let (task, rx, _release) = spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], plan);
    entered(&task, &rx, true, false);
    let Witness::Native { input, boxed } = rx.recv_timeout(WAIT).unwrap() else {
        panic!("real Box before native abandonment")
    };
    assert_eq!(
        (input.pointer, input.capacity),
        (buffer.pointer, buffer.capacity)
    );
    assert_ne!(boxed, 0);
    let Value::Result { ok: false, value } = task.await_result().unwrap() else {
        panic!("the actual local driver preserves its panic classification")
    };
    let Value::Object(error) = *value else {
        panic!("existing task panic error object")
    };
    assert_eq!(error.get("code"), Some(&Value::String("panic".to_owned())));
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    // One fixed worker means the following actual claimant is the same OS
    // worker. Its factory asserts restored cleanup/execution TLS before entry.
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) =
        spawn_source(&runtime, SIMPLE, vec![text], vec![buffer], Plan::normal(1));
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    assert_eq!(
        result_buffer(&task.await_result().unwrap(), &[buffer]),
        proof.outgoing.unwrap()
    );
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
}

#[test]
fn async_string_result_whole_tree_rejections_preserve_original_legacy_inputs_and_cache_roles() {
    for source in [
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return await Child(text) }",
        "async fn Entry(text: str): str! { return await Entry(text) }",
        // The root is already cached as async: its first child call must
        // reject a synchronous dependency role before the later await node.
        "async fn Entry(text: str): str! { child = Entry(text)\nreturn await child }",
        "fn Leaf(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return ok(Leaf(text)?) }",
        "async fn Entry(text: str): str! { result = ok(text)\nreturn result }",
        "fn Pass(value: str!): str! { return value }\nasync fn Entry(text: str): str! { return Pass(ok(text)) }",
        "async fn Entry(text: str): str! { return err(text) }",
        "async fn Entry(text: str): str! { try { marker = 1 } catch (e) { marker = 2 }\nreturn ok(text) }",
        "fn Read(&text: str): int { return text.len() }\nasync fn Entry(text: str): str! { length = Read(text)\nreturn ok(text) }",
        "async fn Entry(text: str): str! { fn Local(value: str): str! { return ok(value) }\nreturn Local(text) }",
        // Result-bearing dependencies cannot become plain String helpers.
        "fn Leaf(text: str): str! { return ok(text) }\nfn Keep(text: str): str { try { return Leaf(text)? } finally { marker = 1 }\nreturn \"fallback\" }\nasync fn Entry(text: str, spare: str): str! { first = Keep(text)\nreturn Leaf(spare) }",
    ] {
        let source_functions = functions(source);
        let function = &source_functions["Entry"];
        assert!(!code::eligible_claimed_async_owned_result_string_entry(&source_functions, function, Limits::default()), "{source}");
        assert!(matches!(CodeHandle::compile_claimed_async_owned_result_string_entry(&source_functions, function, Limits::default()), Err(AdmissionError::Unsupported(_))), "{source}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut args = Vec::new();
        let mut buffers = Vec::new();
        for _ in &function.params {
            let (text, buffer) = string_input(TEXT, 4096);
            args.push(text);
            buffers.push(buffer);
        }
        let mut plan = Plan::normal(1);
        plan.selection_only = true;
        let (task, rx, _release) = spawn_source(&runtime, source, args, buffers, plan);
        entered(&task, &rx, false, false);
        assert_eq!(task.await_result().unwrap(), Value::Null);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
    // A declared String with an actual primitive stays on the old path. Its
    // existing body type failure must match direct legacy execution.
    const LENGTH: &str = "async fn Entry(text: str): str! { length = text.len()\nreturn ok(text) }";
    let source = functions(LENGTH);
    let mut legacy = production_interpreter(LENGTH);
    let expected = legacy
        .call_function_direct(
            source["Entry"].clone(),
            vec![Value::Int(1)],
            Span::default(),
            0,
        )
        .unwrap_err();
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (task, rx, _release) = spawn_source(
        &runtime,
        LENGTH,
        vec![Value::Int(1)],
        vec![],
        Plan::normal(1),
    );
    entered(&task, &rx, false, false);
    let proof = finished(&rx);
    let actual = task.await_result().unwrap_err();
    assert_eq!(
        (actual.message, actual.code),
        (expected.message, expected.code)
    );
    assert_eq!(proof.steps, legacy.steps);
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
    let source = functions(SIMPLE);
    assert!(!code::eligible_owned_named(
        &source,
        &source["Entry"],
        Limits::default()
    ));
    assert!(!code::eligible_borrowed_string_named(
        &source,
        &source["Entry"],
        Limits::default()
    ));
    assert!(!code::eligible_claimed_async_entry(
        &source,
        &source["Entry"],
        Limits::default()
    ));
}
