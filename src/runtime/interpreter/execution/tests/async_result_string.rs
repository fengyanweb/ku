use super::*;
use crate::runtime::task::{LocalTaskRunner, RetirementBatchPoll, TaskHandle, TaskRetirementBatch};
use std::ops::ControlFlow;
use std::sync::mpsc;

const WAIT: Duration = Duration::from_secs(3);

mod await_string;

const TEXT: &str = "\u{4fdd}\u{7559}\0async";
const SIMPLE: &str = "async fn Entry(text: str): str! { return ok(text) }";
const ERR_SIMPLE: &str = "async fn Entry(text: str): str! { return err(text) }";
const ERR_RELAY: &str = r#"
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return err(text) }
    return Relay(text, n - 1)
}
async fn Entry(text: str, n: int): str! { return Relay(text, n) }
"#;
const RELAY: &str = r#"
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return Relay(text, n - 1)
}
async fn Entry(text: str, n: int): str! { return Relay(text, n) }
"#;

#[derive(Clone, Copy, Debug)]
pub(super) struct Buffer {
    pub(super) pointer: usize,
    pub(super) capacity: usize,
    pub(super) text: &'static str,
}

pub(super) fn string_input(text: &'static str, capacity: usize) -> (Value, Buffer) {
    let mut value = String::with_capacity(capacity);
    value.push_str(text);
    let buffer = Buffer {
        pointer: value.as_ptr() as usize,
        capacity: value.capacity(),
        text,
    };
    (Value::String(value), buffer)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ErrorPayload {
    pub(super) boxed: usize,
    pub(super) entries: [usize; 3],
    pub(super) buffers: [(usize, usize); 6],
    pub(super) count: usize,
    pub(super) bytes: usize,
}

pub(super) fn error_payload(value: &Value) -> Option<ErrorPayload> {
    let Value::Result { ok: false, value } = value else {
        return None;
    };
    let Value::Object(fields) = value.as_ref() else {
        return None;
    };
    if fields.len() != 3 {
        return None;
    }
    let mut actual = ErrorPayload {
        boxed: value.as_ref() as *const Value as usize,
        entries: [0; 3],
        buffers: [(0, 0); 6],
        count: fields.capacity(),
        bytes: fields.capacity() * std::mem::size_of::<(String, Value)>(),
    };
    for (index, name) in ["domain", "code", "message"].into_iter().enumerate() {
        let (key, Value::String(text)) = fields.get_key_value(name)? else {
            return None;
        };
        actual.entries[index] = key as *const String as usize;
        actual.buffers[index * 2] = (key.as_ptr() as usize, key.capacity());
        actual.buffers[index * 2 + 1] = (text.as_ptr() as usize, text.capacity());
        actual.bytes += key.capacity() + text.capacity();
    }
    Some(actual)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScalarLeaf {
    Int(i64),
    Float(u64),
    Bool(bool),
    Null,
}

fn scalar_leaf(value: &Value) -> Option<ScalarLeaf> {
    match value {
        Value::Int(value) => Some(ScalarLeaf::Int(*value)),
        Value::Float(value) => Some(ScalarLeaf::Float(value.to_bits())),
        Value::Bool(value) => Some(ScalarLeaf::Bool(*value)),
        Value::Null => Some(ScalarLeaf::Null),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrimitivePayload {
    boxed: usize,
    scalar: Option<ScalarLeaf>,
    error: Option<ErrorPayload>,
}

fn primitive_payload(value: &Value) -> Option<PrimitivePayload> {
    let Value::Result { ok, value: leaf } = value else {
        return None;
    };
    let boxed = leaf.as_ref() as *const Value as usize;
    if *ok {
        Some(PrimitivePayload {
            boxed,
            scalar: Some(scalar_leaf(leaf)?),
            error: None,
        })
    } else {
        Some(PrimitivePayload {
            boxed,
            scalar: None,
            error: Some(error_payload(value)?),
        })
    }
}

fn observe_primitives(
    machine: &Machine,
    seen: &mut Vec<PrimitivePayload>,
) -> Option<PrimitivePayload> {
    // Only a newly allocated candidate on the real value stack can replace
    // this witness. Later arbitration cannot rewrite it from a counter.
    let fresh = machine
        .values
        .as_slice()
        .last()
        .and_then(primitive_payload)
        .filter(|actual| !seen.iter().any(|old| old.boxed == actual.boxed));
    let mut live = Vec::new();
    let mut read = |value: &Value| {
        if let Some(actual) = primitive_payload(value) {
            assert!(
                !live.contains(&actual.boxed),
                "one actual owner per primitive Box"
            );
            live.push(actual.boxed);
            if let Some(old) = seen.iter().find(|old| old.boxed == actual.boxed) {
                assert_eq!(
                    *old, actual,
                    "real primitive Box and false Map backing move intact"
                );
            } else {
                seen.push(actual);
            }
        }
    };
    for value in machine.values.as_slice() {
        read(value);
    }
    if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &machine.pending {
        read(value);
    }
    for exit in machine.exits.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) =
            exit.state.outcome()
        {
            read(value);
        }
    }
    for frame in machine.finally.frames.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &frame.primary {
            read(value);
        }
    }
    if let Some(Ok(value)) = machine
        .awaiting
        .as_ref()
        .and_then(|frame| frame.ticket.test_outcome())
    {
        read(value);
    }
    fresh
}

fn primitive_budget_entries(machine: &mut Machine) {
    let span = Span::default();
    let count = machine.values.capacity()
        + machine.argument_capacity
        + machine.exit_handle_capacity
        + machine.retirement_capacity
        + machine.lease_capacity
        + machine.lease_root_bytes / std::mem::size_of::<Value>()
        + machine.result_wrapper_capacity
        + machine.error_leaf_capacity;
    let bytes = machine.values.charged_storage_bytes()
        + machine.argument_bytes
        + machine.exit_handle_bytes
        + machine.retirement_bytes
        + machine.string_payload_bytes
        + machine.lease_bytes
        + machine.lease_root_bytes
        + machine.result_wrapper_bytes
        + machine.error_leaf_bytes;
    let saved = machine.value_limits;
    for rejection in [None, Some("FrameLimit"), Some("StorageLimit")] {
        machine.value_limits.max_frames = count - usize::from(rejection == Some("FrameLimit"));
        machine.value_limits.max_storage_bytes =
            bytes - usize::from(rejection == Some("StorageLimit"));
        let spare = machine.values.capacity() - machine.values.len();
        for result in [
            machine.reserve_values(spare, span),
            machine.check_argument_capacity(0, span),
            machine.check_exit_storage_capacity(0, 0, span),
            machine.check_lease_capacity(0, 0, span),
        ] {
            if let Some(reason) = rejection {
                assert!(result.unwrap_err().message.contains(reason));
            } else {
                result.unwrap();
            }
        }
    }
    machine.value_limits = saved;
}

#[derive(Default, Debug)]
struct LocalSourceProof {
    live: Vec<PrimitivePayload>,
    last_candidate: Option<PrimitivePayload>,
    peak_env: usize,
    peak_live: usize,
    distinct_clones: usize,
    budget_checks: usize,
    clone_source: Option<PrimitivePayload>,
    clone_source_unchanged: bool,
    full_depth_observed: bool,
    popped: usize,
}

struct LocalSourceCensus {
    env: Vec<PrimitivePayload>,
    live: Vec<PrimitivePayload>,
    ticket: Option<PrimitivePayload>,
    count: usize,
    bytes: usize,
}

fn primitive_charge(owner: PrimitivePayload) -> AwaitPayloadCharge {
    let (error_leaf_count, error_leaf_bytes) = owner.error.map_or((0, 0), |e| (e.count, e.bytes));
    let box_bytes = std::mem::size_of::<Value>();
    AwaitPayloadCharge {
        count: 1 + error_leaf_count,
        bytes: box_bytes + error_leaf_bytes,
        box_count: 1,
        box_bytes,
        error_leaf_count,
        error_leaf_bytes,
    }
}

fn primitive_totals(owners: &[PrimitivePayload]) -> AwaitPayloadCharge {
    owners
        .iter()
        .copied()
        .fold(AwaitPayloadCharge::default(), |total, owner| {
            sum_payload_charge(total, primitive_charge(owner))
        })
}

fn planned_local_clone_charge(source: &Value) -> Option<AwaitPayloadCharge> {
    let actual = primitive_payload(source)?;
    if actual.error.is_none() {
        return Some(primitive_charge(actual));
    }
    let Value::Result { value, .. } = source else {
        return None;
    };
    let Value::Object(fields) = value.as_ref() else {
        return None;
    };
    let Value::String(domain) = fields.get("domain")? else {
        return None;
    };
    let Value::String(code) = fields.get("code")? else {
        return None;
    };
    let Value::String(message) = fields.get("message")? else {
        return None;
    };
    AwaitPayloadCharge::planned_error_object(domain, code, message.len()).ok()
}

fn local_source_census(machine: &Machine) -> LocalSourceCensus {
    // Each source in this mode has at most four Result locals per frame.
    // Allocate and initialize the Copy-only observation slots before taking
    // a binding lock: the callback never allocates, clones, asserts or keeps
    // a reference, Arc, source Value or guard alive beyond that lock.
    let mut metadata = vec![None; machine.calls.len() * 4];
    let mut used = 0;
    let mut env_charge = AwaitPayloadCharge::default();
    for frame in machine.calls.as_slice() {
        let (has_result, overflow) =
            frame
                .env
                .fold_owned_values((false, false), |(has, overflow), value| {
                    let mut overflow = overflow;
                    if let Some(actual) = primitive_payload(value) {
                        if let Some(slot) = metadata.get_mut(used) {
                            *slot = Some(actual);
                            used += 1;
                        } else {
                            overflow = true;
                        }
                    }
                    (has || matches!(value, Value::Result { .. }), overflow)
                });
        assert!(
            !overflow,
            "preallocated Copy observation slots cover the fixture's real bindings"
        );
        assert!(
            !has_result || frame.has_selected_result_bindings,
            "every real Result local activates the production frame census"
        );
        env_charge = sum_payload_charge(env_charge, frame.env.selected_scalar_result_charge());
    }
    let env = metadata.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(env_charge, primitive_totals(&env));
    let mut live = env.clone();
    let mut read = |value: &Value| {
        if let Some(actual) = primitive_payload(value) {
            live.push(actual);
        }
    };
    for value in machine.values.as_slice() {
        read(value);
    }
    if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &machine.pending {
        read(value);
    }
    for exit in machine.exits.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) =
            exit.state.outcome()
        {
            read(value);
        }
    }
    for frame in machine.finally.frames.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &frame.primary {
            read(value);
        }
    }
    let physical = primitive_totals(&live);
    let ticket = machine
        .awaiting
        .as_ref()
        .and_then(|frame| frame.ticket.test_outcome())
        .and_then(|outcome| outcome.as_ref().ok())
        .and_then(primitive_payload);
    let ticket_charge = ticket.map_or_else(AwaitPayloadCharge::default, primitive_charge);
    if let Some(frame) = &machine.awaiting {
        assert_eq!(
            frame.ticket.retained_payload_charge(),
            ticket_charge,
            "actual Ready ticket backing is measured independently of its cached subtotal"
        );
    }
    if let Some(ticket) = ticket {
        live.push(ticket);
    }
    let mut unique = std::collections::HashSet::with_capacity(live.len());
    for owner in &live {
        assert!(
            unique.insert(owner.boxed),
            "one physical owning location per Result Box"
        );
    }
    assert_eq!(
        machine.await_payload_escrow,
        AwaitPayloadCharge::default(),
        "operation-local escrow is checked inside the existing finish_with probes"
    );
    let stable = sum_payload_charge(physical, machine.await_payload_snapshot);
    assert_eq!(
        (
            machine.result_wrapper_capacity,
            machine.result_wrapper_bytes,
            machine.error_leaf_capacity,
            machine.error_leaf_bytes
        ),
        (
            stable.box_count,
            stable.box_bytes,
            stable.error_leaf_count,
            stable.error_leaf_bytes
        )
    );
    assert_eq!(
        (
            machine.metrics.retained_result_wrapper_capacity,
            machine.metrics.retained_result_wrapper_bytes,
            machine.metrics.retained_error_leaf_capacity,
            machine.metrics.retained_error_leaf_bytes
        ),
        (
            stable.box_count,
            stable.box_bytes,
            stable.error_leaf_count,
            stable.error_leaf_bytes
        )
    );
    let current = sum_payload_charge(physical, ticket_charge);
    let count = machine.values.capacity()
        + machine.argument_capacity
        + machine.exit_handle_capacity
        + machine.retirement_capacity
        + machine.lease_capacity
        + machine.lease_root_bytes / std::mem::size_of::<Value>()
        + current.count;
    let bytes = machine.values.charged_storage_bytes()
        + machine.argument_bytes
        + machine.exit_handle_bytes
        + machine.retirement_bytes
        + machine.string_payload_bytes
        + machine.lease_bytes
        + machine.lease_root_bytes
        + current.bytes;
    LocalSourceCensus {
        env,
        live,
        ticket,
        count,
        bytes,
    }
}

fn observe_source_locals(
    machine: &Machine,
    _buffers: &[Buffer],
    proof: &mut LocalSourceProof,
) -> LocalSourceCensus {
    let census = local_source_census(machine);
    // This is an instantaneous census. An owner may retire and an allocator
    // may reuse its address between observations, including across several
    // Work items in a coarse chunk. Clone identity is established separately
    // around the actual one-step clone while source and candidate coexist.
    proof.peak_env = proof.peak_env.max(census.env.len());
    proof.peak_live = proof.peak_live.max(census.live.len());
    proof.live = census.live.clone();
    census
}

fn local_source_budget_entries(machine: &mut Machine, _buffers: &[Buffer]) -> (usize, usize) {
    let census = local_source_census(machine);
    let saved = machine.value_limits;
    for reason in [None, Some("FrameLimit"), Some("StorageLimit")] {
        machine.value_limits.max_frames = census.count - usize::from(reason == Some("FrameLimit"));
        machine.value_limits.max_storage_bytes =
            census.bytes - usize::from(reason == Some("StorageLimit"));
        let spare = machine.values.capacity() - machine.values.len();
        for result in [
            machine.reserve_values(spare, Span::default()),
            machine.check_argument_capacity(0, Span::default()),
            machine.check_exit_storage_capacity(0, 0, Span::default()),
            machine.check_lease_capacity(0, 0, Span::default()),
        ] {
            if let Some(reason) = reason {
                assert!(result.unwrap_err().message.contains(reason));
            } else {
                result.unwrap();
            }
        }
    }
    machine.value_limits = saved;
    (census.count, census.bytes)
}

fn distinct_local_source_clone(source: PrimitivePayload, candidate: PrimitivePayload) {
    assert_ne!(
        source.boxed, candidate.boxed,
        "Result local reads clone rather than consume the resident source"
    );
    assert_eq!(source.scalar, candidate.scalar);
    match (source.error, candidate.error) {
        (Some(a), Some(b)) => {
            for index in 0..3 {
                assert_ne!(a.entries[index], b.entries[index]);
            }
            for index in 0..6 {
                if a.buffers[index].1 > 0 && b.buffers[index].1 > 0 {
                    assert_ne!(
                        a.buffers[index].0, b.buffers[index].0,
                        "each nonempty error key/value buffer is physically cloned"
                    );
                }
            }
        }
        (None, None) => {}
        _ => panic!("clone retains the selected Result shape"),
    }
}

fn err_input_index(value: &Value, buffers: &[Buffer]) -> Option<usize> {
    let Value::Result { ok: false, value } = value else {
        return None;
    };
    let Value::Object(fields) = value.as_ref() else {
        return None;
    };
    if !matches!(fields.get("domain"), Some(Value::String(domain)) if domain == "ku")
        || !matches!(fields.get("code"), Some(Value::String(code)) if code == "err")
    {
        return None;
    }
    let Value::String(message) = fields.get("message")? else {
        return None;
    };
    buffers
        .iter()
        .position(|buffer| buffer.text == message.as_str())
}

pub(super) fn result_buffer(value: &Value, buffers: &[Buffer]) -> (usize, usize) {
    if let Some(index) = err_input_index(value, buffers) {
        let actual = error_payload(value).expect("the source err has its actual ErrorObject");
        if !buffers[index].text.is_empty() {
            assert_ne!(
                actual.buffers[5].0, buffers[index].pointer,
                "source err preserves its existing message.clone contract"
            );
        }
        return (index, actual.boxed);
    }
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

pub(super) fn observe(
    machine: &Machine,
    buffers: &[Buffer],
    boxes: &mut [Option<usize>],
    errors: &mut Vec<ErrorPayload>,
) -> usize {
    let mut pointers = Vec::new();
    let mut error_count = 0;
    let mut error_bytes = 0;
    let mut observe = |value: &Value| {
        if let Value::Result { ok, value: leaf } = value {
            let pointer = leaf.as_ref() as *const Value as usize;
            if *ok {
                if scalar_leaf(leaf).is_none() {
                    let (index, actual) = result_buffer(value, buffers);
                    assert_eq!(actual, pointer);
                    if let Some(previous) = boxes[index] {
                        assert_eq!(pointer, previous, "an actual saved Box must move intact");
                    } else {
                        boxes[index] = Some(pointer);
                    }
                }
            } else {
                let (count, bytes) = error_backing(leaf);
                error_count += count;
                error_bytes += bytes;
                let actual = error_payload(value).expect("the real false Result backing");
                if let Some(previous) = errors.iter().find(|previous| previous.boxed == pointer) {
                    assert_eq!(
                        actual, *previous,
                        "the same Box, Map entries and six String buffers move intact"
                    );
                } else {
                    errors.push(actual);
                }
                if let Some(index) = err_input_index(value, buffers) {
                    assert_eq!(result_buffer(value, buffers), (index, pointer));
                    if let Some(previous) = boxes[index] {
                        assert_eq!(pointer, previous, "a source err relay moves its actual Box");
                    } else {
                        boxes[index] = Some(pointer);
                    }
                }
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
    if let Some(frame) = &machine.awaiting {
        if let Some(Ok(value)) = frame.ticket.test_outcome() {
            observe(value);
        }
    }
    // Stable dispatch boundaries have no operation-local result. Escrow is
    // observed separately by the S2 kernel probes inside finish_with.
    assert_eq!(
        machine.await_payload_escrow,
        crate::runtime::task::AwaitPayloadCharge::default()
    );
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
    assert_eq!(
        (machine.error_leaf_capacity, machine.error_leaf_bytes),
        (error_count, error_bytes)
    );
    assert_eq!(
        (
            machine.metrics.retained_error_leaf_capacity,
            machine.metrics.retained_error_leaf_bytes
        ),
        (error_count, error_bytes)
    );
    count
}

pub(super) fn error_backing(value: &Value) -> (usize, usize) {
    let Value::Object(fields) = value else {
        panic!("an awaited task failure retains its actual ErrorObject")
    };
    assert_eq!(fields.len(), 3);
    let mut bytes = fields.capacity() * std::mem::size_of::<(String, Value)>();
    for name in ["domain", "code", "message"] {
        let (key, Value::String(text)) = fields
            .get_key_value(name)
            .expect("the existing task ErrorObject schema")
        else {
            panic!("the actual ErrorObject values are Strings")
        };
        bytes += key.capacity() + text.capacity();
    }
    (fields.capacity(), bytes)
}

pub(super) fn drained(machine: &Machine) {
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
    assert_eq!(
        (machine.error_leaf_capacity, machine.error_leaf_bytes),
        (0, 0)
    );
    assert_eq!(
        (
            machine.metrics.retained_error_leaf_capacity,
            machine.metrics.retained_error_leaf_bytes
        ),
        (0, 0)
    );
    assert_eq!(
        machine.await_payload_snapshot,
        crate::runtime::task::AwaitPayloadCharge::default()
    );
    assert_eq!(
        machine.await_payload_escrow,
        crate::runtime::task::AwaitPayloadCharge::default()
    );
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
    FuelBefore,
    FuelAfter,
    LocalCloneCount,
    LocalCloneBytes,
}

#[derive(Clone, Copy)]
struct Plan {
    chunk: usize,
    pause: Pause,
    fault: Fault,
    selection_only: bool,
    local_mode: bool,
    pressure_mode: bool,
}

impl Plan {
    fn normal(chunk: usize) -> Self {
        Self {
            chunk,
            pause: Pause::None,
            fault: Fault::None,
            selection_only: false,
            local_mode: false,
            pressure_mode: false,
        }
    }

    fn local(chunk: usize) -> Self {
        Self {
            local_mode: true,
            ..Self::normal(chunk)
        }
    }

    fn local_pressure(chunk: usize) -> Self {
        Self {
            pressure_mode: true,
            ..Self::local(chunk)
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
    peak_errors: usize,
    peak_error_bytes: usize,
    outgoing: Option<(usize, usize)>,
    outgoing_error: Option<ErrorPayload>,
    error_owners: Vec<ErrorPayload>,
    outgoing_primitive: Option<PrimitivePayload>,
    primitive_owners: Vec<PrimitivePayload>,
    last_primitive_candidate: Option<PrimitivePayload>,
    locals: Option<LocalSourceProof>,
}

#[derive(Debug)]
enum Witness {
    LocalProgress {
        popped: usize,
        calls: usize,
        boxes: usize,
    },
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
    Finished(Box<Finished>),
}

struct SourceRunner {
    inner: ClaimedNamedTask,
    plan: Plan,
    buffers: Vec<Buffer>,
    boxes: Vec<Option<usize>>,
    errors: Vec<ErrorPayload>,
    primitives: Vec<PrimitivePayload>,
    last_primitive_candidate: Option<PrimitivePayload>,
    locals: Option<LocalSourceProof>,
    witness: mpsc::Sender<Witness>,
    release: Option<mpsc::Receiver<()>>,
    paused: bool,
    faulted: bool,
    dispatches: usize,
    thread: std::thread::ThreadId,
}

impl SourceRunner {
    fn dispatch_pressure(&mut self, budget: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        // A pending first failure can physically pop skipped Work without
        // incrementing metrics.dispatches. Observe the real kernel receipt
        // instead, while retaining the claimant's task/finish conversion.
        // This fixture has no await; every Continue is an actual More receipt.
        assert!(
            !self.inner.finished,
            "a finished pressure claimant cannot resume"
        );
        assert_eq!(crate::runtime::task::current_task_id(), self.inner.task_id);
        let result = if let Some(outcome) = self.inner.outcome.take() {
            outcome
        } else {
            let machine = self
                .inner
                .machine
                .as_mut()
                .expect("the selected pressure machine");
            match machine.dispatch_chunk(budget, &mut self.inner.interpreter) {
                Ok(ChunkProgress::More { popped }) => {
                    assert_eq!(popped, budget.get());
                    self.locals.as_mut().unwrap().popped += popped;
                    return ControlFlow::Continue(());
                }
                Ok(ChunkProgress::Waiting { .. }) => {
                    panic!("the synchronous Relay pressure fixture has no await")
                }
                Ok(ChunkProgress::Finished { popped, result }) => {
                    assert!(popped <= budget.get());
                    self.locals.as_mut().unwrap().popped += popped;
                    let kind = if machine.captures.is_some() {
                        CallKind::FunctionValue
                    } else {
                        CallKind::Named
                    };
                    result.and_then(|flow| flow.into_call_value(self.inner.span, kind.label()))
                }
                Err(error) => Err(error),
            }
        };
        self.inner.finished = true;
        ControlFlow::Break(result)
    }

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
        let mut clone_source = None;
        let mut saved_local_limits = None;
        if let (Some(machine), Some(proof)) = (self.inner.machine.as_mut(), self.locals.as_mut()) {
            if self.plan.pressure_mode {
                let depth = machine.calls.len();
                assert!(depth <= MAX_CALL_DEPTH);
                if !proof.full_depth_observed
                    && depth != 0
                    && (depth >= proof.peak_env + 64 || depth == MAX_CALL_DEPTH)
                    && machine
                        .calls
                        .as_slice()
                        .last()
                        .unwrap()
                        .has_selected_result_bindings
                {
                    let census = observe_source_locals(machine, &self.buffers, proof);
                    assert_eq!(
                        census.env.len(),
                        depth,
                        "one genuine keep Result in every live call at this checkpoint"
                    );
                    if depth == MAX_CALL_DEPTH {
                        local_source_budget_entries(machine, &self.buffers);
                        proof.budget_checks += 1;
                        proof.full_depth_observed = true;
                    }
                }
                if proof.full_depth_observed && depth > 32 {
                    self.plan.chunk = 256;
                    proof.live.clear();
                    proof.last_candidate = None;
                } else if !proof.full_depth_observed {
                    // One real Work can enter at most one call. This budget
                    // cannot cross 512; at 512 it remains one step until the
                    // first keep Bind is observable, before a 513th attempt or
                    // leaf effects. Only outer dispatch observation is batched.
                    self.plan.chunk = (MAX_CALL_DEPTH - depth).clamp(1, 256);
                } else {
                    self.plan.chunk = 1;
                }
            }
            if !self.plan.pressure_mode || (proof.full_depth_observed && machine.calls.len() <= 32)
            {
                observe_source_locals(machine, &self.buffers, proof);
                if self.plan.chunk == 1 && machine.pending.is_none() {
                    if let Some(Work::CloneSelectedScalarResult(name, _)) =
                        machine.work.as_slice().last()
                    {
                        let name = *name;
                        clone_source = machine
                            .calls
                            .as_slice()
                            .last()
                            .unwrap()
                            .env
                            .with_value(machine.code.symbol(name), Span::default(), |value| {
                                Ok(primitive_payload(value))
                            })
                            .unwrap();
                        if proof.budget_checks == 0 {
                            machine.reserve_values(1, Span::default()).unwrap();
                            local_source_budget_entries(machine, &self.buffers);
                            proof.budget_checks += 1;
                        }
                        if !self.faulted
                            && matches!(
                                self.plan.fault,
                                Fault::LocalCloneCount | Fault::LocalCloneBytes
                            )
                        {
                            machine.reserve_values(1, Span::default()).unwrap();
                            let (count, bytes) =
                                local_source_budget_entries(machine, &self.buffers);
                            let minimum = machine
                                .calls
                                .as_slice()
                                .last()
                                .unwrap()
                                .env
                                .with_value(machine.code.symbol(name), Span::default(), |value| {
                                    Ok(planned_local_clone_charge(value))
                                })
                                .unwrap()
                                .unwrap();
                            proof.budget_checks += 1;
                            saved_local_limits = Some(machine.value_limits);
                            if self.plan.fault == Fault::LocalCloneCount {
                                machine.value_limits.max_frames = count + minimum.count - 1;
                            } else {
                                machine.value_limits.max_storage_bytes = bytes + minimum.bytes - 1;
                            }
                            proof.clone_source = clone_source;
                            self.faulted = true;
                        }
                    }
                }
            }
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
                        Some(
                            Work::WrapTerminalOk(_, true)
                                | Work::WrapTerminalErr(_)
                                | Work::WrapOwnedScalarResult(_, _)
                        )
                    );
                if at_wrap && matches!(self.plan.fault, Fault::WrapperCount | Fault::WrapperBytes) {
                    primitive_budget_entries(machine);
                    let operand = machine.values.get(machine.values.len() - 1).unwrap();
                    let input = self.buffers.first().copied().unwrap_or(Buffer {
                        pointer: 0,
                        capacity: 0,
                        text: "",
                    });
                    if let Value::String(text) = operand {
                        assert_eq!(
                            (text.as_ptr() as usize, text.capacity()),
                            (input.pointer, input.capacity)
                        );
                    } else {
                        assert!(
                            scalar_leaf(operand).is_some(),
                            "the real primitive operand is ready before Box admission"
                        );
                    }
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
                        machine.value_limits.max_frames += machine.error_leaf_capacity;
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
                                + machine.error_leaf_bytes
                                + std::mem::size_of::<Value>()
                                - 1;
                    }
                    self.faulted = true;
                }
                if (self.plan.fault == Fault::FuelBefore && at_wrap)
                    || (self.plan.fault == Fault::FuelAfter && machine.result_wrapper_capacity > 0)
                {
                    machine.test_fuel = true;
                    machine.remaining_dispatches = 0;
                    self.faulted = true;
                }
                if self.plan.fault == Fault::FuelBefore
                    && machine.pending.is_none()
                    && matches!(
                        machine.work.as_slice().last(),
                        Some(Work::AwaitOwnedStringOperand(_))
                    )
                {
                    machine.test_fuel = true;
                    machine.remaining_dispatches = 0;
                    self.faulted = true;
                }
                if self.plan.fault == Fault::NativeBox && machine.result_wrapper_capacity > 0 {
                    let boxed = if let Some(proof) = &mut self.locals {
                        let census = observe_source_locals(machine, &self.buffers, proof);
                        local_source_budget_entries(machine, &self.buffers);
                        proof.budget_checks += 1;
                        census.env.first().or(census.live.first()).unwrap().boxed
                    } else {
                        let count =
                            observe(machine, &self.buffers, &mut self.boxes, &mut self.errors);
                        assert_eq!(count, 1);
                        primitive_budget_entries(machine);
                        if let Some(actual) = observe_primitives(machine, &mut self.primitives) {
                            self.last_primitive_candidate = Some(actual);
                        }
                        self.primitives
                            .last()
                            .map_or_else(|| self.boxes[0].unwrap(), |actual| actual.boxed)
                    };
                    self.witness
                        .send(Witness::Native {
                            input: self.buffers.first().copied().unwrap_or(Buffer {
                                pointer: 0,
                                capacity: 0,
                                text: "",
                            }),
                            boxed,
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
        let budget = NonZeroUsize::new(self.plan.chunk).unwrap();
        let result = if self.plan.pressure_mode {
            self.dispatch_pressure(budget)
        } else {
            self.inner.dispatch(budget)
        };
        if let Some(saved) = saved_local_limits {
            self.inner.machine.as_mut().unwrap().value_limits = saved;
        }
        if let Some(machine) = self.inner.machine.as_ref() {
            if machine.state != MachineState::Finished {
                assert_eq!(
                    machine.string_payload_bytes,
                    self.buffers
                        .iter()
                        .map(|buffer| buffer.capacity)
                        .sum::<usize>(),
                    "the original input pool remains reserved until this actual run finishes"
                );
            }
            if let Some(proof) = &mut self.locals {
                if machine.state != MachineState::Finished
                    && (!self.plan.pressure_mode
                        || (proof.full_depth_observed && machine.calls.len() <= 32))
                {
                    let census = observe_source_locals(machine, &self.buffers, proof);
                    if let Some(source) = clone_source {
                        assert!(
                            census.env.contains(&source),
                            "the real clone leaves its original source owner intact"
                        );
                        proof.clone_source_unchanged = true;
                        if let Some(candidate) =
                            machine.values.as_slice().last().and_then(primitive_payload)
                        {
                            distinct_local_source_clone(source, candidate);
                            // Record the actual completed clone, while its
                            // independently measured source is still resident.
                            // Never infer a new candidate from address history.
                            proof.last_candidate = Some(candidate);
                            proof.distinct_clones += 1;
                        } else {
                            assert!(
                                saved_local_limits.is_some(),
                                "only the controlled quota rejection omits publication"
                            );
                            assert_eq!(census.live.len(), census.env.len());
                            proof.clone_source_unchanged = true;
                        }
                    }
                }
            } else {
                observe(machine, &self.buffers, &mut self.boxes, &mut self.errors);
                if let Some(actual) = observe_primitives(machine, &mut self.primitives) {
                    self.last_primitive_candidate = Some(actual);
                }
            }
        }
        if matches!(result, ControlFlow::Continue(())) && self.plan.pressure_mode {
            if let (Some(machine), Some(proof)) =
                (self.inner.machine.as_ref(), self.locals.as_mut())
            {
                // Every real More receipt advances popped, including skipped
                // Work after the first failure. Depth-only milestones can be
                // separated by substantial genuine work and do not prove a
                // stalled receiver or a stalled worker.
                self.witness
                    .send(Witness::LocalProgress {
                        popped: proof.popped,
                        calls: machine.calls.len(),
                        boxes: machine.result_wrapper_capacity,
                    })
                    .unwrap();
            }
        }
        if let ControlFlow::Break(outcome) = &result {
            let outgoing = outcome.as_ref().ok().and_then(|value| {
                if matches!(value, Value::Result { ok: true, value } if scalar_leaf(value).is_none())
                    || err_input_index(value, &self.buffers).is_some()
                {
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
            let outgoing_error = outcome.as_ref().ok().and_then(error_payload);
            let outgoing_primitive = outcome.as_ref().ok().and_then(primitive_payload);
            if let Some(actual) = outgoing_primitive {
                if self.plan.chunk == 1 {
                    if let Some(proof) = &self.locals {
                        assert_eq!(proof.last_candidate, Some(actual),
                            "one-step local claimant publishes its independently observed final candidate");
                        assert!(
                            proof.live.contains(&actual),
                            "the outgoing physical owner existed before teardown"
                        );
                    } else {
                        assert_eq!(
                            self.primitives
                                .iter()
                                .find(|old| old.boxed == actual.boxed)
                                .copied(),
                            Some(actual),
                            "one-step claimant independently observes its outgoing primitive Box"
                        );
                    }
                }
            }
            if let Some(actual) = outgoing_error {
                if let Some(previous) = self
                    .errors
                    .iter()
                    .find(|previous| previous.boxed == actual.boxed)
                {
                    assert_eq!(
                        actual, *previous,
                        "the published false Result keeps its Box, Map and String backing"
                    );
                }
            }
            let machine = self.inner.machine.as_ref();
            if let Some(machine) = machine {
                drained(machine);
            }
            assert_eq!(self.inner.interpreter.call_depth, 0);
            self.witness
                .send(Witness::Finished(Box::new(Finished {
                    steps: self.inner.interpreter.steps,
                    calls: machine.map_or(0, |m| m.metrics.entered_calls),
                    peak_calls: machine.map_or(0, |m| m.metrics.peak_calls),
                    finally: machine.map_or(0, |m| m.metrics.entered_finally_frames),
                    peak_boxes: machine.map_or(0, |m| m.metrics.peak_result_wrapper_capacity),
                    peak_box_bytes: machine.map_or(0, |m| m.metrics.peak_result_wrapper_bytes),
                    peak_strings: machine.map_or(0, |m| m.metrics.peak_string_payload_bytes),
                    peak_args: machine.map_or(0, |m| m.peak_argument_bytes),
                    peak_errors: machine.map_or(0, |m| m.metrics.peak_error_leaf_capacity),
                    peak_error_bytes: machine.map_or(0, |m| m.metrics.peak_error_leaf_bytes),
                    outgoing,
                    outgoing_error,
                    error_owners: self.errors.clone(),
                    outgoing_primitive,
                    primitive_owners: self.primitives.clone(),
                    last_primitive_candidate: self.last_primitive_candidate,
                    locals: self.locals.take(),
                })))
                .unwrap();
            if self.plan.pause == Pause::Produced && !self.paused {
                self.pause(Pause::Produced, outgoing);
            }
        } else if self.plan.pause == Pause::Running
            && !self.paused
            && self.inner.interpreter.steps > 0
            && (!self.plan.local_mode
                || self.locals.as_ref().is_some_and(|proof| proof.peak_env > 0))
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
                    errors: Vec::new(),
                    primitives: Vec::new(),
                    last_primitive_candidate: None,
                    locals: plan.local_mode.then(LocalSourceProof::default),
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
    *finished
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
        // The root is already cached as async: its first child call must
        // reject a synchronous dependency role before the later await node.
        "async fn Entry(text: str): str! { child = Entry(text)\nreturn await child }",
        "fn Leaf(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return ok(Leaf(text)?) }",
        "async fn Entry(text: str): str! { result = ok(text)\nreturn result }",
        "async fn Entry(text: str): str! { result = err(text)\nreturn result }",
        "fn err(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return err(text) }",
        "fn Pass(value: str!): str! { return value }\nasync fn Entry(text: str): str! { return Pass(ok(text)) }",
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

fn err_source_receipt(
    source: &str,
    args: Vec<Value>,
    buffers: Vec<Buffer>,
    plan: Plan,
) -> (Finished, Vec<Witness>, KuResult<Value>) {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (task, rx, _release) = spawn_source(&runtime, source, args, buffers, plan);
    let id = task.id();
    let constructor = rx.recv_timeout(WAIT).unwrap();
    let mut proof = None;
    let mut other = Vec::new();
    let mut progress_popped = 0;
    for _ in 0..if plan.pressure_mode { 100_000 } else { 4 } {
        match rx.recv_timeout(WAIT).unwrap() {
            Witness::Finished(actual) => {
                proof = Some(*actual);
                break;
            }
            Witness::LocalProgress {
                popped,
                calls,
                boxes,
            } => {
                assert!(
                    plan.pressure_mode && popped > progress_popped,
                    "pressure progress comes only from real completed dispatch work"
                );
                assert!(calls <= MAX_CALL_DEPTH);
                assert!(boxes <= calls * 4 + 3);
                progress_popped = popped;
            }
            event => other.push(event),
        }
    }
    let result = task.await_result();
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
    // All actual factory/queue/completion owners have retired before the
    // caller makes semantic assertions about the source and its payload.
    let Witness::Entered {
        task,
        selected,
        legacy,
        pending_error,
        steps,
        calls,
    } = constructor
    else {
        panic!("the retired claimant must report its actual constructor");
    };
    assert_eq!(task, id);
    assert!(selected && !legacy && !pending_error);
    assert_eq!((steps, calls), (0, 1));
    (
        proof.expect("the real source machine finished"),
        other,
        result,
    )
}

fn assert_source_error(result: &Value, proof: &Finished, buffers: &[Buffer]) -> ErrorPayload {
    let actual = error_payload(result).expect("the source returns a false ErrorObject Result");
    assert_eq!(proof.outgoing, Some(result_buffer(result, buffers)));
    assert_eq!(proof.outgoing_error, Some(actual));
    assert_eq!(
        proof.peak_strings,
        buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
    );
    actual
}

#[test]
fn terminal_err_real_claimant_input_helper_and_sync_relay_keep_512_and_first_513() {
    const HELPER: &str = "fn Keep(text: str): str { try { marker = 1 } finally { marker = 2 }\nreturn text }\nasync fn Entry(text: str): str! { return err(Keep(text)) }";
    const FAILURE: &str = "fn Keep(text: str, ignored: int): str { return text }\nasync fn Entry(text: str): str! { return err(Keep(text, 1 / 0)) }";
    for chunk in [1, 7, 256] {
        for (source, n, calls, finalies) in [
            (ERR_SIMPLE, None, 1, 0),
            (HELPER, None, 2, 1),
            (ERR_RELAY, Some(510), 512, 0),
            (ERR_RELAY, Some(511), 512, 0),
            (FAILURE, None, 1, 0),
        ] {
            let source_functions = functions(source);
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
            let mut args = vec![text];
            if let Some(n) = n {
                args.push(Value::Int(n));
            }
            let (proof, events, result) =
                err_source_receipt(source, args, vec![buffer], Plan::normal(chunk));
            assert!(events.is_empty());
            assert_eq!(
                (proof.calls, proof.peak_calls, proof.finally),
                (calls, calls, finalies)
            );
            if n == Some(511) || source == FAILURE {
                let error = result.unwrap_err();
                if source == FAILURE {
                    assert_eq!(error.message, "division by zero");
                } else {
                    assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
                }
                assert_eq!((proof.peak_boxes, proof.peak_errors), (0, 0));
            } else {
                let actual = assert_source_error(&result.unwrap(), &proof, &[buffer]);
                assert_eq!(
                    (proof.peak_boxes, proof.peak_box_bytes),
                    (1, std::mem::size_of::<Value>())
                );
                assert_eq!(
                    (proof.peak_errors, proof.peak_error_bytes),
                    (actual.count, actual.bytes)
                );
            }
        }
    }
}

#[test]
fn terminal_err_real_claimant_preallocation_budgets_never_publish_candidate_box_or_map() {
    for fault in [Fault::WrapperCount, Fault::WrapperBytes] {
        let (text, buffer) = string_input(TEXT, 4096);
        let mut plan = Plan::normal(1);
        plan.fault = fault;
        let (proof, events, result) =
            err_source_receipt(ERR_SIMPLE, vec![text], vec![buffer], plan);
        let [Witness::Wrap { steps, input }] = events.as_slice() else {
            panic!("one actual preallocation checkpoint")
        };
        assert!(*steps > 0);
        assert_eq!(
            (input.pointer, input.capacity),
            (buffer.pointer, buffer.capacity)
        );
        assert!(result
            .unwrap_err()
            .message
            .contains(if fault == Fault::WrapperCount {
                "FrameLimit"
            } else {
                "StorageLimit"
            }));
        assert_eq!(
            (
                proof.peak_boxes,
                proof.peak_box_bytes,
                proof.peak_errors,
                proof.peak_error_bytes
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(proof.peak_strings, buffer.capacity);
        assert!(proof.outgoing.is_none() && proof.outgoing_error.is_none());
    }
}

#[test]
fn terminal_err_real_claimant_q2_q3_charge_each_saved_map_and_six_buffers() {
    const Q2: &str = "async fn Entry(first: str, second: str, third: str, fallback: str): str! { try { return err(first) } finally { return err(second) }\nreturn ok(fallback) }";
    const Q3: &str = "async fn Entry(first: str, second: str, third: str, fallback: str): str! { try { return err(first) } finally { try { return err(second) } finally { return err(third) } }\nreturn ok(fallback) }";
    for (source, owners) in [(Q2, 2), (Q3, 3)] {
        for chunk in [1, 7, 256] {
            let (first, a) = string_input("中\0primary", 1024);
            let (second, b) = string_input("é\0replacement", 4096);
            let (third, c) = string_input(TEXT, 2048);
            let (fallback, d) = string_input("\0fallback", 512);
            let buffers = [a, b, c, d];
            let (proof, events, result) = err_source_receipt(
                source,
                vec![first, second, third, fallback],
                buffers.to_vec(),
                Plan::normal(chunk),
            );
            assert!(events.is_empty());
            let result = result.unwrap();
            let actual = assert_source_error(&result, &proof, &buffers);
            assert_eq!(result_buffer(&result, &buffers).0, owners - 1);
            assert_eq!((proof.peak_boxes, proof.finally), (owners, owners - 1));
            assert_eq!(proof.peak_box_bytes, owners * std::mem::size_of::<Value>());
            // Single-work dispatches expose every real saved primary. A
            // larger chunk can create and retire those owners before the
            // next boundary, so it proves output/cleanup and reported peaks,
            // while the one-work run supplies independent physical evidence.
            if chunk != 1 {
                assert!(proof.peak_errors >= actual.count);
                assert!(proof.peak_error_bytes >= actual.bytes);
                continue;
            }
            assert_eq!(proof.error_owners.len(), owners);
            assert_eq!(
                (proof.peak_errors, proof.peak_error_bytes),
                (
                    proof.error_owners.iter().map(|p| p.count).sum::<usize>(),
                    proof.error_owners.iter().map(|p| p.bytes).sum::<usize>()
                )
            );
            for (index, owner) in proof.error_owners.iter().enumerate() {
                for other in &proof.error_owners[..index] {
                    assert_ne!(owner.boxed, other.boxed);
                    assert!(owner
                        .entries
                        .iter()
                        .all(|entry| !other.entries.contains(entry)));
                }
            }
        }
    }
}

#[test]
fn terminal_err_uncommitted_late_result_keeps_actual_completion_and_queue_receipts() {
    for cancel in [false, true] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(TEXT, 4096);
        let mut plan = Plan::normal(1);
        plan.pause = Pause::Produced;
        let (task, rx, release) =
            spawn_source(&runtime, ERR_SIMPLE, vec![text], vec![buffer], plan);
        let initial = (0..3)
            .map(|_| rx.recv_timeout(WAIT).unwrap())
            .collect::<Vec<_>>();
        let context = CancellationContext::new(TerminationReason::TimedOut);
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: context.cleanup_deadline - Duration::from_millis(1),
        };
        let expected = CancellationContext {
            reason: context.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        let accepted = cancel.then(|| task.request_cancel_with(context));
        let tightened = cancel.then(|| task.request_cancel_with(shorter));
        let witness = task.retirement_witness();
        let mut owner = Some(task);
        let mut batch = TaskRetirementBatch::from_reserved(
            vec![witness],
            if cancel { expected } else { context },
        );
        let before = if cancel {
            None
        } else {
            drop(owner.take());
            Some(batch.poll(Instant::now()))
        };
        release.send(()).unwrap();
        let result = if cancel {
            let task = owner.take().unwrap();
            let result = task.await_result();
            drop(task);
            Some(result)
        } else {
            None
        };
        retired(batch);
        close(&runtime);
        let [Witness::Entered {
            selected: true,
            legacy: false,
            ..
        }, Witness::Finished(proof), Witness::Paused {
            stage: Pause::Produced,
            outgoing,
            ..
        }] = initial.as_slice()
        else {
            panic!("actual produced false Result checkpoint")
        };
        assert!(proof.outgoing_error.is_some());
        assert_eq!(*outgoing, proof.outgoing);
        assert_eq!(proof.peak_boxes, 1);
        if cancel {
            assert_eq!((accepted, tightened), (Some(true), Some(false)));
            assert_eq!(
                result.unwrap().unwrap_err().runtime_termination(),
                Some(expected)
            );
        } else {
            assert!(
                matches!(before, Some(RetirementBatchPoll::Pending { .. })),
                "last owner release cannot acknowledge the held real err Map or running queue node"
            );
        }
    }
}

#[test]
fn terminal_err_native_map_abandonment_restores_the_same_real_worker_for_next_claimant() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    plan.fault = Fault::NativeBox;
    let (task, rx, _release) = spawn_source(&runtime, ERR_SIMPLE, vec![text], vec![buffer], plan);
    let native_constructor = rx.recv_timeout(WAIT).unwrap();
    let native_checkpoint = rx.recv_timeout(WAIT).unwrap();
    let result = task.await_result();
    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
    let mut native_events = vec![native_constructor, native_checkpoint];
    native_events.extend(rx);
    let (text, healthy_buffer) = string_input(TEXT, 4096);
    let (healthy, healthy_rx, _release) = spawn_source(
        &runtime,
        ERR_SIMPLE,
        vec![text],
        vec![healthy_buffer],
        Plan::normal(1),
    );
    let healthy_constructor = healthy_rx.recv_timeout(WAIT).unwrap();
    let healthy_completion = healthy_rx.recv_timeout(WAIT).unwrap();
    let healthy_result = healthy.await_result();
    release_task(
        healthy,
        CancellationContext::new(TerminationReason::Cancelled),
    );
    close(&runtime);
    let result = result.unwrap();
    let healthy_result = healthy_result.unwrap();
    let mut healthy_events = vec![healthy_constructor, healthy_completion];
    healthy_events.extend(healthy_rx);
    let [Witness::Entered { selected: true, .. }, Witness::Native { input, boxed }] =
        native_events.as_slice()
    else {
        panic!("the native kernel abandoned its real false Result")
    };
    assert_eq!(
        (input.pointer, input.capacity),
        (buffer.pointer, buffer.capacity)
    );
    assert_ne!(*boxed, 0);
    let Value::Result { ok: false, value } = result else {
        panic!("existing native panic classification")
    };
    let Value::Object(fields) = *value else {
        unreachable!()
    };
    assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
    let [Witness::Entered { selected: true, .. }, Witness::Finished(proof)] =
        healthy_events.as_slice()
    else {
        panic!("the same fixed worker starts healthy with restored TLS")
    };
    assert_source_error(&healthy_result, proof, &[healthy_buffer]);
}

#[test]
fn terminal_err_queued_and_running_cancellation_retire_input_before_result_allocation() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (held_tx, held_rx) = mpsc::channel();
    let held = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            held_rx.recv_timeout(WAIT).expect("finite held worker");
            Ok(Value::Null)
        })
        .unwrap();
    ready_rx.recv_timeout(WAIT).unwrap();
    let (text, buffer) = string_input(TEXT, 4096);
    let (task, rx, _release) = spawn_source(
        &runtime,
        ERR_SIMPLE,
        vec![text],
        vec![buffer],
        Plan::normal(1),
    );
    let queued_before = runtime.snapshot().unwrap().queued_tasks;
    let context = CancellationContext::new(TerminationReason::TimedOut);
    let accepted = task.request_cancel_with(context);
    let result = task.await_result();
    release_task(task, context);
    let abandoned_factory = rx.recv_timeout(WAIT);
    let queued_after = runtime.snapshot().unwrap().queued_tasks;
    held_tx.send(()).unwrap();
    let held_result = held.await_result();
    release_task(held, CancellationContext::new(TerminationReason::Cancelled));
    close(&runtime);
    assert_eq!((queued_before, queued_after), (1, 0));
    assert!(accepted);
    assert!(matches!(
        abandoned_factory,
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    assert_eq!(result.unwrap_err().runtime_termination(), Some(context));
    assert_eq!(held_result.unwrap(), Value::Null);

    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(TEXT, 4096);
    let mut plan = Plan::normal(1);
    plan.pause = Pause::Running;
    let (task, rx, release) = spawn_source(&runtime, ERR_SIMPLE, vec![text], vec![buffer], plan);
    let constructor = rx.recv_timeout(WAIT).unwrap();
    let paused = rx.recv_timeout(WAIT).unwrap();
    let accepted = task.request_cancel_with(context);
    release.send(()).unwrap();
    let result = task.await_result();
    release_task(task, context);
    close(&runtime);
    let proof = finished(&rx);
    assert!(matches!(
        constructor,
        Witness::Entered {
            selected: true,
            legacy: false,
            ..
        }
    ));
    assert!(matches!(
        paused,
        Witness::Paused {
            stage: Pause::Running,
            outgoing: None,
            ..
        }
    ));
    assert!(accepted);
    assert_eq!(result.unwrap_err().runtime_termination(), Some(context));
    assert_eq!(
        (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
        (0, 0, 0)
    );
    assert!(proof.outgoing_error.is_none());
}

const ASYNC_PRIMITIVES: [(code::PrimitiveResultKind, &str, &str); 4] = [
    (code::PrimitiveResultKind::Int, "int", "7"),
    (code::PrimitiveResultKind::Float, "float", "1.25"),
    (code::PrimitiveResultKind::Bool, "bool", "true"),
    (code::PrimitiveResultKind::Null, "null", "null"),
];

fn expected_scalar(kind: code::PrimitiveResultKind) -> ScalarLeaf {
    match kind {
        code::PrimitiveResultKind::Int => ScalarLeaf::Int(7),
        code::PrimitiveResultKind::Float => ScalarLeaf::Float(1.25_f64.to_bits()),
        code::PrimitiveResultKind::Bool => ScalarLeaf::Bool(true),
        code::PrimitiveResultKind::Null => ScalarLeaf::Null,
    }
}

fn assert_primitive_source(
    value: &Value,
    proof: &Finished,
    expected: ScalarLeaf,
    chunk: usize,
) -> PrimitivePayload {
    let actual = primitive_payload(value).expect("the real outgoing scalar success Box");
    assert_eq!(actual.scalar, Some(expected));
    assert_eq!(actual.error, None);
    assert_eq!(proof.outgoing_primitive, Some(actual));
    assert_eq!(
        (
            proof.peak_boxes,
            proof.peak_box_bytes,
            proof.peak_errors,
            proof.peak_error_bytes
        ),
        (1, std::mem::size_of::<Value>(), 0, 0)
    );
    if chunk == 1 {
        assert!(proof.primitive_owners.contains(&actual));
    }
    actual
}

#[test]
fn claimed_async_primitive_actual_leaves_helpers_and_sync_relays_keep_512_and_first_513() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        let read = match kind {
            code::PrimitiveResultKind::Int => "text.len()",
            code::PrimitiveResultKind::Bool => "text.byte_len() > 0",
            _ => literal,
        };
        let read_expected = if kind == code::PrimitiveResultKind::Int {
            ScalarLeaf::Int(TEXT.chars().count() as i64)
        } else {
            expected_scalar(kind)
        };
        for (body, helper, expected) in [
            (
                format!("ok({literal})"),
                String::new(),
                expected_scalar(kind),
            ),
            (
                "ok(Produce(text))".to_string(),
                format!("fn Produce(text: str): {ty} {{ return {read} }}\n"),
                read_expected,
            ),
            (
                "Leaf(text)".to_string(),
                format!("fn Leaf(text: str): {ty}! {{ return ok({read}) }}\n"),
                read_expected,
            ),
        ] {
            let source = format!("{helper}async fn Entry(text: str): {ty}! {{ return {body} }}");
            for chunk in [1, 7, 256] {
                let (text, buffer) = string_input(TEXT, 4096);
                let (proof, events, result) =
                    err_source_receipt(&source, vec![text], vec![buffer], Plan::normal(chunk));
                assert!(events.is_empty());
                assert_primitive_source(&result.unwrap(), &proof, expected, chunk);
                assert_eq!(proof.peak_strings, buffer.capacity);
            }
        }
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({read})")
            };
            for mutual in [false, true] {
                let helper = if mutual {
                    format!("fn Other(text: str, n: int): {ty}! {{ if (n <= 0) {{ return {leaf} }}\nreturn Relay(text, n - 1) }}\n")
                } else {
                    String::new()
                };
                let next = if mutual { "Other" } else { "Relay" };
                let source = format!("{helper}fn Relay(text: str, n: int): {ty}! {{ if (n <= 0) {{ return {leaf} }}\nreturn {next}(text, n - 1) }}\nasync fn Entry(text: str, n: int): {ty}! {{ return Relay(text, n) }}");
                for n in [510, 511] {
                    let (text, buffer) = string_input(TEXT, 4096);
                    let (proof, _, result) = err_source_receipt(
                        &source,
                        vec![text, Value::Int(n)],
                        vec![buffer],
                        Plan::normal(1),
                    );
                    assert_eq!(proof.peak_calls, 512);
                    if n == 510 {
                        let result = result.unwrap();
                        if error {
                            assert_source_error(&result, &proof, &[buffer]);
                        } else {
                            assert_primitive_source(&result, &proof, read_expected, 1);
                        }
                        assert_eq!(proof.calls, 512);
                    } else {
                        assert_eq!(
                            result.unwrap_err().code.as_deref(),
                            Some("call_depth_exceeded")
                        );
                        assert_eq!(
                            (proof.calls, proof.peak_boxes, proof.peak_errors),
                            (512, 0, 0)
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn claimed_async_primitive_input_capacity_and_four_budget_entries_reject_before_box() {
    for (_, ty, literal) in ASYNC_PRIMITIVES {
        let source = format!("async fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(TEXT, Limits::default().values.max_storage_bytes + 1);
        let (task, rx, _) =
            spawn_source(&runtime, &source, vec![text], vec![buffer], Plan::normal(1));
        let constructor = rx.recv_timeout(WAIT).unwrap();
        let completion = rx.recv_timeout(WAIT).unwrap();
        let result = task.await_result();
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
        assert!(matches!(
            constructor,
            Witness::Entered {
                selected: true,
                pending_error: true,
                steps: 0,
                calls: 0,
                ..
            }
        ));
        let Witness::Finished(proof) = completion else {
            panic!("rejected real entry drains")
        };
        assert!(result.unwrap_err().message.contains("StorageLimit"));
        assert_eq!(
            (
                proof.steps,
                proof.calls,
                proof.peak_boxes,
                proof.peak_errors
            ),
            (0, 0, 0, 0)
        );
        for fault in [Fault::WrapperCount, Fault::WrapperBytes] {
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::normal(1);
            plan.fault = fault;
            let (proof, events, result) =
                err_source_receipt(&source, vec![text], vec![buffer], plan);
            assert!(
                matches!(events.as_slice(), [Witness::Wrap { steps, input }] if *steps > 0 && input.pointer == buffer.pointer && input.capacity == buffer.capacity)
            );
            assert!(result
                .unwrap_err()
                .message
                .contains(if fault == Fault::WrapperCount {
                    "FrameLimit"
                } else {
                    "StorageLimit"
                }));
            assert_eq!((proof.peak_boxes, proof.peak_errors), (0, 0));
            assert_eq!(proof.peak_strings, buffer.capacity);
        }
    }
}

#[test]
fn claimed_async_primitive_err_clones_real_message_and_charges_box_map_and_six_buffers() {
    for (_, ty, _) in ASYNC_PRIMITIVES {
        let source = format!("fn Keep(text: str): str {{ try {{ marker = 0 }} finally {{ marker = 1 }}\nreturn text }}\nasync fn Entry(text: str): {ty}! {{ return err(Keep(text)) }}");
        let source_functions = functions(&source);
        code::admit_claimed_async_owned_scalar_result_entry(
            &source_functions,
            &source_functions["Entry"],
            Limits::default(),
        )
        .unwrap();
        CodeHandle::compile_claimed_async_owned_scalar_result_entry(
            &source_functions,
            &source_functions["Entry"],
            Limits::default(),
        )
        .unwrap();
        for chunk in [1, 7, 256] {
            let (text, buffer) = string_input(TEXT, 4096);
            let (proof, _, result) =
                err_source_receipt(&source, vec![text], vec![buffer], Plan::normal(chunk));
            let result = result.unwrap();
            let actual = assert_source_error(&result, &proof, &[buffer]);
            assert_eq!(
                (
                    proof.peak_boxes,
                    proof.peak_box_bytes,
                    proof.peak_errors,
                    proof.peak_error_bytes
                ),
                (1, std::mem::size_of::<Value>(), actual.count, actual.bytes)
            );
            assert_eq!(proof.finally, 1);
            if chunk == 1 {
                assert!(proof
                    .primitive_owners
                    .contains(&primitive_payload(&result).unwrap()));
            }
        }
        for fault in [Fault::WrapperCount, Fault::WrapperBytes] {
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::normal(1);
            plan.fault = fault;
            let (proof, _, result) = err_source_receipt(&source, vec![text], vec![buffer], plan);
            assert!(result.is_err());
            assert_eq!(
                (proof.peak_boxes, proof.peak_errors, proof.peak_error_bytes),
                (0, 0, 0)
            );
        }
    }
}

#[test]
fn claimed_async_primitive_q2_q3_return_the_last_independently_observed_finally_box() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        for (errors, owners) in [
            ([false, false, false], 2),
            ([true, true, false], 2),
            ([true, false, false], 2),
            ([false, true, false], 2),
            ([true, false, true], 3),
        ] {
            let leaf = |index: usize| {
                if errors[index] {
                    format!("err({})", ["first", "second", "third"][index])
                } else {
                    format!("ok({literal})")
                }
            };
            let replacement = if owners == 2 {
                format!("return {}", leaf(1))
            } else {
                format!(
                    "try {{ return {} }} finally {{ return {} }}",
                    leaf(1),
                    leaf(2)
                )
            };
            let source = format!("async fn Entry(first: str, second: str, third: str): {ty}! {{ try {{ return {} }} finally {{ {replacement} }}\nreturn ok({literal}) }}", leaf(0));
            for chunk in [1, 7, 256] {
                let (a, first) = string_input("中\0primary", 1024);
                let (b, second) = string_input("é\0replacement", 4096);
                let (c, third) = string_input(TEXT, 2048);
                let buffers = [first, second, third];
                let (proof, _, result) = err_source_receipt(
                    &source,
                    vec![a, b, c],
                    buffers.to_vec(),
                    Plan::normal(chunk),
                );
                let result = result.unwrap();
                let actual = primitive_payload(&result).unwrap();
                if errors[owners - 1] {
                    assert_eq!(result_buffer(&result, &buffers).0, owners - 1);
                } else {
                    assert_eq!(actual.scalar, Some(expected_scalar(kind)));
                }
                assert_eq!(
                    (proof.peak_boxes, proof.peak_box_bytes, proof.finally),
                    (owners, owners * std::mem::size_of::<Value>(), owners - 1)
                );
                assert_eq!(
                    proof.peak_strings,
                    buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
                );
                if chunk == 1 {
                    assert_eq!(proof.primitive_owners.len(), owners);
                    assert_eq!(
                        proof.last_primitive_candidate,
                        Some(actual),
                        "the last real finally candidate wins with the same Box and full snapshot"
                    );
                    assert_eq!(
                        (proof.peak_errors, proof.peak_error_bytes),
                        (
                            proof.error_owners.iter().map(|e| e.count).sum::<usize>(),
                            proof.error_owners.iter().map(|e| e.bytes).sum::<usize>()
                        )
                    );
                }
            }
        }
    }
}

#[test]
fn claimed_async_primitive_later_argument_fuel_native_tls_and_same_worker_reuse() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        let source = format!("async fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        let failure = format!("fn Leaf(text: str, ignored: int): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return Leaf(text, 1 / 0) }}");
        let (text, buffer) = string_input(TEXT, 4096);
        let (proof, _, result) =
            err_source_receipt(&failure, vec![text], vec![buffer], Plan::normal(1));
        assert_eq!(result.unwrap_err().message, "division by zero");
        assert_eq!(
            (proof.calls, proof.peak_boxes, proof.peak_errors),
            (1, 0, 0)
        );
        for fault in [Fault::FuelBefore, Fault::FuelAfter] {
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::normal(1);
            plan.fault = fault;
            let (proof, _, result) = err_source_receipt(&source, vec![text], vec![buffer], plan);
            assert_eq!(
                result.unwrap_err().message,
                "internal execution prototype test fuel exhausted"
            );
            assert_eq!(proof.peak_boxes, usize::from(fault == Fault::FuelAfter));
        }
        for error in [false, true] {
            let failing = if error {
                format!("async fn Entry(text: str): {ty}! {{ return err(text) }}")
            } else {
                source.clone()
            };
            let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::normal(1);
            plan.fault = Fault::NativeBox;
            let (task, rx, _) = spawn_source(&runtime, &failing, vec![text], vec![buffer], plan);
            let constructor = rx.recv_timeout(WAIT).unwrap();
            let native = rx.recv_timeout(WAIT).unwrap();
            let failed = task.await_result();
            release_task(task, CancellationContext::new(TerminationReason::Cancelled));
            let (text, buffer) = string_input(TEXT, 4096);
            let (healthy, healthy_rx, _) =
                spawn_source(&runtime, &source, vec![text], vec![buffer], Plan::normal(1));
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
                Witness::Entered {
                    selected: true,
                    legacy: false,
                    ..
                }
            ));
            assert!(matches!(native, Witness::Native { boxed, .. } if boxed != 0));
            assert!(matches!(
                healthy_constructor,
                Witness::Entered {
                    selected: true,
                    legacy: false,
                    ..
                }
            ));
            let Witness::Finished(proof) = healthy_completion else {
                panic!("same worker finishes its healthy next claimant")
            };
            let Value::Result { ok: false, value } = failed.unwrap() else {
                panic!("native task panic remains a false Result")
            };
            let Value::Object(fields) = *value else {
                unreachable!()
            };
            assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
            assert_primitive_source(&healthy_result.unwrap(), &proof, expected_scalar(kind), 1);
        }
    }
}

#[test]
fn claimed_async_primitive_roles_unknown_actuals_and_queued_running_produced_retirement() {
    for (_, ty, literal) in ASYNC_PRIMITIVES {
        let supported = functions(&format!(
            "async fn Entry(text: str): {ty}! {{ return ok({literal}) }}"
        ));
        for stage in 0..3 {
            let mut limits = Limits::default();
            match stage {
                0 => limits.syntax_steps = 0,
                1 => limits.syntax.max_frames = 0,
                _ => limits.syntax.max_storage_bytes = 0,
            }
            assert!(matches!(
                code::admit_claimed_async_owned_scalar_result_entry(
                    &supported,
                    &supported["Entry"],
                    limits
                ),
                Err(AdmissionError::Rejected(_))
            ));
            assert!(matches!(
                CodeHandle::compile_claimed_async_owned_scalar_result_entry(
                    &supported,
                    &supported["Entry"],
                    limits
                ),
                Err(AdmissionError::Rejected(_))
            ));
        }
        let wrong_ty = if ty == "float" { "int" } else { "float" };
        let wrong_literal = if wrong_ty == "int" { "7" } else { "1.25" };
        let invalid = format!("fn Wrong(text: str): {wrong_ty}! {{ return ok({wrong_literal}) }}\nasync fn Entry(text: str): {ty}! {{ return ok({literal})\nreturn Wrong(text) }}\nfn main() {{}}");
        let program = Parser::new(Lexer::new(&invalid).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let error = crate::checker::Checker::new()
            .check(&program)
            .expect_err("even dead typed cross-kind returns are checker-invalid");
        assert!(
            error
                .message
                .contains(&format!("expected {ty}! but got {wrong_ty}!")),
            "{error:?}"
        );
        let fresh_literal = format!("fn Keep(text: str): str {{ try {{ return text }} finally {{ marker = 1 }}\nreturn \"fallback\" }}\nasync fn Entry(text: str): {ty}! {{ return err(Keep(text)) }}");
        for source in [
            format!("async fn Entry(text: str): {ty}! {{ result = ok({literal})\nreturn result }}"),
            format!("fn Leaf(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return ok(Leaf(text)?) }}"),
            format!("async fn Entry(text: str): {ty}! {{ try {{ marker = 1 }} catch (e) {{ marker = 2 }}\nreturn ok({literal}) }}"),
            // A legal dead expression statement is outside this strict slice;
            // it is not a substitute for the same-arena cache contract.
            format!("fn Wrong(text: str): {wrong_ty}! {{ return ok({wrong_literal}) }}\nasync fn Entry(text: str): {ty}! {{ return ok({literal})\nWrong(text) }}"),
            // Whole-tree strict admission also rejects an unreachable fresh
            // String literal; the public legacy body remains legal below.
            fresh_literal.clone(),
        ] {
            let functions = functions(&source);
            assert!(!code::eligible_claimed_async_owned_scalar_result_entry(&functions, &functions["Entry"], Limits::default()));
            assert!(matches!(CodeHandle::compile_claimed_async_owned_scalar_result_entry(&functions, &functions["Entry"], Limits::default()), Err(AdmissionError::Unsupported(_))));
            let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::normal(1); plan.selection_only = true;
            let (task, rx, _) = spawn_source(&runtime, &source, vec![text], vec![buffer], plan);
            let constructor = rx.recv_timeout(WAIT).unwrap();
            let result = task.await_result();
            release_task(task, CancellationContext::new(TerminationReason::Cancelled)); close(&runtime);
            assert!(matches!(constructor, Witness::Entered { selected: false, legacy: true, steps: 0, .. }));
            assert_eq!(result.unwrap(), Value::Null);
        }
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut host = production_interpreter(&fresh_literal);
        host.task_runtime = Some(runtime.clone());
        let (text, _) = string_input(TEXT, 4096);
        let Value::Task(task) = host
            .call_function("Entry", vec![text], Span::default(), 0)
            .unwrap()
        else {
            panic!("the existing public async fallback creates its real Task")
        };
        let result = task.await_result();
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
        let Value::Result { ok: false, value } = result.unwrap() else {
            panic!("the existing legal body still returns its source err")
        };
        let Value::Object(fields) = *value else {
            unreachable!()
        };
        assert!(matches!(fields.get("domain"), Some(Value::String(domain)) if domain == "ku"));
        assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "err"));
        assert!(matches!(fields.get("message"), Some(Value::String(message)) if message == TEXT));
        let source = format!("async fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        for stage in [Pause::Running, Pause::Produced] {
            let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::normal(1);
            plan.pause = stage;
            let (task, rx, release) =
                spawn_source(&runtime, &source, vec![text], vec![buffer], plan);
            let constructor = rx.recv_timeout(WAIT).unwrap();
            let before = if stage == Pause::Produced {
                Some(rx.recv_timeout(WAIT).unwrap())
            } else {
                None
            };
            let paused = rx.recv_timeout(WAIT).unwrap();
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
            let completion = if stage == Pause::Running {
                Some(rx.recv_timeout(WAIT).unwrap())
            } else {
                None
            };
            let result = task.await_result();
            release_task(task, expected);
            close(&runtime);
            assert!(matches!(
                constructor,
                Witness::Entered {
                    selected: true,
                    legacy: false,
                    ..
                }
            ));
            assert!(matches!(paused, Witness::Paused { stage: actual, .. } if actual == stage));
            assert!(accepted && !second);
            assert_eq!(result.unwrap_err().runtime_termination(), Some(expected));
            let Witness::Finished(proof) = before.or(completion).unwrap() else {
                panic!("actual source retirement proof")
            };
            assert_eq!(proof.peak_boxes, usize::from(stage == Pause::Produced));
            if stage == Pause::Produced {
                assert!(proof.outgoing_primitive.is_some());
            }
        }
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (held_tx, held_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let held = runtime
            .spawn(move || {
                ready_tx.send(()).unwrap();
                held_rx.recv_timeout(WAIT).unwrap();
                Ok(Value::Null)
            })
            .unwrap();
        ready_rx.recv_timeout(WAIT).unwrap();
        let (text, buffer) = string_input(TEXT, 4096);
        let (task, rx, _) =
            spawn_source(&runtime, &source, vec![text], vec![buffer], Plan::normal(1));
        let queued = runtime.snapshot().unwrap().queued_tasks;
        let context = CancellationContext::new(TerminationReason::TimedOut);
        let accepted = task.request_cancel_with(context);
        let result = task.await_result();
        release_task(task, context);
        let abandoned = rx.recv_timeout(WAIT);
        let after = runtime.snapshot().unwrap().queued_tasks;
        held_tx.send(()).unwrap();
        let held_result = held.await_result();
        release_task(held, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
        assert!(accepted);
        assert_eq!((queued, after), (1, 0));
        assert!(matches!(
            abandoned,
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert_eq!(result.unwrap_err().runtime_termination(), Some(context));
        assert_eq!(held_result.unwrap(), Value::Null);
    }
    // Declared Float is metadata, not coercion: a legal Unknown leaf can be
    // actual Null, while opaque Unknown inputs must stay wholly legacy.
    let source = "async fn Entry(text: str, value: float): float! { return ok(value) }";
    let (text, buffer) = string_input(TEXT, 4096);
    let unknown = crate::stdlib::array::eval("first", &[Value::Array(Vec::new())], Span::default())
        .unwrap()
        .expect("registered array.first intrinsic");
    let (proof, _, result) =
        err_source_receipt(source, vec![text, unknown], vec![buffer], Plan::normal(1));
    assert_primitive_source(&result.unwrap(), &proof, ScalarLeaf::Null, 1);
    let complete_null = format!("{source}\nfn RequireNull(value): null! {{ if (value == null) {{ return ok(null) }}\nreturn err(\"Unknown primitive changed\") }}\nasync fn main(): null! {{ items = []\nunknown = array.first(items)\nactual = (await Entry(\"text\", unknown))?\nRequireNull(actual)?\nreturn ok(null) }}");
    let program = Parser::new(Lexer::new(&complete_null).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    crate::cli::run_source("async-primitive-unknown-null.ku", &complete_null).unwrap();
    for syntax in ["[1, 2]", "{answer: 7}"] {
        let complete = format!("{source}\nasync fn main(): null! {{ items = []\nitems = array.push(items, {syntax})\nunknown = array.first(items)\nresult = await Entry(\"text\", unknown)\nreturn ok(null) }}");
        let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
            .parse_program()
            .unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
        let opaque = if syntax.starts_with('[') {
            Value::Array(vec![Value::Int(1), Value::Int(2)])
        } else {
            Value::Object(HashMap::from([("answer".to_string(), Value::Int(7))]))
        };
        let unknown =
            crate::stdlib::array::eval("first", &[Value::Array(vec![opaque])], Span::default())
                .unwrap()
                .expect("registered array.first intrinsic");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut host = production_interpreter(source);
        host.task_runtime = Some(runtime.clone());
        host.async_execution = true;
        let (text, buffer) = string_input(TEXT, 4096);
        let shape = opaque_identity(&unknown);
        let inner = ClaimedNamedTask::new(
            host,
            functions(source)["Entry"].clone(),
            vec![text, unknown],
            Span::default(),
        );
        let selected = inner.machine.is_some();
        let (_, args) = inner
            .legacy
            .as_ref()
            .expect("opaque Unknown retains complete legacy arguments");
        let actual_shape = opaque_identity(&args[1]);
        let string_untouched = matches!(&args[0], Value::String(text) if text.as_ptr() as usize == buffer.pointer && text.capacity() == buffer.capacity);
        let effects = (inner.interpreter.steps, inner.interpreter.call_depth);
        drop(inner);
        close(&runtime);
        assert!(!selected);
        assert_eq!(actual_shape, shape);
        assert!(string_untouched);
        assert_eq!(effects, (0, 0));
        crate::cli::run_source("async-primitive-unknown-opaque.ku", &complete).unwrap();
    }
}

fn opaque_identity(value: &Value) -> (usize, usize) {
    match value {
        Value::Array(values) => (values.as_ptr() as usize, values.capacity()),
        Value::Object(fields) => (
            fields.get_key_value("answer").unwrap().0 as *const String as usize,
            fields.capacity(),
        ),
        _ => panic!("the real opaque Unknown owner"),
    }
}

fn spawn_primitive_child(
    runtime: &TaskRuntime,
    source: &str,
    code: Option<(CodeHandle, FnId)>,
    args: Vec<Value>,
    plan: Plan,
) -> (TaskHandle, mpsc::Receiver<Witness>) {
    let mut producer = production_interpreter(source);
    producer.task_runtime = Some(runtime.clone());
    let function = producer.functions["Child"].clone();
    let child_runtime = runtime.clone();
    let (tx, rx) = mpsc::channel();
    let (_release, release_rx) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            let template = producer.template();
            move || {
                assert!(crate::runtime::task::current_cleanup_context().is_none());
                assert!(crate::runtime::task::current_execution_termination().is_none());
                let mut host = Interpreter::from_template(template, child_runtime);
                host.async_execution = true;
                let inner = if let Some((code, function)) = code {
                    ClaimedNamedTask::from_code(host, code, function, args, Span::default())
                } else {
                    ClaimedNamedTask::new(host, function, args, Span::default())
                };
                tx.send(Witness::Entered {
                    task: crate::runtime::task::current_task_id(),
                    selected: inner.machine.is_some(),
                    legacy: inner.legacy.is_some(),
                    pending_error: false,
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
                    buffers: Vec::new(),
                    boxes: Vec::new(),
                    errors: Vec::new(),
                    primitives: Vec::new(),
                    last_primitive_candidate: None,
                    locals: plan.local_mode.then(LocalSourceProof::default),
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
    (task, rx)
}

#[test]
fn pure_scalar_fresh_child_uses_charged_root_and_real_box_budget() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        for zero_args in [false, true] {
            let params = if zero_args { "" } else { "n: int" };
            let operand = if zero_args { "" } else { "0" };
            let source = format!("async fn Child({params}): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ return await Child({operand}) }}");
            let functions = functions(&source);
            let code = CodeHandle::compile_claimed_async_owned_scalar_result_entry(
                &functions,
                &functions["Entry"],
                Limits::default(),
            )
            .unwrap();
            let child = code.find_function("Child").unwrap();
            assert_eq!(code.function(child).owned_scalar_result, Some(kind));
            for from_code in [true, false] {
                for fault in [Fault::None, Fault::WrapperCount, Fault::WrapperBytes] {
                    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
                    let args = if zero_args {
                        vec![]
                    } else {
                        vec![Value::Int(0)]
                    };
                    let mut plan = Plan::normal(1);
                    plan.fault = fault;
                    let (task, rx) = spawn_primitive_child(
                        &runtime,
                        &source,
                        from_code.then(|| (code.clone(), child)),
                        args,
                        plan,
                    );
                    let constructor = rx.recv_timeout(WAIT).unwrap();
                    let mut events = Vec::new();
                    let proof = loop {
                        match rx.recv_timeout(WAIT).unwrap() {
                            Witness::Finished(proof) => break *proof,
                            event => events.push(event),
                        }
                    };
                    let result = task.await_result();
                    release_task(task, CancellationContext::new(TerminationReason::Cancelled));
                    close(&runtime);
                    assert!(matches!(
                        constructor,
                        Witness::Entered {
                            selected: true,
                            legacy: false,
                            steps: 0,
                            calls: 1,
                            ..
                        }
                    ));
                    assert_eq!(proof.peak_strings, 0);
                    if fault == Fault::None {
                        assert_primitive_source(&result.unwrap(), &proof, expected_scalar(kind), 1);
                        assert!(events.is_empty());
                    } else {
                        assert_eq!(proof.peak_boxes, 0);
                        assert!(
                            matches!(events.as_slice(), [Witness::Wrap { input, .. }] if input.capacity == 0)
                        );
                        assert!(result.unwrap_err().message.contains(
                            if fault == Fault::WrapperCount {
                                "FrameLimit"
                            } else {
                                "StorageLimit"
                            }
                        ));
                    }
                }
            }
        }
    }
}

#[test]
fn claimed_async_scalar_locals_clone_actual_resident_boxes_and_false_buffers_across_chunks() {
    for (kind, ty, literal) in ASYNC_PRIMITIVES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            for helper in [false, true] {
                let dependency = if helper {
                    format!("fn Child(text: str): {ty}! {{ r: {ty}! = {leaf}\nr\nr\nr = r\nreturn r }}\n")
                } else {
                    String::new()
                };
                let rhs = if helper {
                    "Child(text)".to_string()
                } else {
                    leaf.clone()
                };
                let source = format!("{dependency}async fn Entry(text: str): {ty}! {{ r: {ty}! = {rhs}\nr\nr\nr = r\nreturn r }}");
                let declarations = functions(&source);
                assert!(code::eligible_claimed_async_owned_scalar_result_entry(
                    &declarations,
                    &declarations["Entry"],
                    Limits::default()
                ));
                CodeHandle::compile_claimed_async_owned_scalar_result_entry(
                    &declarations,
                    &declarations["Entry"],
                    Limits::default(),
                )
                .unwrap();
                for chunk in [1, 7, 256] {
                    let (text, buffer) = string_input(TEXT, 4096);
                    let (proof, events, result) =
                        err_source_receipt(&source, vec![text], vec![buffer], Plan::local(chunk));
                    assert!(events.is_empty());
                    let result = result.unwrap();
                    let actual = primitive_payload(&result).unwrap();
                    if error {
                        let Value::Result { value, .. } = &result else {
                            unreachable!()
                        };
                        let Value::Object(fields) = value.as_ref() else {
                            unreachable!()
                        };
                        assert!(
                            matches!(fields.get("domain"), Some(Value::String(domain)) if domain == "ku")
                        );
                        assert!(
                            matches!(fields.get("code"), Some(Value::String(code)) if code == "err")
                        );
                        assert!(
                            matches!(fields.get("message"), Some(Value::String(message)) if message == TEXT)
                        );
                        assert_ne!(
                            actual.error.unwrap().buffers[5].0,
                            buffer.pointer,
                            "source err clones the original message before local clones"
                        );
                    } else {
                        assert_eq!(actual.scalar, Some(expected_scalar(kind)));
                    }
                    assert_eq!(proof.outgoing_primitive, Some(actual));
                    assert_eq!(proof.calls, if helper { 2 } else { 1 });
                    assert_eq!(proof.peak_strings, buffer.capacity);
                    assert_eq!(proof.peak_boxes, 2);
                    assert_eq!(proof.peak_box_bytes, 2 * std::mem::size_of::<Value>());
                    let local = proof.locals.as_ref().unwrap();
                    if chunk == 1 {
                        assert!(local.peak_env >= 1 && local.peak_live >= 1);
                        assert_eq!(local.last_candidate, Some(actual));
                        assert_eq!(local.distinct_clones, if helper { 8 } else { 4 });
                        assert_eq!(local.peak_live, 2);
                        assert!(local.budget_checks > 0);
                    }
                }
            }
        }
    }
}

fn unlimited_source_local_policy() -> AwaitPayloadPolicy {
    AwaitPayloadPolicy {
        max_count: usize::MAX,
        max_bytes: usize::MAX,
        input_string_capacity: 0,
        span: Span::default(),
    }
}

#[test]
fn claimed_async_scalar_local_clone_exact_budgets_one_short_and_actual_map_peak_preserve_source() {
    for (_, ty, literal) in ASYNC_PRIMITIVES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let source =
                format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = {leaf}\nreturn r }}");
            for fault in [Fault::None, Fault::LocalCloneCount, Fault::LocalCloneBytes] {
                let (text, buffer) = string_input(TEXT, 4096);
                let mut plan = Plan::local(1);
                plan.fault = fault;
                let (proof, events, result) =
                    err_source_receipt(&source, vec![text], vec![buffer], plan);
                assert!(events.is_empty());
                let local = proof.locals.as_ref().unwrap();
                assert!(local.budget_checks > 0);
                assert_eq!(proof.calls, 1);
                assert_eq!(proof.peak_strings, buffer.capacity);
                if fault != Fault::None {
                    assert!(result.unwrap_err().message.contains(
                        if fault == Fault::LocalCloneCount {
                            "FrameLimit"
                        } else {
                            "StorageLimit"
                        }
                    ));
                    assert_eq!(
                        proof.peak_boxes, 1,
                        "failed clone admission never allocates its Result Box"
                    );
                    assert!(proof.outgoing_primitive.is_none());
                    assert!(local.clone_source.is_some() && local.clone_source_unchanged);
                    continue;
                }
                let source_value = result.unwrap();
                let original = primitive_payload(&source_value).unwrap();
                let mut observed = AwaitPayloadCharge::default();
                let (candidate, fee) = clone_selected_scalar_result(
                    &source_value,
                    unlimited_source_local_policy(),
                    &mut observed,
                )
                .unwrap();
                let actual = primitive_payload(&candidate).unwrap();
                distinct_local_source_clone(original, actual);
                assert_eq!(fee, AwaitPayloadCharge::for_value(&candidate, 0).unwrap());
                assert_eq!(observed, fee);
                drop(candidate);
                for reason in [None, Some("FrameLimit"), Some("StorageLimit")] {
                    let policy = AwaitPayloadPolicy {
                        max_count: fee.count - usize::from(reason == Some("FrameLimit")),
                        max_bytes: fee.bytes - usize::from(reason == Some("StorageLimit")),
                        ..unlimited_source_local_policy()
                    };
                    let mut observed = AwaitPayloadCharge::default();
                    let cloned = clone_selected_scalar_result(&source_value, policy, &mut observed);
                    if let Some(reason) = reason {
                        assert!(cloned
                            .expect_err("one-short real clone quota rejects")
                            .message
                            .contains(reason));
                        assert_eq!(observed.box_count, 0);
                    } else {
                        let (candidate, charge) = cloned.unwrap();
                        distinct_local_source_clone(
                            original,
                            primitive_payload(&candidate).unwrap(),
                        );
                        assert_eq!(charge, fee);
                        assert_eq!(observed, fee);
                        drop(candidate);
                    }
                    assert_eq!(primitive_payload(&source_value), Some(original),
                        "successful, rejected and dropped actual candidates preserve the resident source");
                }
            }
        }
    }
    // A private genuine sparse Map exercises actual capacity after its clone.
    // This does not claim source err naturally allocates this sparse shape.
    let mut fields = HashMap::with_capacity(128);
    fields.insert("domain".to_string(), Value::String("ku".to_string()));
    fields.insert("code".to_string(), Value::String("err".to_string()));
    fields.insert("message".to_string(), Value::String(TEXT.to_string()));
    let source = Value::Result {
        ok: false,
        value: Box::new(Value::Object(fields)),
    };
    let original = primitive_payload(&source).unwrap();
    let minimum = planned_local_clone_charge(&source).unwrap();
    let mut observed = AwaitPayloadCharge::default();
    let (candidate, actual) =
        clone_selected_scalar_result(&source, unlimited_source_local_policy(), &mut observed)
            .unwrap();
    distinct_local_source_clone(original, primitive_payload(&candidate).unwrap());
    assert!(actual.count > minimum.count && actual.bytes > minimum.bytes);
    drop(candidate);
    for (count, bytes, reason) in [
        (minimum.count, usize::MAX, "FrameLimit"),
        (usize::MAX, minimum.bytes, "StorageLimit"),
    ] {
        let mut observed = AwaitPayloadCharge::default();
        let result = clone_selected_scalar_result(
            &source,
            AwaitPayloadPolicy {
                max_count: count,
                max_bytes: bytes,
                ..unlimited_source_local_policy()
            },
            &mut observed,
        );
        assert!(result
            .expect_err("the actual cloned Map exceeds its planned minimum")
            .message
            .contains(reason));
        assert_eq!((observed.box_count, observed.box_bytes), (0, 0));
        assert_eq!(
            (observed.error_leaf_count, observed.error_leaf_bytes),
            (actual.error_leaf_count, actual.error_leaf_bytes),
            "a rejected real Map records its peak without inventing a Result Box"
        );
        assert_eq!(primitive_payload(&source), Some(original));
    }
}

#[test]
fn claimed_async_scalar_local_unknown_null_and_whole_tree_fallback_keep_original_ability() {
    let source = "async fn Entry(text: str, value: float): float! { r: float! = ok(value)\nr\nr\nr = r\nreturn r }";
    let unknown = crate::stdlib::array::eval("first", &[Value::Array(Vec::new())], Span::default())
        .unwrap()
        .expect("registered array.first yields its real Unknown Null");
    let (text, buffer) = string_input(TEXT, 4096);
    let (proof, _, result) =
        err_source_receipt(source, vec![text, unknown], vec![buffer], Plan::local(1));
    let actual = primitive_payload(&result.unwrap()).unwrap();
    assert_eq!(
        actual.scalar,
        Some(ScalarLeaf::Null),
        "declared Float never coerces an actual Unknown Null"
    );
    assert_eq!(proof.locals.as_ref().unwrap().distinct_clones, 4);
    assert_eq!(proof.locals.as_ref().unwrap().last_candidate, Some(actual));
    let complete = format!("{source}\nfn RequireNull(value): null! {{ if (value == null) {{ return ok(null) }}\nreturn err(\"Unknown local changed\") }}\nasync fn main(): null! {{ items = []\nunknown = array.first(items)\nactual = (await Entry(\"text\", unknown))?\nRequireNull(actual)?\nreturn ok(null) }}");
    let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    crate::cli::run_source("claimed-async-local-unknown-null.ku", &complete).unwrap();
    for (_, ty, literal) in ASYNC_PRIMITIVES {
        for negative in [
            format!("fn Pass(value: {ty}!): {ty}! {{ return value }}\nasync fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nreturn Pass(r) }}"),
            format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nreturn ok(r?) }}"),
            format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\ntry {{ marker = 1 }} catch (e) {{ marker = 2 }}\nreturn r }}"),
            format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nfn Local(): {ty}! {{ return r.clone() }}\nreturn Local() }}"),
            format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nasync fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nchild = Child(text)\nreturn await child }}"),
            format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nother: str! = ok(text)\nreturn r }}"),
        ] {
            let declarations = functions(&negative);
            assert!(!code::eligible_claimed_async_owned_scalar_result_entry(&declarations,
                &declarations["Entry"], Limits::default()), "{negative}");
            assert!(matches!(CodeHandle::compile_claimed_async_owned_scalar_result_entry(&declarations,
                &declarations["Entry"], Limits::default()), Err(AdmissionError::Unsupported(_))), "{negative}");
            let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
            let (text, buffer) = string_input(TEXT, 4096);
            let mut plan = Plan::local(1);
            plan.selection_only = true;
            let (task, rx, _) = spawn_source(&runtime, &negative, vec![text], vec![buffer], plan);
            entered(&task, &rx, false, false);
            assert_eq!(task.await_result().unwrap(), Value::Null);
            release_task(task, CancellationContext::new(TerminationReason::Cancelled));
            close(&runtime);
        }
        // An ordinary legal fallback executes the complete body, rather than
        // returning the selection-only fixture's intentional Null sentinel.
        let legacy_source = format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\ntry {{ marker = 1 }} catch (e) {{ marker = 2 }}\nr\nr = r\nreturn r }}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut host = production_interpreter(&legacy_source);
        host.task_runtime = Some(runtime.clone());
        let (text, _) = string_input(TEXT, 4096);
        let Value::Task(task) = host
            .call_function("Entry", vec![text], Span::default(), 0)
            .unwrap()
        else {
            panic!("ordinary async fallback creates its real Task")
        };
        let result = task.await_result().unwrap();
        assert_eq!(
            primitive_payload(&result).unwrap().scalar,
            Some(expected_scalar(
                ASYNC_PRIMITIVES
                    .iter()
                    .find(|(_, name, _)| *name == ty)
                    .unwrap()
                    .0
            ))
        );
        release_task(task, CancellationContext::new(TerminationReason::Cancelled));
        close(&runtime);
    }
    for syntax in ["[1, 2]", "{answer: 7}"] {
        let opaque = if syntax.starts_with('[') {
            Value::Array(vec![Value::Int(1), Value::Int(2)])
        } else {
            Value::Object(HashMap::from([("answer".to_string(), Value::Int(7))]))
        };
        let unknown =
            crate::stdlib::array::eval("first", &[Value::Array(vec![opaque])], Span::default())
                .unwrap()
                .expect("registered array.first preserves its opaque Unknown owner");
        let original = opaque_identity(&unknown);
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut host = production_interpreter(source);
        host.task_runtime = Some(runtime.clone());
        host.async_execution = true;
        let (text, buffer) = string_input(TEXT, 4096);
        let inner = ClaimedNamedTask::new(
            host,
            functions(source)["Entry"].clone(),
            vec![text, unknown],
            Span::default(),
        );
        assert!(inner.machine.is_none());
        let (_, args) = inner.legacy.as_ref().unwrap();
        assert_eq!(opaque_identity(&args[1]), original);
        assert!(
            matches!(&args[0], Value::String(text) if text.as_ptr() as usize == buffer.pointer && text.capacity() == buffer.capacity)
        );
        assert_eq!(
            (inner.interpreter.steps, inner.interpreter.call_depth),
            (0, 0)
        );
        drop(inner);
        close(&runtime);
        let complete = format!("{source}\nasync fn main(): null! {{ items = []\nitems = array.push(items, {syntax})\nunknown = array.first(items)\nactual = await Entry(\"text\", unknown)\nreturn ok(null) }}");
        let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
            .parse_program()
            .unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
        crate::cli::run_source("claimed-async-local-unknown-opaque.ku", &complete).unwrap();
    }
}
