use super::*;
use crate::runtime::task::{AwaitPayloadCharge, AwaitPayloadPolicy, AwaitPoll, AwaitTicket};

const AWAIT_CHILD: &str = "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return await Child(text) }";
const ERR_AWAIT_CHILD: &str = "fn Leaf(text: str): str! { return err(text) }\nasync fn Child(text: str): str! { return Leaf(text) }\nasync fn Entry(text: str): str! { return await Child(text) }";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Payload {
    boxed: usize,
    string: Option<(usize, usize)>,
    map_buffers: [(usize, usize); 6],
    map_entries: [usize; 3],
    error_count: usize,
    error_bytes: usize,
    scalar: Option<ScalarLeaf>,
}

fn payload_opt(value: &Value) -> Option<Payload> {
    let Value::Result { ok, value } = value else {
        return None;
    };
    let boxed = value.as_ref() as *const Value as usize;
    if *ok {
        let (string, scalar) = if let Value::String(text) = value.as_ref() {
            (Some((text.as_ptr() as usize, text.capacity())), None)
        } else {
            (None, Some(scalar_leaf(value)?))
        };
        Some(Payload {
            boxed,
            string,
            scalar,
            map_buffers: [(0, 0); 6],
            map_entries: [0; 3],
            error_count: 0,
            error_bytes: 0,
        })
    } else {
        let Value::Object(fields) = value.as_ref() else {
            return None;
        };
        if fields.len() != 3 {
            return None;
        }
        let error_count = fields.capacity();
        let mut error_bytes = error_count * std::mem::size_of::<(String, Value)>();
        let mut map_buffers = [(0, 0); 6];
        let mut map_entries = [0; 3];
        for (index, name) in ["domain", "code", "message"].into_iter().enumerate() {
            let (key, Value::String(text)) = fields.get_key_value(name)? else {
                return None;
            };
            error_bytes += key.capacity() + text.capacity();
            map_entries[index] = key as *const String as usize;
            map_buffers[index * 2] = (key.as_ptr() as usize, key.capacity());
            map_buffers[index * 2 + 1] = (text.as_ptr() as usize, text.capacity());
        }
        Some(Payload {
            boxed,
            string: None,
            scalar: None,
            map_buffers,
            map_entries,
            error_count,
            error_bytes,
        })
    }
}

fn payload(value: &Value) -> Payload {
    payload_opt(value).expect("independently measured selected Result shape")
}

fn slot_payload(task: &TaskHandle) -> Option<Payload> {
    // Only Copy primitive metadata crosses this real lock boundary. Shape
    // assertions and all pointer comparisons occur after the guard releases.
    task.test_with_result_slot(|slot| {
        slot.and_then(|result| result.as_ref().ok())
            .and_then(payload_opt)
    })
    .unwrap()
}

fn charge(actual: Payload) -> AwaitPayloadCharge {
    let box_bytes = std::mem::size_of::<Value>();
    AwaitPayloadCharge {
        count: 1 + actual.error_count,
        bytes: box_bytes + actual.error_bytes,
        box_count: 1,
        box_bytes,
        error_leaf_count: actual.error_count,
        error_leaf_bytes: actual.error_bytes,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AwaitFault {
    None,
    CountBeforeTake,
    BytesBeforeTake,
    NativeReady,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AwaitPause {
    None,
    Waiting,
    Ready,
}

struct AwaitOptions {
    chunk: usize,
    fault: AwaitFault,
    pause: AwaitPause,
    replacement: Option<TaskHandle>,
    source_fault: Fault,
}

impl AwaitOptions {
    fn normal(chunk: usize) -> Self {
        Self {
            chunk,
            fault: AwaitFault::None,
            pause: AwaitPause::None,
            replacement: None,
            source_fault: Fault::None,
        }
    }
}

enum AwaitWitness {
    Waiting,
    Ticket {
        actual: Payload,
        cached: AwaitPayloadCharge,
    },
    FreshRetired,
    Child(crate::runtime::task::TaskRetirementWitness),
    ReadyNative(Payload),
    ReadyPaused(Payload),
    CancelTargetReleased(CancellationContext),
}

struct AwaitRunner {
    inner: SourceRunner,
    options: AwaitOptions,
    events: mpsc::Sender<AwaitWitness>,
    release: mpsc::Receiver<()>,
    paused: bool,
    adjusted: bool,
    yielded_span: Option<SpanId>,
}

fn base_storage(machine: &Machine) -> (usize, usize) {
    (
        machine.values.capacity()
            + machine.argument_capacity
            + machine.exit_handle_capacity
            + machine.retirement_capacity
            + machine.lease_capacity
            + machine.lease_root_bytes / std::mem::size_of::<Value>(),
        machine.values.charged_storage_bytes()
            + machine.argument_bytes
            + machine.exit_handle_bytes
            + machine.retirement_bytes
            + machine.string_payload_bytes
            + machine.lease_bytes
            + machine.lease_root_bytes,
    )
}

fn reject_all_value_budget_entries(machine: &mut Machine, reason: &str) {
    let span = Span::default();
    let spare = machine.values.capacity() - machine.values.len();
    assert!(machine
        .reserve_values(spare, span)
        .unwrap_err()
        .message
        .contains(reason));
    assert!(machine
        .check_argument_capacity(0, span)
        .unwrap_err()
        .message
        .contains(reason));
    assert!(machine
        .check_exit_storage_capacity(0, 0, span)
        .unwrap_err()
        .message
        .contains(reason));
    assert!(machine
        .check_lease_capacity(0, 0, span)
        .unwrap_err()
        .message
        .contains(reason));
}

impl LocalTaskRunner for AwaitRunner {
    fn dispatch(&mut self, budget: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        // Private controlled scheduling: after the real child helper retires,
        // the ordinary driver would let Machine::AwaitResume poll and consume
        // inline. Yield once per terminal await source span so the existing
        // LocalDriver waiting branch performs the actual poll first, leaving
        // its real Ready ticket observable on the following dispatch. This
        // adds no Machine tick, payload, charge or acknowledgement, and is not
        // a claim about ordinary production scheduling or performance. Each
        // runner owns one Task; terminal returns execute each source await at
        // most once, and finally replacements have distinct source spans.
        let waiting_span = self.inner.inner.machine.as_ref().and_then(|machine| {
            if machine.state != MachineState::Waiting {
                return None;
            }
            machine.awaiting.as_ref().and_then(|frame| {
                if frame.ticket.test_outcome().is_none() {
                    Some(frame.span)
                } else {
                    None
                }
            })
        });
        if let Some(span) = waiting_span {
            if self.yielded_span != Some(span) {
                self.yielded_span = Some(span);
                return ControlFlow::Continue(());
            }
        }
        if !self.adjusted
            && (self.options.replacement.is_some() || self.options.fault != AwaitFault::None)
        {
            self.inner.plan.chunk = 1;
        }
        if let Some(machine) = self.inner.inner.machine.as_mut() {
            let at_operand = machine.pending.is_none()
                && machine.exits.len() == 0
                && matches!(
                    machine
                        .work
                        .len()
                        .checked_sub(1)
                        .and_then(|i| machine.work.get(i)),
                    Some(Work::AwaitOwnedStringOperand(_))
                );
            if at_operand && !self.adjusted {
                if let Some(replacement) = self.options.replacement.take() {
                    let index = machine.values.len() - 1;
                    let original =
                        std::mem::replace(machine.values.get_mut(index).unwrap(), Value::Null);
                    let Value::Task(original) = original else {
                        panic!("replace only the actual fresh static Child Task operand")
                    };
                    let witness = original.retirement_witness();
                    drop(original);
                    retired(TaskRetirementBatch::from_reserved(
                        vec![witness],
                        CancellationContext::new(TerminationReason::Cancelled),
                    ));
                    *machine.values.get_mut(index).unwrap() = Value::Task(replacement);
                    self.events.send(AwaitWitness::FreshRetired).unwrap();
                } else if let Some(Value::Task(child)) =
                    machine.values.get(machine.values.len() - 1)
                {
                    self.events
                        .send(AwaitWitness::Child(child.retirement_witness()))
                        .unwrap();
                }
                if matches!(
                    self.options.fault,
                    AwaitFault::CountBeforeTake | AwaitFault::BytesBeforeTake
                ) {
                    // Reserve the actual future operand backing first, as the
                    // production operation does, so failure reaches the ticket
                    // pre-take policy rather than an earlier Vec reservation.
                    machine.reserve_values(1, Span::default()).unwrap();
                    let (count, bytes) = base_storage(machine);
                    if self.options.fault == AwaitFault::CountBeforeTake {
                        machine.value_limits.max_frames = count;
                    } else {
                        machine.value_limits.max_storage_bytes = bytes;
                    }
                }
                self.adjusted = true;
            }
            if let Some(Ok(value)) = machine
                .awaiting
                .as_ref()
                .and_then(|frame| frame.ticket.test_outcome())
            {
                let actual = payload(value);
                if let Some(owner) = primitive_payload(value) {
                    if !self
                        .inner
                        .primitives
                        .iter()
                        .any(|old| old.boxed == owner.boxed)
                    {
                        self.inner.last_primitive_candidate = Some(owner);
                        self.inner.primitives.push(owner);
                    }
                }
                let measured = charge(actual);
                let ticket = &machine.awaiting.as_ref().unwrap().ticket;
                assert_eq!(
                    ticket.retained_payload_charge(),
                    measured,
                    "the ticket reports independently measured actual backing"
                );
                let cached = machine.await_payload_snapshot;
                // LocalDriver may poll directly without a Machine dispatch.
                // A budget entry must include that actual ticket even if its
                // stable fields and metrics still contain the previous cache.
                let (base_count, base_bytes) = base_storage(machine);
                let stable_count =
                    machine.result_wrapper_capacity + machine.error_leaf_capacity - cached.count;
                let stable_bytes =
                    machine.result_wrapper_bytes + machine.error_leaf_bytes - cached.bytes;
                let saved = machine.value_limits;
                machine.value_limits.max_frames = base_count + stable_count + measured.count;
                machine.value_limits.max_storage_bytes = base_bytes + stable_bytes + measured.bytes;
                let spare = machine.values.capacity() - machine.values.len();
                machine.reserve_values(spare, Span::default()).unwrap();
                machine.check_argument_capacity(0, Span::default()).unwrap();
                machine
                    .check_exit_storage_capacity(0, 0, Span::default())
                    .unwrap();
                machine.check_lease_capacity(0, 0, Span::default()).unwrap();
                machine.value_limits.max_frames = base_count + stable_count + measured.count - 1;
                reject_all_value_budget_entries(machine, "FrameLimit");
                machine.value_limits = saved;
                machine.value_limits.max_storage_bytes =
                    base_bytes + stable_bytes + measured.bytes - 1;
                reject_all_value_budget_entries(machine, "StorageLimit");
                machine.value_limits = saved;
                self.events
                    .send(AwaitWitness::Ticket { actual, cached })
                    .unwrap();
                if self.options.pause == AwaitPause::Ready && !self.paused {
                    self.paused = true;
                    self.events.send(AwaitWitness::ReadyPaused(actual)).unwrap();
                    self.release
                        .recv_timeout(WAIT)
                        .expect("finite actual Ready ticket checkpoint");
                }
                if self.options.fault == AwaitFault::NativeReady {
                    self.events.send(AwaitWitness::ReadyNative(actual)).unwrap();
                    let _cleanup =
                        CleanupGuard::enter(CancellationContext::new(TerminationReason::Cancelled));
                    let _abandoned = self.inner.inner.machine.take().unwrap();
                    panic!("actual owned String await ticket kernel abandonment");
                }
            }
        }
        let cancelled_ticket = self
            .inner
            .inner
            .machine
            .as_ref()
            .and_then(|machine| machine.awaiting.as_ref())
            .and_then(|frame| frame.ticket.test_outcome())
            .and_then(|outcome| outcome.as_ref().err())
            .and_then(KuError::runtime_termination);
        let result = self.inner.dispatch(budget);
        if let Some(context) = cancelled_ticket {
            if self
                .inner
                .inner
                .machine
                .as_ref()
                .is_some_and(|machine| machine.awaiting.is_none())
            {
                self.events
                    .send(AwaitWitness::CancelTargetReleased(context))
                    .unwrap();
            }
        }
        if self.options.pause == AwaitPause::Waiting
            && !self.paused
            && self
                .inner
                .inner
                .machine
                .as_ref()
                .is_some_and(|m| m.state == MachineState::Waiting)
        {
            self.paused = true;
            self.events.send(AwaitWitness::Waiting).unwrap();
            self.release
                .recv_timeout(WAIT)
                .expect("finite actual waiting checkpoint");
        }
        result
    }
    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        self.inner.waiting()
    }
    fn cleanup_wait_once(&mut self) {
        self.inner.cleanup_wait_once();
    }
}

fn spawn_await(
    runtime: &TaskRuntime,
    source: &str,
    args: Vec<Value>,
    buffers: Vec<Buffer>,
    options: AwaitOptions,
) -> (
    TaskHandle,
    mpsc::Receiver<Witness>,
    mpsc::Receiver<AwaitWitness>,
    mpsc::Sender<()>,
) {
    let mut producer = production_interpreter(source);
    producer.task_runtime = Some(runtime.clone());
    let function = producer.functions["Entry"].clone();
    let child_runtime = runtime.clone();
    let (tx, rx) = mpsc::channel();
    let (events, events_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let (_unused_release, inner_release) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            let template = producer.template();
            move || {
                assert!(crate::runtime::task::current_cleanup_context().is_none());
                assert!(crate::runtime::task::current_execution_termination().is_none());
                let mut host = Interpreter::from_template(template, child_runtime);
                host.async_execution = true;
                let inner = ClaimedNamedTask::new(host, function, args, Span::default());
                assert!(inner.machine.is_some() && inner.legacy.is_none());
                tx.send(Witness::Entered {
                    task: crate::runtime::task::current_task_id(),
                    selected: true,
                    legacy: false,
                    pending_error: false,
                    steps: inner.interpreter.steps,
                    calls: inner.machine.as_ref().unwrap().metrics.entered_calls,
                })
                .unwrap();
                AwaitRunner {
                    inner: SourceRunner {
                        inner,
                        plan: Plan {
                            fault: options.source_fault,
                            ..Plan::normal(options.chunk)
                        },
                        boxes: vec![None; buffers.len()],
                        errors: Vec::new(),
                        primitives: Vec::new(),
                        last_primitive_candidate: None,
                        buffers,
                        witness: tx,
                        release: Some(inner_release),
                        paused: false,
                        faulted: false,
                        dispatches: 0,
                        thread: std::thread::current().id(),
                    },
                    options,
                    events,
                    release: release_rx,
                    paused: false,
                    adjusted: false,
                    yielded_span: None,
                }
            }
        })
        .unwrap();
    (task, rx, events_rx, release)
}

fn ticket_events(rx: &mpsc::Receiver<AwaitWitness>) -> Vec<Payload> {
    rx.try_iter()
        .filter_map(|event| match event {
            AwaitWitness::Ticket { actual, cached } => {
                assert_eq!(
                    cached,
                    AwaitPayloadCharge::default(),
                    "real LocalDriver direct poll precedes stable Machine synchronization"
                );
                Some(actual)
            }
            AwaitWitness::Child(witness) => {
                retired(TaskRetirementBatch::from_reserved(
                    vec![witness],
                    CancellationContext::new(TerminationReason::Cancelled),
                ));
                None
            }
            AwaitWitness::FreshRetired => None,
            _ => panic!("unexpected controlled await event"),
        })
        .collect()
}

#[test]
fn production_terminal_await_string_result_returns_original_buffer_through_real_child_task() {
    // Same safe old-API lineage as the fixed-S1 RED run: only the source
    // adds a fresh static child and a terminal await, with no deep-stack probe.
    let source = "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return await Child(text) }";
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
        panic!("a production async call must return a real Task")
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
        panic!("the actual child must return its success Result")
    };
    let Value::String(text) = *value else {
        panic!("the actual task result must retain the input String")
    };
    assert_eq!(
        text.as_ptr(),
        pointer,
        "legacy terminal await cloned the original String buffer"
    );
    assert_eq!(text.capacity(), capacity);
    assert_eq!(text, "\u{4fdd}\u{7559}\0async");
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn actual_fresh_static_terminal_await_moves_child_box_and_input_buffer_through_chunks() {
    for chunk in [1, 7, 256] {
        let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
        let (text, buffer) = string_input(TEXT, 4096);
        let (task, rx, events, _release) = spawn_await(
            &runtime,
            AWAIT_CHILD,
            vec![text],
            vec![buffer],
            AwaitOptions::normal(chunk),
        );
        entered(&task, &rx, true, false);
        let proof = finished(&rx);
        let result = task.await_result().unwrap();
        let (index, boxed) = result_buffer(&result, &[buffer]);
        assert_eq!(proof.outgoing, Some((index, boxed)));
        let tickets = ticket_events(&events);
        assert_eq!(
            tickets.len(),
            1,
            "actual LocalDriver direct poll must transfer one real child outcome"
        );
        assert_eq!(tickets[0].boxed, boxed);
        assert_eq!(tickets[0].string, Some((buffer.pointer, buffer.capacity)));
        assert_eq!(
            (proof.calls, proof.peak_boxes, proof.peak_errors),
            (1, 1, 0)
        );
        assert_eq!(proof.peak_strings, buffer.capacity);
        assert_eq!(proof.peak_box_bytes, std::mem::size_of::<Value>());
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

#[test]
fn real_single_worker_self_and_mutual_string_await_keep_64_and_first_65_boundaries() {
    const SELF: &str = r#"
async fn Entry(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return await Entry(text, n - 1)
}
"#;
    const MUTUAL: &str = r#"
async fn Other(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return await Entry(text, n - 1)
}
async fn Entry(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return await Other(text, n - 1)
}
"#;
    assert_eq!(crate::runtime::task::MAX_AWAIT_DEPTH, 64);
    for source in [SELF, MUTUAL] {
        for n in [64, 65] {
            let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
            let (text, buffer) = string_input(TEXT, 4096);
            let (task, rx, events, _release) = spawn_await(
                &runtime,
                source,
                vec![text, Value::Int(n)],
                vec![buffer],
                AwaitOptions::normal(1),
            );
            entered(&task, &rx, true, false);
            let proof = finished(&rx);
            let result = task.await_result().unwrap();
            let tickets = ticket_events(&events);
            assert_eq!(
                tickets.len(),
                1,
                "the instrumented root sees its own one actual child ticket"
            );
            if n == 64 {
                let (_, boxed) = result_buffer(&result, &[buffer]);
                assert_eq!(tickets[0].boxed, boxed);
                assert_eq!(proof.peak_errors, 0);
            } else {
                let actual = payload(&result);
                assert_eq!(actual, tickets[0]);
                let Value::Result { ok: false, value } = &result else {
                    panic!("existing await depth failure Result")
                };
                let Value::Object(fields) = value.as_ref() else {
                    panic!("existing task ErrorObject")
                };
                assert!(
                    matches!(fields.get("code"), Some(Value::String(code)) if code == "await_depth")
                );
                assert_eq!(
                    (proof.peak_errors, proof.peak_error_bytes),
                    (actual.error_count, actual.error_bytes)
                );
            }
            assert_eq!((proof.calls, proof.peak_calls, proof.peak_boxes), (1, 1, 1));
            assert_eq!(proof.peak_strings, buffer.capacity);
            release_task(task, CancellationContext::new(TerminationReason::Cancelled));
            close(&runtime);
        }
    }
}

#[test]
fn terminal_await_child_retains_per_task_sync_512_limit_and_first_513_error() {
    const SOURCE: &str = r#"
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return Relay(text, n - 1)
}
async fn Child(text: str, n: int): str! { return Relay(text, n) }
async fn Entry(text: str, n: int): str! { return await Child(text, n) }
"#;
    for n in [510, 511] {
        let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
        let mut host = production_interpreter(SOURCE);
        host.task_runtime = Some(runtime.clone());
        let (text, buffer) = string_input(TEXT, 4096);
        let Value::Task(task) = host
            .call_function("Entry", vec![text, Value::Int(n)], Span::default(), 0)
            .unwrap()
        else {
            panic!("actual source parent Task")
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
fn terminal_await_finally_retains_saved_primary_and_replacement_ticket_as_two_real_boxes() {
    const SOURCE: &str = r#"
async fn Child(text: str): str! { return ok(text) }
async fn Entry(first: str, second: str, fallback: str): str! {
    try { return await Child(first) } finally { return await Child(second) }
    return ok(fallback)
}
"#;
    for chunk in [1, 7, 256] {
        let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
        let (first, a) = string_input("\u{4e2d}\0primary", 1024);
        let (second, b) = string_input(TEXT, 4096);
        let (fallback, c) = string_input("fallback", 512);
        let buffers = vec![a, b, c];
        let (task, rx, events, _release) = spawn_await(
            &runtime,
            SOURCE,
            vec![first, second, fallback],
            buffers.clone(),
            AwaitOptions::normal(chunk),
        );
        entered(&task, &rx, true, false);
        let proof = finished(&rx);
        let result = task.await_result().unwrap();
        let (index, boxed) = result_buffer(&result, &buffers);
        assert_eq!(index, 1);
        assert_eq!(proof.outgoing, Some((index, boxed)));
        let tickets = ticket_events(&events);
        assert_eq!(tickets.len(), 2);
        assert_eq!(tickets[0].string, Some((a.pointer, a.capacity)));
        assert_eq!(tickets[1].boxed, boxed);
        assert_eq!((proof.peak_boxes, proof.finally), (2, 1));
        assert_eq!(proof.peak_box_bytes, 2 * std::mem::size_of::<Value>());
        assert_eq!(proof.peak_strings, a.capacity + b.capacity + c.capacity);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

fn completed_target(runtime: &TaskRuntime, native_failure: bool) -> (TaskHandle, Payload) {
    completed_target_source(runtime, SIMPLE, native_failure)
}

fn completed_target_source(
    runtime: &TaskRuntime,
    source: &str,
    native_failure: bool,
) -> (TaskHandle, Payload) {
    let (text, buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    if native_failure {
        plan.fault = Fault::NativeBox;
    }
    let (task, rx, _release) = spawn_source(runtime, source, vec![text], vec![buffer], plan);
    entered(&task, &rx, true, false);
    if native_failure {
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            Witness::Native { .. }
        ));
    } else {
        finished(&rx);
    }
    let end = Instant::now() + WAIT;
    while !matches!(task.status(), "completed" | "panicked") {
        assert!(Instant::now() < end, "finite actual child slot publication");
        std::thread::yield_now();
    }
    let actual =
        slot_payload(&task).expect("the actual child completion slot must own its payload");
    (task, actual)
}

#[test]
fn private_real_child_slot_count_and_byte_rejections_do_not_take_string_or_native_error_payload() {
    for (source, native_failure) in [(SIMPLE, false), (SIMPLE, true), (ERR_SIMPLE, false)] {
        for fault in [AwaitFault::CountBeforeTake, AwaitFault::BytesBeforeTake] {
            let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
            let (child, original) = completed_target_source(&runtime, source, native_failure);
            // This dedicated observation owner is explicit. Last-owner and
            // cancellation tests below never retain such a diagnostic clone.
            let diagnostic = child.clone();
            let (text, buffer) = string_input(TEXT, 4096);
            let mut options = AwaitOptions::normal(1);
            options.replacement = Some(child);
            options.fault = fault;
            let (task, rx, events, _release) =
                spawn_await(&runtime, AWAIT_CHILD, vec![text], vec![buffer], options);
            entered(&task, &rx, true, false);
            let proof = finished(&rx);
            let result = task.await_result();
            let after_rejection = slot_payload(&diagnostic);
            let tickets = ticket_events(&events);
            release_task(task, CancellationContext::new(TerminationReason::Cancelled));
            release_task(
                diagnostic,
                CancellationContext::new(TerminationReason::Cancelled),
            );
            close(&runtime);
            let error = result.unwrap_err();
            assert!(error
                .message
                .contains(if fault == AwaitFault::CountBeforeTake {
                    "FrameLimit"
                } else {
                    "StorageLimit"
                }));
            assert_eq!(after_rejection, Some(original), "pre-take rejection retains the same real Box, Map entries and every actual buffer in the child slot");
            assert_eq!(
                (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
                (0, 0, 0)
            );
            assert!(tickets.is_empty());
        }
    }
}

#[test]
fn private_real_native_child_error_preserves_box_map_and_all_string_buffers_through_parent() {
    let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
    let (child, original) = completed_target(&runtime, true);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut options = AwaitOptions::normal(1);
    options.replacement = Some(child);
    let (task, rx, events, _release) =
        spawn_await(&runtime, AWAIT_CHILD, vec![text], vec![buffer], options);
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    let result = task.await_result().unwrap();
    assert_eq!(payload(&result), original);
    let Value::Result { ok: false, value } = &result else {
        panic!("real LocalDriver child panic remains a false Result")
    };
    let Value::Object(fields) = value.as_ref() else {
        panic!("the actual native task error object")
    };
    assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
    assert!(
        matches!(fields.get("message"), Some(Value::String(message)) if message == "async task panicked")
    );
    assert_eq!(
        (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
        (1, original.error_count, original.error_bytes)
    );
    assert!(
        ticket_events(&events).is_empty(),
        "an already completed injected target is taken by the actual Machine poll"
    );
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
}

#[test]
fn private_sparse_error_map_policy_charges_reported_capacity_and_each_actual_string_buffer() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let mut fields = HashMap::with_capacity(128);
    for (name, text, capacity) in [
        ("domain", "task", 128),
        ("code", "sparse", 512),
        ("message", "retained sparse error", 4096),
    ] {
        let mut key = String::with_capacity(64);
        key.push_str(name);
        let mut value = String::with_capacity(capacity);
        value.push_str(text);
        fields.insert(key, Value::String(value));
    }
    // This is a private real Task-slot capacity probe. It does not claim that
    // source err or arbitrary Object construction is admitted in S2.
    let child = runtime
        .spawn(move || {
            Ok(Value::Result {
                ok: false,
                value: Box::new(Value::Object(fields)),
            })
        })
        .unwrap();
    let end = Instant::now() + WAIT;
    while child.status() != "completed" {
        assert!(
            Instant::now() < end,
            "finite actual sparse slot publication"
        );
        std::thread::yield_now();
    }
    let original = slot_payload(&child).unwrap();
    assert!(original.error_count > 3);
    let witness = child.retirement_witness();
    let measured = charge(original);
    let mut ticket = AwaitTicket::begin_with_payload_policy(
        child,
        AwaitPayloadPolicy {
            max_count: measured.count,
            max_bytes: measured.bytes,
            input_string_capacity: 0,
            span: Span::default(),
        },
    );
    assert_eq!(ticket.poll(), AwaitPoll::Ready);
    let actual = payload(ticket.test_outcome().unwrap().as_ref().unwrap());
    assert_eq!(actual, original);
    assert_eq!(ticket.retained_payload_charge(), measured);
    assert_eq!(
        AwaitPayloadCharge::for_value(ticket.test_outcome().unwrap().as_ref().unwrap(), 0).unwrap(),
        measured
    );
    let mut observed = None;
    let result = ticket
        .finish_with(|result| {
            observed = Some(payload(result.as_ref().unwrap()));
        })
        .unwrap();
    assert_eq!(
        observed,
        Some(original),
        "the operation-local finish observer borrows the actual payload"
    );
    assert_eq!(payload(&result), original);
    drop(result);
    retired(TaskRetirementBatch::from_reserved(
        vec![witness],
        CancellationContext::new(TerminationReason::Cancelled),
    ));
    close(&runtime);
}

#[test]
fn source_parent_wait_cancellation_keeps_first_cause_minimum_d_and_actual_child_retirement() {
    let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut options = AwaitOptions::normal(1);
    options.pause = AwaitPause::Waiting;
    let (task, rx, events, release) =
        spawn_await(&runtime, AWAIT_CHILD, vec![text], vec![buffer], options);
    entered(&task, &rx, true, false);
    let mut child = None;
    loop {
        match events.recv_timeout(WAIT).unwrap() {
            AwaitWitness::Child(witness) => child = Some(witness),
            AwaitWitness::Waiting => break,
            _ => panic!("the actual fresh child must still be pending"),
        }
    }
    let original = CancellationContext::new(TerminationReason::TimedOut);
    let shorter = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
    };
    assert!(task.request_cancel_with(original));
    assert!(!task.request_cancel_with(shorter));
    let expected = CancellationContext {
        reason: original.reason,
        cleanup_deadline: shorter.cleanup_deadline,
    };
    release.send(()).unwrap();
    let proof = finished(&rx);
    assert_eq!(
        task.await_result().unwrap_err().runtime_termination(),
        Some(expected)
    );
    assert_eq!(proof.peak_boxes, 0);
    release_task(task, expected);
    retired(TaskRetirementBatch::from_reserved(
        vec![child.expect("the real child witness")],
        expected,
    ));
    close(&runtime);
}

#[test]
fn native_ready_string_ticket_abandonment_retires_actual_child_and_next_claimant_is_healthy() {
    let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut options = AwaitOptions::normal(1);
    options.fault = AwaitFault::NativeReady;
    let (task, rx, events, _release) =
        spawn_await(&runtime, AWAIT_CHILD, vec![text], vec![buffer], options);
    entered(&task, &rx, true, false);
    let mut child = None;
    loop {
        match events.recv_timeout(WAIT).unwrap() {
            AwaitWitness::Child(witness) => child = Some(witness),
            AwaitWitness::Ticket { actual, .. } => {
                assert_eq!(actual.string, Some((buffer.pointer, buffer.capacity)))
            }
            AwaitWitness::ReadyNative(actual) => {
                assert_eq!(actual.string, Some((buffer.pointer, buffer.capacity)));
                break;
            }
            _ => panic!("unexpected ready native kernel event"),
        }
    }
    let Value::Result { ok: false, value } = task.await_result().unwrap() else {
        panic!("actual LocalDriver native failure")
    };
    let Value::Object(fields) = *value else {
        panic!("existing task failure object")
    };
    assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    retired(TaskRetirementBatch::from_reserved(
        vec![child.expect("real completed child witness")],
        CancellationContext::new(TerminationReason::Cancelled),
    ));
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, events, _release) = spawn_await(
        &runtime,
        AWAIT_CHILD,
        vec![text],
        vec![buffer],
        AwaitOptions::normal(1),
    );
    entered(&task, &rx, true, false);
    let proof = finished(&rx);
    assert_eq!(
        result_buffer(&task.await_result().unwrap(), &[buffer]),
        proof.outgoing.unwrap()
    );
    assert_eq!(ticket_events(&events).len(), 1);
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
}

#[test]
fn terminal_await_string_role_rejects_task_result_storage_and_cross_role_cached_dependencies() {
    for source in [
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { task = Child(text)\nreturn await task }",
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { result = await Child(text)\nreturn result }",
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return ok((await Child(text))?) }",
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str, spare: str): str! { task = Child(text)\nreturn ok(spare) }",
        "async fn Entry(text: str): str! { task = Entry(text)\nreturn await task }",
        "async fn Child(text: str): int! { return ok(text.len()) }\nasync fn Entry(text: str, spare: str): str! { task = Child(text)\nreturn ok(spare) }",
        "async fn Entry(text: str): str! { fn Child(value: str): str! { return ok(value) }\nreturn ok(Child(text)?) }",
        "async fn Child(text: str): str! { result = ok(text)\nreturn result }\nasync fn Entry(text: str): str! { return await Child(text) }",
        "async fn Child(text: str): str! { try { marker = 1 } catch (e) { marker = 2 }\nreturn ok(text) }\nasync fn Entry(text: str): str! { return await Child(text) }",
    ] {
        let source_functions = functions(source);
        let entry = &source_functions["Entry"];
        assert!(!code::eligible_claimed_async_owned_result_string_entry(&source_functions, entry, Limits::default()), "{source}");
        assert!(matches!(CodeHandle::compile_claimed_async_owned_result_string_entry(&source_functions, entry, Limits::default()), Err(AdmissionError::Unsupported(_))), "{source}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut args = Vec::new();
        let mut buffers = Vec::new();
        for _ in &entry.params {
            let (text, buffer) = string_input(TEXT, 4096);
            args.push(text); buffers.push(buffer);
        }
        let mut plan = Plan::normal(1);
        plan.selection_only = true;
        let (task, rx, _release) = spawn_source(&runtime, source, args, buffers, plan);
        entered(&task, &rx, false, false);
        assert_eq!(task.await_result().unwrap(), Value::Null);
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
}

#[test]
fn private_actual_late_child_result_after_parent_cancel_retires_without_retaining_task_owners() {
    let runtime = TaskRuntime::with_limits(2, 128, 0, 0, 128);
    let (text, child_buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    plan.pause = Pause::Produced;
    let (child, child_rx, child_release) =
        spawn_source(&runtime, SIMPLE, vec![text], vec![child_buffer], plan);
    entered(&child, &child_rx, true, false);
    let child_proof = finished(&child_rx);
    let Witness::Paused {
        stage: Pause::Produced,
        outgoing,
        ..
    } = child_rx.recv_timeout(WAIT).unwrap()
    else {
        panic!("real child result generated before commitment")
    };
    assert_eq!(outgoing, child_proof.outgoing);
    assert_eq!(child_proof.peak_boxes, 1);
    let child_witness = child.retirement_witness();
    let (text, buffer) = string_input(TEXT, 4096);
    let mut options = AwaitOptions::normal(1);
    options.replacement = Some(child); // The real sole owner transfers once.
    options.pause = AwaitPause::Waiting;
    let (task, rx, events, release) =
        spawn_await(&runtime, AWAIT_CHILD, vec![text], vec![buffer], options);
    entered(&task, &rx, true, false);
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        AwaitWitness::FreshRetired
    ));
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        AwaitWitness::Waiting
    ));
    let context = CancellationContext::new(TerminationReason::TimedOut);
    assert!(task.request_cancel_with(context));
    release.send(()).unwrap();
    let AwaitWitness::CancelTargetReleased(observed) = events.recv_timeout(WAIT).unwrap() else {
        panic!("actual ticket finish must drop its sole target before releasing the late child")
    };
    assert_eq!(observed, context);
    // Cancellation is already latched and the actual sole target owner has
    // ended. Now the real child runner submits its original String/Box late.
    child_release.send(()).unwrap();
    let proof = finished(&rx);
    assert_eq!(
        task.await_result().unwrap_err().runtime_termination(),
        Some(context)
    );
    assert_eq!(proof.peak_boxes, 0);
    release_task(task, context);
    retired(TaskRetirementBatch::from_reserved(
        vec![child_witness],
        context,
    ));
    close(&runtime);
}

fn err_await_receipt(
    source: &str,
    args: Vec<Value>,
    buffers: Vec<Buffer>,
    chunk: usize,
) -> (Finished, Vec<Payload>, KuResult<Value>) {
    let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
    let (task, rx, events, _release) =
        spawn_await(&runtime, source, args, buffers, AwaitOptions::normal(chunk));
    let id = task.id();
    let constructor = rx.recv_timeout(WAIT).unwrap();
    let completion = rx.recv_timeout(WAIT).unwrap();
    let result = task.await_result();
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
    let Witness::Finished(proof) = completion else {
        panic!("the actual worker finishes before ordinary await can help")
    };
    let tickets = ticket_events(&events);
    let Witness::Entered {
        task,
        selected,
        legacy,
        pending_error,
        steps,
        calls,
    } = constructor
    else {
        panic!("the real retired terminal-await claimant")
    };
    assert_eq!(task, id);
    assert!(selected && !legacy && !pending_error);
    assert_eq!((steps, calls), (0, 1));
    (*proof, tickets, result)
}

#[test]
fn terminal_err_fresh_child_relay_moves_actual_box_map_entries_and_six_buffers_across_chunks() {
    for chunk in [1, 7, 256] {
        let source_functions = functions(ERR_AWAIT_CHILD);
        assert!(code::eligible_claimed_async_owned_result_string_entry(
            &source_functions,
            &source_functions["Entry"],
            Limits::default()
        ));
        CodeHandle::compile_claimed_async_owned_result_string_entry(
            &source_functions,
            &source_functions["Entry"],
            Limits::default(),
        )
        .unwrap();
        let (text, buffer) = string_input(TEXT, 4096);
        let (proof, tickets, result) =
            err_await_receipt(ERR_AWAIT_CHILD, vec![text], vec![buffer], chunk);
        let result = result.unwrap();
        let actual = assert_source_error(&result, &proof, &[buffer]);
        assert_eq!(tickets.as_slice(), &[payload(&result)]);
        assert_eq!(actual.entries, tickets[0].map_entries);
        assert_eq!(actual.buffers, tickets[0].map_buffers);
        assert_eq!((proof.calls, proof.peak_calls, proof.peak_boxes), (1, 1, 1));
        assert_eq!(
            (proof.peak_errors, proof.peak_error_bytes),
            (actual.count, actual.bytes)
        );
    }
}

#[test]
fn terminal_err_single_worker_self_and_mutual_await_keep_64_and_first_65_and_child_512_513() {
    const SELF: &str = "async fn Entry(text: str, n: int): str! { if (n <= 0) { return err(text) }\nreturn await Entry(text, n - 1) }";
    const MUTUAL: &str = "async fn Other(text: str, n: int): str! { if (n <= 0) { return err(text) }\nreturn await Entry(text, n - 1) }\nasync fn Entry(text: str, n: int): str! { if (n <= 0) { return err(text) }\nreturn await Other(text, n - 1) }";
    const SYNC_CHILD: &str = "fn Relay(text: str, n: int): str! { if (n <= 0) { return err(text) }\nreturn Relay(text, n - 1) }\nasync fn Child(text: str, n: int): str! { return Relay(text, n) }\nasync fn Entry(text: str, n: int): str! { return await Child(text, n) }";
    for source in [SELF, MUTUAL] {
        for n in [64, 65] {
            let (text, buffer) = string_input(TEXT, 4096);
            let (proof, tickets, result) =
                err_await_receipt(source, vec![text, Value::Int(n)], vec![buffer], 1);
            let result = result.unwrap();
            let actual = payload(&result);
            if let Value::Result { ok: false, value } = &result {
                if let Value::Object(fields) = value.as_ref() {
                    let expected = if n == 64 { "err" } else { "await_depth" };
                    if !matches!(fields.get("code"), Some(Value::String(code)) if code == expected)
                    {
                        eprintln!("terminal err original source depth={n} limit={} domain={:?} code={:?} message={:?}", crate::runtime::task::MAX_AWAIT_DEPTH, fields.get("domain"), fields.get("code"), fields.get("message"));
                    }
                }
            }
            assert_eq!(tickets.as_slice(), &[actual]);
            if n == 64 {
                assert_source_error(&result, &proof, &[buffer]);
            } else {
                let Value::Result { ok: false, value } = &result else {
                    unreachable!()
                };
                let Value::Object(fields) = value.as_ref() else {
                    unreachable!()
                };
                assert!(
                    matches!(fields.get("code"), Some(Value::String(code)) if code == "await_depth")
                );
                assert!(proof.outgoing_error.is_some());
            }
            assert_eq!((proof.calls, proof.peak_calls, proof.peak_boxes), (1, 1, 1));
            assert_eq!(
                (proof.peak_errors, proof.peak_error_bytes),
                (actual.error_count, actual.error_bytes)
            );
        }
    }
    for n in [510, 511] {
        let (text, buffer) = string_input(TEXT, 4096);
        let (proof, tickets, result) =
            err_await_receipt(SYNC_CHILD, vec![text, Value::Int(n)], vec![buffer], 1);
        if n == 510 {
            let result = result.unwrap();
            assert_source_error(&result, &proof, &[buffer]);
            assert_eq!(tickets.as_slice(), &[payload(&result)]);
        } else {
            assert_eq!(
                result.unwrap_err().code.as_deref(),
                Some("call_depth_exceeded")
            );
            assert_eq!((proof.peak_boxes, proof.peak_errors), (0, 0));
            assert!(tickets.is_empty());
        }
    }
}

#[test]
fn terminal_err_terminal_await_q2_q3_keep_saved_primaries_and_replacement_ticket_charges() {
    const Q2: &str = "async fn Child(text: str): str! { return err(text) }\nasync fn Entry(first: str, second: str, third: str, fallback: str): str! { try { return await Child(first) } finally { return await Child(second) }\nreturn ok(fallback) }";
    const Q3: &str = "async fn Child(text: str): str! { return err(text) }\nasync fn Entry(first: str, second: str, third: str, fallback: str): str! { try { return await Child(first) } finally { try { return await Child(second) } finally { return await Child(third) } }\nreturn ok(fallback) }";
    for (source, owners) in [(Q2, 2), (Q3, 3)] {
        for chunk in [1, 7, 256] {
            let (first, a) = string_input("中\0primary", 1024);
            let (second, b) = string_input("é\0replacement", 4096);
            let (third, c) = string_input(TEXT, 2048);
            let (fallback, d) = string_input("\0fallback", 512);
            let buffers = [a, b, c, d];
            let (proof, tickets, result) = err_await_receipt(
                source,
                vec![first, second, third, fallback],
                buffers.to_vec(),
                chunk,
            );
            let result = result.unwrap();
            assert_source_error(&result, &proof, &buffers);
            assert_eq!(result_buffer(&result, &buffers).0, owners - 1);
            assert_eq!(tickets.len(), owners);
            assert_eq!(tickets[owners - 1], payload(&result));
            assert_eq!((proof.peak_boxes, proof.finally), (owners, owners - 1));
            assert_eq!(
                (proof.peak_errors, proof.peak_error_bytes),
                (
                    tickets.iter().map(|p| p.error_count).sum::<usize>(),
                    tickets.iter().map(|p| p.error_bytes).sum::<usize>()
                )
            );
            for (index, current) in tickets.iter().enumerate() {
                for previous in &tickets[..index] {
                    assert_ne!(current.boxed, previous.boxed);
                    assert!(current
                        .map_entries
                        .iter()
                        .all(|entry| !previous.map_entries.contains(entry)));
                }
            }
        }
    }
}

#[test]
fn terminal_err_ready_native_and_late_cancel_retire_actual_child_maps_without_owner_clones() {
    let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut options = AwaitOptions::normal(1);
    options.fault = AwaitFault::NativeReady;
    let (task, rx, events, _release) =
        spawn_await(&runtime, ERR_AWAIT_CHILD, vec![text], vec![buffer], options);
    let native_constructor = rx.recv_timeout(WAIT).unwrap();
    let mut native_events = Vec::new();
    for _ in 0..4 {
        let event = events.recv_timeout(WAIT).unwrap();
        let ready = matches!(event, AwaitWitness::ReadyNative(_));
        native_events.push(event);
        if ready {
            break;
        }
    }
    let result = task.await_result();
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    let (text, healthy_buffer) = string_input(TEXT, 4096);
    let (healthy, healthy_rx, healthy_events, _release) = spawn_await(
        &runtime,
        ERR_AWAIT_CHILD,
        vec![text],
        vec![healthy_buffer],
        AwaitOptions::normal(1),
    );
    let _healthy_constructor = healthy_rx.recv_timeout(WAIT).unwrap();
    let healthy_completion = healthy_rx.recv_timeout(WAIT).unwrap();
    let healthy_result = healthy.await_result();
    release_task(
        healthy,
        CancellationContext::new(TerminationReason::Cancelled),
    );
    close(&runtime);
    let result = result.unwrap();
    let healthy_result = healthy_result.unwrap();
    let Witness::Finished(healthy_proof) = healthy_completion else {
        panic!("the real healthy worker completes before ordinary await")
    };
    let healthy_tickets = ticket_events(&healthy_events);
    let mut ready = None;
    let mut ticket = None;
    for event in native_events.into_iter().chain(events.try_iter()) {
        match event {
            AwaitWitness::Child(witness) => retired(TaskRetirementBatch::from_reserved(
                vec![witness],
                CancellationContext::new(TerminationReason::Cancelled),
            )),
            AwaitWitness::Ticket { actual, cached } => {
                assert_eq!(cached, AwaitPayloadCharge::default());
                ticket = Some(actual);
            }
            AwaitWitness::ReadyNative(actual) => ready = Some(actual),
            _ => panic!("only the actual child Ready ticket may be abandoned"),
        }
    }
    assert!(matches!(
        native_constructor,
        Witness::Entered {
            selected: true,
            legacy: false,
            ..
        }
    ));
    let ready = ready.unwrap();
    assert_eq!(Some(ready), ticket);
    assert!(ready.string.is_none() && ready.error_count >= 3);
    assert_ne!(ready.map_buffers[5].0, buffer.pointer);
    let Value::Result { ok: false, value } = result else {
        unreachable!()
    };
    let Value::Object(fields) = *value else {
        unreachable!()
    };
    assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
    assert_source_error(&healthy_result, &healthy_proof, &[healthy_buffer]);
    assert_eq!(healthy_tickets.as_slice(), &[payload(&healthy_result)]);

    // A real produced source-err Map is held by the child runner before
    // commitment. Only primitive retirement witnesses survive owner release.
    let runtime = TaskRuntime::with_limits(2, 128, 0, 0, 128);
    let (text, child_buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    plan.pause = Pause::Produced;
    let (child, child_rx, child_release) =
        spawn_source(&runtime, ERR_SIMPLE, vec![text], vec![child_buffer], plan);
    let produced = (0..3)
        .map(|_| child_rx.recv_timeout(WAIT).unwrap())
        .collect::<Vec<_>>();
    let child_witness = child.retirement_witness();
    let (text, buffer) = string_input(TEXT, 4096);
    let mut options = AwaitOptions::normal(1);
    options.replacement = Some(child);
    options.pause = AwaitPause::Waiting;
    let (task, rx, events, release) =
        spawn_await(&runtime, ERR_AWAIT_CHILD, vec![text], vec![buffer], options);
    let constructor = rx.recv_timeout(WAIT).unwrap();
    let fresh_retired = events.recv_timeout(WAIT).unwrap();
    let waiting = events.recv_timeout(WAIT).unwrap();
    let context = CancellationContext::new(TerminationReason::TimedOut);
    let accepted = task.request_cancel_with(context);
    release.send(()).unwrap();
    let target_released = events.recv_timeout(WAIT).unwrap();
    child_release.send(()).unwrap();
    let completion = rx.recv_timeout(WAIT).unwrap();
    let result = task.await_result();
    release_task(task, context);
    retired(TaskRetirementBatch::from_reserved(
        vec![child_witness],
        context,
    ));
    close(&runtime);
    let Witness::Finished(proof) = completion else {
        panic!("the late-cancel parent finishes before ordinary await")
    };
    assert!(matches!(
        constructor,
        Witness::Entered {
            selected: true,
            legacy: false,
            ..
        }
    ));
    assert!(matches!(fresh_retired, AwaitWitness::FreshRetired));
    assert!(matches!(waiting, AwaitWitness::Waiting));
    assert!(
        matches!(target_released, AwaitWitness::CancelTargetReleased(actual) if actual == context)
    );
    let [Witness::Entered { selected: true, .. }, Witness::Finished(child_proof), Witness::Paused {
        stage: Pause::Produced,
        outgoing,
        ..
    }] = produced.as_slice()
    else {
        panic!("the actual late source err payload")
    };
    assert!(child_proof.outgoing_error.is_some());
    assert_eq!(*outgoing, child_proof.outgoing);
    assert!(accepted);
    assert_eq!(result.unwrap_err().runtime_termination(), Some(context));
    assert_eq!(
        (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
        (0, 0, 0)
    );
}

#[test]
fn primitive_terminal_await_fresh_child_preserves_actual_scalar_or_false_map_across_chunks() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let source = format!("fn Leaf(text: str): {ty}! {{ return {leaf} }}\nasync fn Child(text: str): {ty}! {{ return Leaf(text) }}\nasync fn Entry(text: str): {ty}! {{ return await Child(text) }}");
            let functions = functions(&source);
            assert!(code::eligible_claimed_async_owned_scalar_result_entry(
                &functions,
                &functions["Entry"],
                Limits::default()
            ));
            let code = CodeHandle::compile_claimed_async_owned_scalar_result_entry(
                &functions,
                &functions["Entry"],
                Limits::default(),
            )
            .unwrap();
            for name in ["Entry", "Child", "Leaf"] {
                assert_eq!(
                    code.function(code.find_function(name).unwrap())
                        .owned_scalar_result,
                    Some(kind)
                );
            }
            for chunk in [1, 7, 256] {
                let (text, buffer) = string_input(TEXT, 4096);
                let (proof, tickets, result) =
                    err_await_receipt(&source, vec![text], vec![buffer], chunk);
                let result = result.unwrap();
                let actual = payload(&result);
                assert_eq!(tickets.as_slice(), &[actual]);
                assert_eq!(
                    (proof.calls, proof.peak_calls, proof.peak_strings),
                    (1, 1, buffer.capacity)
                );
                if error {
                    let backing = assert_source_error(&result, &proof, &[buffer]);
                    assert_eq!(
                        (actual.map_entries, actual.map_buffers),
                        (backing.entries, backing.buffers)
                    );
                    assert_eq!(
                        charge(actual).bytes,
                        std::mem::size_of::<Value>() + backing.bytes
                    );
                } else {
                    let boxed =
                        assert_primitive_source(&result, &proof, expected_scalar(kind), chunk);
                    assert_eq!(actual.boxed, boxed.boxed);
                    assert_eq!(actual.scalar, Some(expected_scalar(kind)));
                    assert_eq!(
                        (charge(actual).count, charge(actual).bytes),
                        (1, std::mem::size_of::<Value>())
                    );
                }
            }
        }
    }
    let source = "async fn Child(text: str, value: float): float! { return ok(value) }\nasync fn Entry(text: str, value: float): float! { return await Child(text, value) }";
    let (text, buffer) = string_input(TEXT, 4096);
    let actual_null =
        crate::stdlib::array::eval("first", &[Value::Array(Vec::new())], Span::default())
            .unwrap()
            .expect("registered array.first intrinsic");
    let (proof, tickets, result) =
        err_await_receipt(source, vec![text, actual_null], vec![buffer], 1);
    let result = result.unwrap();
    assert_primitive_source(&result, &proof, ScalarLeaf::Null, 1);
    assert_eq!(tickets.as_slice(), &[payload(&result)]);
    assert_eq!(
        (tickets[0].scalar, charge(tickets[0]).bytes),
        (Some(ScalarLeaf::Null), std::mem::size_of::<Value>())
    );
}

#[test]
fn primitive_terminal_await_single_worker_keeps_64_65_fresh_child_edge_and_sync_512_513() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        for mutual in [false, true] {
            let other = if mutual {
                format!("async fn Other(text: str, n: int): {ty}! {{ if (n <= 0) {{ return ok({literal}) }}\nreturn await Entry(text, n - 1) }}\n")
            } else {
                String::new()
            };
            let next = if mutual { "Other" } else { "Entry" };
            let source = format!("{other}async fn Entry(text: str, n: int): {ty}! {{ if (n <= 0) {{ return ok({literal}) }}\nreturn await {next}(text, n - 1) }}");
            for n in [64, 65] {
                let (text, buffer) = string_input(TEXT, 4096);
                let (proof, tickets, result) =
                    err_await_receipt(&source, vec![text, Value::Int(n)], vec![buffer], 1);
                let result = result.unwrap();
                assert_eq!(tickets.as_slice(), &[payload(&result)]);
                if n == 64 {
                    assert_primitive_source(&result, &proof, expected_scalar(kind), 1);
                } else {
                    assert_await_depth(&result, n);
                    assert_eq!(proof.peak_boxes, 1);
                }
                assert_eq!((proof.calls, proof.peak_calls), (1, 1));
            }
        }
        let source = format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str, n: int): {ty}! {{ if (n <= 0) {{ return await Child(text) }}\nreturn await Entry(text, n - 1) }}");
        for n in [63, 64] {
            let (text, buffer) = string_input(TEXT, 4096);
            let (proof, tickets, result) =
                err_await_receipt(&source, vec![text, Value::Int(n)], vec![buffer], 1);
            let result = result.unwrap();
            assert_eq!(tickets.as_slice(), &[payload(&result)]);
            if n == 63 {
                assert_primitive_source(&result, &proof, expected_scalar(kind), 1);
            } else {
                assert_await_depth(&result, n);
            }
        }
        let source = format!("fn Relay(text: str, n: int): {ty}! {{ if (n <= 0) {{ return ok({literal}) }}\nreturn Relay(text, n - 1) }}\nasync fn Child(text: str, n: int): {ty}! {{ return Relay(text, n) }}\nasync fn Entry(text: str, n: int): {ty}! {{ return await Child(text, n) }}");
        for n in [510, 511] {
            let (text, buffer) = string_input(TEXT, 4096);
            let (proof, tickets, result) =
                err_await_receipt(&source, vec![text, Value::Int(n)], vec![buffer], 1);
            if n == 510 {
                let result = result.unwrap();
                assert_primitive_source(&result, &proof, expected_scalar(kind), 1);
                assert_eq!(tickets.as_slice(), &[payload(&result)]);
            } else {
                assert_eq!(
                    result.unwrap_err().code.as_deref(),
                    Some("call_depth_exceeded")
                );
                assert_eq!(proof.peak_boxes, 0);
                assert!(tickets.is_empty());
            }
        }
    }
}

fn assert_await_depth(value: &Value, depth: i64) {
    let Value::Result { ok: false, value } = value else {
        panic!("the original first await-depth rejection is a false Result at source depth {depth}")
    };
    let Value::Object(fields) = value.as_ref() else {
        unreachable!()
    };
    assert!(
        matches!(fields.get("domain"), Some(Value::String(domain)) if domain == "task"),
        "depth={depth}, fields={fields:?}"
    );
    assert!(
        matches!(fields.get("code"), Some(Value::String(code)) if code == "await_depth"),
        "depth={depth}, limit={}, fields={fields:?}",
        crate::runtime::task::MAX_AWAIT_DEPTH
    );
}

#[test]
fn primitive_terminal_await_real_slot_pre_take_rejections_preserve_box_and_four_budget_entries() {
    for (_, ty, literal) in ASYNC_PRIMITIVES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let child_source = format!("async fn Entry(text: str): {ty}! {{ return {leaf} }}");
            let parent = format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return await Child(text) }}");
            for fault in [AwaitFault::CountBeforeTake, AwaitFault::BytesBeforeTake] {
                let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
                let (child, original) = completed_target_source(&runtime, &child_source, false);
                // This explicit diagnostic owner is limited to slot metadata;
                // the cancellation/last-owner tests retain no such clone.
                let diagnostic = child.clone();
                let (text, buffer) = string_input(TEXT, 4096);
                let mut options = AwaitOptions::normal(1);
                options.replacement = Some(child);
                options.fault = fault;
                let (task, rx, events, _) =
                    spawn_await(&runtime, &parent, vec![text], vec![buffer], options);
                let constructor = rx.recv_timeout(WAIT).unwrap();
                let completion = rx.recv_timeout(WAIT).unwrap();
                let result = task.await_result();
                let retained = slot_payload(&diagnostic);
                release_task(task, CancellationContext::new(TerminationReason::Cancelled));
                release_task(
                    diagnostic,
                    CancellationContext::new(TerminationReason::Cancelled),
                );
                close(&runtime);
                let tickets = ticket_events(&events);
                assert!(matches!(
                    constructor,
                    Witness::Entered {
                        selected: true,
                        legacy: false,
                        ..
                    }
                ));
                let Witness::Finished(proof) = completion else {
                    panic!("pre-take rejection drains real parent")
                };
                assert_eq!(retained, Some(original), "the rejected real child slot preserves Box, actual scalar or Map and all six buffers");
                assert!(result.unwrap_err().message.contains(
                    if fault == AwaitFault::CountBeforeTake {
                        "FrameLimit"
                    } else {
                        "StorageLimit"
                    }
                ));
                assert_eq!(
                    (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
                    (0, 0, 0)
                );
                assert!(tickets.is_empty());
                if !error {
                    assert_eq!(
                        (charge(original).count, charge(original).bytes),
                        (1, std::mem::size_of::<Value>())
                    );
                }
            }
        }
    }
}

#[test]
fn primitive_terminal_await_q2_q3_saved_primaries_return_last_real_ticket_box() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        for (errors, owners) in [
            ([false, false, false], 2),
            ([true, true, false], 2),
            ([true, false, false], 2),
            ([false, true, false], 2),
            ([true, false, true], 3),
        ] {
            let mut children = String::new();
            for (index, name) in ["First", "Second", "Third"].into_iter().enumerate() {
                let leaf = if errors[index] {
                    "err(text)".to_string()
                } else {
                    format!("ok({literal})")
                };
                children.push_str(&format!(
                    "async fn {name}(text: str): {ty}! {{ return {leaf} }}\n"
                ));
            }
            let replacement = if owners == 2 {
                "return await Second(second)"
            } else {
                "try { return await Second(second) } finally { return await Third(third) }"
            };
            let source = format!("{children}async fn Entry(first: str, second: str, third: str): {ty}! {{ try {{ return await First(first) }} finally {{ {replacement} }}\nreturn ok({literal}) }}");
            for chunk in [1, 7, 256] {
                let (a, first) = string_input("中\0primary", 1024);
                let (b, second) = string_input("é\0replacement", 4096);
                let (c, third) = string_input(TEXT, 2048);
                let buffers = [first, second, third];
                let (proof, tickets, result) =
                    err_await_receipt(&source, vec![a, b, c], buffers.to_vec(), chunk);
                let result = result.unwrap();
                let actual = payload(&result);
                assert_eq!(tickets.len(), owners);
                assert_eq!(tickets.last(), Some(&actual));
                assert_eq!(
                    (proof.peak_boxes, proof.peak_box_bytes, proof.finally),
                    (owners, owners * std::mem::size_of::<Value>(), owners - 1)
                );
                assert_eq!(
                    proof.peak_strings,
                    buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
                );
                if errors[owners - 1] {
                    assert_eq!(result_buffer(&result, &buffers).0, owners - 1);
                } else {
                    assert_eq!(actual.scalar, Some(expected_scalar(kind)));
                }
                if chunk == 1 {
                    assert_eq!(proof.primitive_owners.len(), owners);
                    assert_eq!(
                        proof.last_primitive_candidate,
                        primitive_payload(&result),
                        "the last independently read Ready ticket is the final actual Box"
                    );
                    assert_eq!(
                        (proof.peak_errors, proof.peak_error_bytes),
                        (
                            tickets.iter().map(|p| p.error_count).sum::<usize>(),
                            tickets.iter().map(|p| p.error_bytes).sum::<usize>()
                        )
                    );
                }
            }
        }
    }
}

#[test]
fn primitive_terminal_await_later_argument_fuel_ready_native_waiting_and_late_cancel_retire() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        let source = format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return await Child(text) }}");
        let later = format!("async fn Child(text: str, ignored: int): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return await Child(text, 1 / 0) }}");
        let (text, buffer) = string_input(TEXT, 4096);
        let (proof, tickets, result) = err_await_receipt(&later, vec![text], vec![buffer], 1);
        assert_eq!(result.unwrap_err().message, "division by zero");
        assert_eq!(proof.peak_boxes, 0);
        assert!(tickets.is_empty());
        for fault in [Fault::FuelBefore, Fault::FuelAfter] {
            let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
            let (text, buffer) = string_input(TEXT, 4096);
            let mut options = AwaitOptions::normal(1);
            options.source_fault = fault;
            let (task, rx, events, _) =
                spawn_await(&runtime, &source, vec![text], vec![buffer], options);
            let constructor = rx.recv_timeout(WAIT).unwrap();
            let completion = rx.recv_timeout(WAIT).unwrap();
            let result = task.await_result();
            release_task(task, CancellationContext::new(TerminationReason::Cancelled));
            close(&runtime);
            let tickets = ticket_events(&events);
            assert!(matches!(
                constructor,
                Witness::Entered { selected: true, .. }
            ));
            let Witness::Finished(proof) = completion else {
                panic!("actual fuel parent drain")
            };
            assert_eq!(
                result.unwrap_err().message,
                "internal execution prototype test fuel exhausted"
            );
            assert_eq!(proof.peak_boxes, usize::from(fault == Fault::FuelAfter));
            if fault == Fault::FuelBefore {
                assert!(tickets.is_empty());
            }
        }
        let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
        let (text, buffer) = string_input(TEXT, 4096);
        let mut options = AwaitOptions::normal(1);
        options.fault = AwaitFault::NativeReady;
        let (task, rx, events, _) =
            spawn_await(&runtime, &source, vec![text], vec![buffer], options);
        let constructor = rx.recv_timeout(WAIT).unwrap();
        let mut observed = Vec::new();
        loop {
            let event = events.recv_timeout(WAIT).unwrap();
            let ready = matches!(event, AwaitWitness::ReadyNative(_));
            observed.push(event);
            if ready {
                break;
            }
        }
        let result = task.await_result();
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        let (text, buffer) = string_input(TEXT, 4096);
        let (healthy, healthy_rx, healthy_events, _) = spawn_await(
            &runtime,
            &source,
            vec![text],
            vec![buffer],
            AwaitOptions::normal(1),
        );
        let healthy_constructor = healthy_rx.recv_timeout(WAIT).unwrap();
        let healthy_completion = healthy_rx.recv_timeout(WAIT).unwrap();
        let healthy_result = healthy.await_result();
        release_task(
            healthy,
            CancellationContext::new(TerminationReason::Cancelled),
        );
        close(&runtime);
        assert!(matches!(
            constructor,
            Witness::Entered { selected: true, .. }
        ));
        assert!(matches!(
            healthy_constructor,
            Witness::Entered { selected: true, .. }
        ));
        let Witness::Finished(proof) = healthy_completion else {
            panic!("same worker remains healthy after actual Ready abandonment")
        };
        let healthy_result = healthy_result.unwrap();
        assert_primitive_source(&healthy_result, &proof, expected_scalar(kind), 1);
        assert_eq!(
            ticket_events(&healthy_events).as_slice(),
            &[payload(&healthy_result)]
        );
        let mut ticket = None;
        let mut ready = None;
        for event in observed.into_iter().chain(events.try_iter()) {
            match event {
                AwaitWitness::Ticket { actual, .. } => ticket = Some(actual),
                AwaitWitness::ReadyNative(actual) => ready = Some(actual),
                AwaitWitness::Child(witness) => retired(TaskRetirementBatch::from_reserved(
                    vec![witness],
                    CancellationContext::new(TerminationReason::Cancelled),
                )),
                _ => panic!("actual Ready ownership events"),
            }
        }
        assert_eq!(ticket, ready);
        assert_eq!(ready.unwrap().scalar, Some(expected_scalar(kind)));
        let Value::Result { ok: false, value } = result.unwrap() else {
            panic!("native panic Result")
        };
        let Value::Object(fields) = *value else {
            unreachable!()
        };
        assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));

        let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
        let (text, buffer) = string_input(TEXT, 4096);
        let mut options = AwaitOptions::normal(1);
        options.pause = AwaitPause::Ready;
        let (task, rx, events, release) =
            spawn_await(&runtime, &source, vec![text], vec![buffer], options);
        let constructor = rx.recv_timeout(WAIT).unwrap();
        let mut ready_events = Vec::new();
        loop {
            let event = events.recv_timeout(WAIT).unwrap();
            let ready = matches!(event, AwaitWitness::ReadyPaused(_));
            ready_events.push(event);
            if ready {
                break;
            }
        }
        let original = CancellationContext::new(TerminationReason::TimedOut);
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
        };
        let accepted = task.request_cancel_with(original);
        let second = task.request_cancel_with(shorter);
        let expected = CancellationContext {
            reason: original.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        release.send(()).unwrap();
        let completion = rx.recv_timeout(WAIT).unwrap();
        let result = task.await_result();
        release_task(task, expected);
        close(&runtime);
        assert!(matches!(
            constructor,
            Witness::Entered { selected: true, .. }
        ));
        assert!(matches!(completion, Witness::Finished(_)));
        assert!(accepted && !second);
        assert_eq!(result.unwrap_err().runtime_termination(), Some(expected));
        let mut ticket = None;
        let mut ready = None;
        for event in ready_events.into_iter().chain(events.try_iter()) {
            match event {
                AwaitWitness::Ticket { actual, .. } => ticket = Some(actual),
                AwaitWitness::ReadyPaused(actual) => ready = Some(actual),
                AwaitWitness::Child(witness) => {
                    retired(TaskRetirementBatch::from_reserved(vec![witness], expected))
                }
                AwaitWitness::CancelTargetReleased(context) => assert_eq!(context, expected),
                _ => panic!("actual Ready cancellation events"),
            }
        }
        assert_eq!(ticket, ready);
        assert_eq!(ready.unwrap().scalar, Some(expected_scalar(kind)));

        // A real produced child remains on its runner stack. No diagnostic
        // Task clone survives the sole target transfer or cancellation.
        let runtime = TaskRuntime::with_limits(2, 128, 0, 0, 128);
        let child_source = format!("async fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        let (text, child_buffer) = string_input(TEXT, 4096);
        let mut plan = Plan::normal(1);
        plan.pause = Pause::Produced;
        let (child, child_rx, child_release) = spawn_source(
            &runtime,
            &child_source,
            vec![text],
            vec![child_buffer],
            plan,
        );
        let child_constructor = child_rx.recv_timeout(WAIT).unwrap();
        let child_completion = child_rx.recv_timeout(WAIT).unwrap();
        let child_pause = child_rx.recv_timeout(WAIT).unwrap();
        let witness = child.retirement_witness();
        let (text, buffer) = string_input(TEXT, 4096);
        let mut options = AwaitOptions::normal(1);
        options.replacement = Some(child);
        options.pause = AwaitPause::Waiting;
        let (task, rx, events, release) =
            spawn_await(&runtime, &source, vec![text], vec![buffer], options);
        let constructor = rx.recv_timeout(WAIT).unwrap();
        let fresh_retired = events.recv_timeout(WAIT).unwrap();
        let waiting = events.recv_timeout(WAIT).unwrap();
        let original = CancellationContext::new(TerminationReason::TimedOut);
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
        };
        let accepted = task.request_cancel_with(original);
        let second = task.request_cancel_with(shorter);
        let expected = CancellationContext {
            reason: original.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        release.send(()).unwrap();
        let target_released = events.recv_timeout(WAIT).unwrap();
        child_release.send(()).unwrap();
        let completion = rx.recv_timeout(WAIT).unwrap();
        let result = task.await_result();
        release_task(task, expected);
        retired(TaskRetirementBatch::from_reserved(vec![witness], expected));
        close(&runtime);
        assert!(matches!(
            child_constructor,
            Witness::Entered { selected: true, .. }
        ));
        assert!(matches!(
            child_pause,
            Witness::Paused {
                stage: Pause::Produced,
                ..
            }
        ));
        let Witness::Finished(child_proof) = child_completion else {
            panic!("produced real child")
        };
        assert!(child_proof.outgoing_primitive.is_some());
        assert!(matches!(
            constructor,
            Witness::Entered { selected: true, .. }
        ));
        assert!(matches!(fresh_retired, AwaitWitness::FreshRetired));
        assert!(matches!(waiting, AwaitWitness::Waiting));
        assert!(
            matches!(target_released, AwaitWitness::CancelTargetReleased(context) if context == expected)
        );
        assert!(accepted && !second);
        let Witness::Finished(proof) = completion else {
            panic!("cancelled real parent drains")
        };
        assert_eq!(proof.peak_boxes, 0);
        assert_eq!(result.unwrap_err().runtime_termination(), Some(expected));
    }
}

#[test]
fn primitive_terminal_await_legal_whole_tree_and_cached_kind_rejections_keep_legacy_inputs() {
    for (_, ty, literal) in ASYNC_PRIMITIVES {
        let wrong_ty = if ty == "float" { "int" } else { "float" };
        let wrong_literal = if wrong_ty == "int" { "7" } else { "1.25" };
        let invalid = format!("async fn Wrong(text: str): {wrong_ty}! {{ return ok({wrong_literal}) }}\nasync fn Entry(text: str): {ty}! {{ return ok({literal})\nreturn await Wrong(text) }}\nfn main() {{}}");
        let program = Parser::new(Lexer::new(&invalid).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let error = crate::checker::Checker::new()
            .check(&program)
            .expect_err("typed cross-kind terminal await return is checker-invalid even when dead");
        assert!(
            error
                .message
                .contains(&format!("expected {ty}! but got {wrong_ty}!")),
            "{error:?}"
        );
        for source in [
            format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ task = Child(text)\nreturn await task }}"),
            format!("async fn Child(text: str): {ty}! {{ result = ok({literal})\nreturn result }}\nasync fn Entry(text: str): {ty}! {{ return await Child(text) }}"),
            format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return ok((await Child(text))?) }}"),
            format!("async fn Wrong(text: str): {wrong_ty}! {{ return ok({wrong_literal}) }}\nasync fn Entry(text: str): {ty}! {{ return ok({literal})\nawait Wrong(text) }}"),
        ] {
            let functions = functions(&source); let entry = &functions["Entry"];
            assert!(!code::eligible_claimed_async_owned_scalar_result_entry(&functions, entry, Limits::default()), "{source}");
            assert!(matches!(CodeHandle::compile_claimed_async_owned_scalar_result_entry(&functions, entry, Limits::default()), Err(AdmissionError::Unsupported(_))), "{source}");
            let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8); let (text, buffer) = string_input(TEXT, 4096); let mut plan = Plan::normal(1); plan.selection_only = true;
            let (task, rx, _) = spawn_source(&runtime, &source, vec![text], vec![buffer], plan); let constructor = rx.recv_timeout(WAIT).unwrap(); let result = task.await_result();
            release_task(task, CancellationContext::new(TerminationReason::Cancelled)); close(&runtime);
            assert!(matches!(constructor, Witness::Entered { selected: false, legacy: true, steps: 0, .. })); assert_eq!(result.unwrap(), Value::Null);
        }
    }
    // Strict Unsupported must preserve the previously admitted scalar-only
    // Machine slice, including its legal nested Result unwrap capability.
    let source = "async fn Child(n: int): int! { return ok(n) }\nasync fn Entry(n: int): int! { return ok((await Child(n))?) }";
    let functions = functions(source);
    assert!(matches!(
        code::admit_claimed_async_owned_scalar_result_entry(
            &functions,
            &functions["Entry"],
            Limits::default()
        ),
        Err(AdmissionError::Unsupported(_))
    ));
    let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
    let mut host = production_interpreter(source);
    host.task_runtime = Some(runtime.clone());
    host.async_execution = true;
    let inner = ClaimedNamedTask::new(
        host,
        functions["Entry"].clone(),
        vec![Value::Int(7)],
        Span::default(),
    );
    let machine = inner
        .machine
        .as_ref()
        .expect("strict Unsupported preserves the admitted old scalar Machine");
    let legacy = inner.legacy.is_some();
    let entry = machine.code.find_function("Entry").unwrap();
    let declared = machine.code.function(entry).owned_scalar_result;
    let effects = (inner.interpreter.steps, inner.interpreter.call_depth);
    drop(inner);
    close(&runtime);
    assert!(!legacy);
    assert_eq!(declared, None);
    assert_eq!(effects, (0, 0));
    let complete = format!("{source}\nasync fn main(): null! {{ value = (await Entry(7))?\nif (value != 7) {{ return err(\"old scalar result changed\") }}\nreturn ok(null) }}");
    let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    crate::cli::run_source("async-primitive-strict-unsupported-legacy.ku", &complete).unwrap();
}
