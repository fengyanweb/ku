use super::*;
use crate::runtime::task::{AwaitPayloadCharge, AwaitPayloadPolicy, AwaitPoll, AwaitTicket};

const AWAIT_CHILD: &str = "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return await Child(text) }";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Payload {
    boxed: usize,
    string: Option<(usize, usize)>,
    map_buffers: [(usize, usize); 6],
    error_count: usize,
    error_bytes: usize,
}

fn payload_opt(value: &Value) -> Option<Payload> {
    let Value::Result { ok, value } = value else {
        return None;
    };
    let boxed = value.as_ref() as *const Value as usize;
    if *ok {
        let Value::String(text) = value.as_ref() else {
            return None;
        };
        Some(Payload {
            boxed,
            string: Some((text.as_ptr() as usize, text.capacity())),
            map_buffers: [(0, 0); 6],
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
        for (index, name) in ["domain", "code", "message"].into_iter().enumerate() {
            let (key, Value::String(text)) = fields.get_key_value(name)? else {
                return None;
            };
            error_bytes += key.capacity() + text.capacity();
            map_buffers[index * 2] = (key.as_ptr() as usize, key.capacity());
            map_buffers[index * 2 + 1] = (text.as_ptr() as usize, text.capacity());
        }
        Some(Payload {
            boxed,
            string: None,
            map_buffers,
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
}

struct AwaitOptions {
    chunk: usize,
    fault: AwaitFault,
    pause: AwaitPause,
    replacement: Option<TaskHandle>,
}

impl AwaitOptions {
    fn normal(chunk: usize) -> Self {
        Self {
            chunk,
            fault: AwaitFault::None,
            pause: AwaitPause::None,
            replacement: None,
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
                        plan: Plan::normal(options.chunk),
                        boxes: vec![None; buffers.len()],
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
    let (text, buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    if native_failure {
        plan.fault = Fault::NativeBox;
    }
    let (task, rx, _release) = spawn_source(runtime, SIMPLE, vec![text], vec![buffer], plan);
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
    for native_failure in [false, true] {
        for fault in [AwaitFault::CountBeforeTake, AwaitFault::BytesBeforeTake] {
            let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
            let (child, original) = completed_target(&runtime, native_failure);
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
            let error = task.await_result().unwrap_err();
            assert!(error
                .message
                .contains(if fault == AwaitFault::CountBeforeTake {
                    "FrameLimit"
                } else {
                    "StorageLimit"
                }));
            assert_eq!(slot_payload(&diagnostic), Some(original), "pre-take rejection retains the same real Box and every actual buffer in the child slot");
            assert_eq!(
                (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
                (0, 0, 0)
            );
            assert!(ticket_events(&events).is_empty());
            release_task(task, CancellationContext::new(TerminationReason::Cancelled));
            release_task(
                diagnostic,
                CancellationContext::new(TerminationReason::Cancelled),
            );
            close(&runtime);
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
        "fn Leaf(text: str): str! { return err(text) }\nasync fn Child(text: str): str! { return Leaf(text) }\nasync fn Entry(text: str): str! { return await Child(text) }",
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
