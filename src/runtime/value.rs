use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Weak},
};

use crate::ast::{ParamMode, Stmt};
use crate::env::Env;
use crate::error::{KuError, KuResult};
use crate::runtime::task::TaskHandle;
use crate::span::Span;

/// Opaque ownership token stored only in an interpreter HTTP-listener object.
/// Cloning a `Value` clones this `Arc`; the final owner releases an unconsumed
/// socket from the process registry.
#[doc(hidden)]
#[derive(Debug)]
pub struct HttpListenerLease {
    id: i64,
}

impl HttpListenerLease {
    pub(crate) fn new(id: i64) -> Arc<Self> {
        Arc::new(Self { id })
    }
}

impl Drop for HttpListenerLease {
    fn drop(&mut self) {
        crate::runtime::http_listener_registry::release_best_effort(self.id);
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
    Struct {
        name: String,
        fields: HashMap<String, Value>,
    },
    Enum {
        name: String,
        variant: String,
        fields: Vec<Value>,
    },
    Result {
        ok: bool,
        value: Box<Value>,
    },
    Function {
        params: Vec<String>,
        param_modes: Vec<ParamMode>,
        body: Vec<Stmt>,
        captures: Env,
        self_name: Option<String>,
        is_async: bool,
    },
    Task(TaskHandle),
    #[doc(hidden)]
    HttpListenerLease(Arc<HttpListenerLease>),
    #[doc(hidden)]
    Borrowed(BorrowedValue),
    Null,
}

/// A call-scoped read view. Only the caller's guard owns the root; a view cannot
/// keep its source alive. Projection paths never copy the projected container.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct BorrowedValue {
    root: Weak<Value>,
    path: Vec<BorrowProjection>,
    owner_thread: std::thread::ThreadId,
    owner_task_id: i64,
}

#[cfg(test)]
thread_local! {
    static BORROW_ROOTS_CREATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn borrow_roots_created() -> usize {
    BORROW_ROOTS_CREATED.with(std::cell::Cell::get)
}

#[derive(Debug, Clone)]
pub(crate) enum ValueProjection {
    Field(String),
    Index(usize),
    EnumField(usize),
}

pub(crate) type BorrowProjection = ValueProjection;

impl BorrowedValue {
    pub(crate) fn new(root: &Arc<Value>) -> Self {
        #[cfg(test)]
        BORROW_ROOTS_CREATED.with(|count| count.set(count.get() + 1));
        Self {
            root: Arc::downgrade(root),
            path: Vec::new(),
            owner_thread: std::thread::current().id(),
            owner_task_id: crate::runtime::task::current_task_id(),
        }
    }

    pub(crate) fn is_current_owner(&self) -> bool {
        self.owner_thread == std::thread::current().id()
            && self.owner_task_id == crate::runtime::task::current_task_id()
    }

    pub(crate) fn check_owner(&self, span: Span) -> KuResult<()> {
        if self.is_current_owner() {
            return Ok(());
        }
        Err(KuError::runtime(
            "borrowed value cannot be read across threads or async tasks",
            span,
        ))
    }

    pub(crate) fn with_read<T>(
        &self,
        span: Span,
        read: impl FnOnce(&Value) -> KuResult<T>,
    ) -> KuResult<T> {
        self.check_owner(span)?;
        self.read_root(span, read)
    }

    /// Only Env may use this for an ordinary read of the caller's owned source
    /// while another task has an active loan. Its source binding mutex MUST
    /// remain held throughout the callback, serializing with lease restoration.
    /// The callback receives the owned source, never a cross-task weak view.
    pub(crate) fn with_source_read_locked<T>(
        &self,
        _source_guard: &std::sync::MutexGuard<'_, crate::env::Binding>,
        span: Span,
        read: impl FnOnce(&Value) -> KuResult<T>,
    ) -> KuResult<T> {
        self.read_root(span, read)
    }

    fn read_root<T>(&self, span: Span, read: impl FnOnce(&Value) -> KuResult<T>) -> KuResult<T> {
        let root = self.root.upgrade().ok_or_else(|| {
            KuError::runtime("borrowed value outlived its synchronous call", span)
        })?;
        let mut value = root.as_ref();
        for projection in &self.path {
            value = match (projection, value) {
                (
                    BorrowProjection::Field(name),
                    Value::Object(fields) | Value::Struct { fields, .. },
                ) => fields.get(name),
                (BorrowProjection::Index(index), Value::Array(values)) => values.get(*index),
                (BorrowProjection::EnumField(index), Value::Enum { fields, .. }) => {
                    fields.get(*index)
                }
                _ => None,
            }
            .ok_or_else(|| KuError::runtime("invalid borrowed projection", span))?;
        }
        read(value)
    }

    pub(crate) fn project(&self, projection: BorrowProjection, span: Span) -> KuResult<Value> {
        let mut projected = self.clone();
        projected.path.push(projection);
        projected
            .with_read(span, |value| Ok(value.copy_value()))?
            .map_or_else(|| Ok(Value::Borrowed(projected)), Ok)
    }
}

impl Value {
    /// Inspect only values owned by this container. Closure capture environments
    /// and weak borrowed roots are not descendants of its ownership tree.
    /// Iterators keep scratch space proportional to depth, not array width.
    fn visit_owned_tasks(
        &self,
        span: Span,
        mut visit: impl FnMut(&TaskHandle) -> KuResult<bool>,
    ) -> KuResult<bool> {
        let mut ancestors: Vec<OwnedValueChildren<'_>> = Vec::new();
        let mut next = Some(self);
        loop {
            if let Some(value) = next.take() {
                if let Value::Task(task) = value {
                    if visit(task)? {
                        return Ok(true);
                    }
                } else if let Some(mut children) = OwnedValueChildren::of(value) {
                    if let Some(child) = children.next() {
                        ancestors.try_reserve(1).map_err(|_| {
                            KuError::runtime("task ownership traversal out of memory", span)
                        })?;
                        ancestors.push(children);
                        next = Some(child);
                        continue;
                    }
                }
            }
            while let Some(children) = ancestors.last_mut() {
                if let Some(child) = children.next() {
                    next = Some(child);
                    break;
                }
                ancestors.pop();
            }
            if next.is_none() {
                return Ok(false);
            }
        }
    }

    pub(crate) fn contains_owned_task(&self, span: Span) -> KuResult<bool> {
        self.visit_owned_tasks(span, |_| Ok(true))
    }

    pub(crate) fn collect_owned_tasks(
        &self,
        tasks: &mut Vec<TaskHandle>,
        span: Span,
    ) -> KuResult<()> {
        self.visit_owned_tasks(span, |task| {
            tasks
                .try_reserve(1)
                .map_err(|_| KuError::runtime("task ownership collection out of memory", span))?;
            tasks.push(task.clone());
            Ok(false)
        })?;
        Ok(())
    }

    pub(crate) fn contains_task_projection(
        &self,
        path: &[ValueProjection],
        span: Span,
    ) -> KuResult<bool> {
        let mut selected = self;
        for projection in path {
            selected = match (projection, selected) {
                (
                    ValueProjection::Field(name),
                    Value::Object(fields) | Value::Struct { fields, .. },
                ) => {
                    let Some(value) = fields.get(name) else {
                        return Ok(false);
                    };
                    value
                }
                (ValueProjection::Index(index), Value::Array(values)) => {
                    let Some(value) = values.get(*index) else {
                        return Ok(false);
                    };
                    value
                }
                (ValueProjection::EnumField(index), Value::Enum { fields, .. }) => {
                    let Some(value) = fields.get(*index) else {
                        return Ok(false);
                    };
                    value
                }
                _ => return Ok(false),
            };
        }
        selected.contains_owned_task(span)
    }

    /// The caller evaluates projection operands before entering this API. A
    /// missing/ordinary projection is untouched so existing lookup diagnostics
    /// and transparent non-Task snapshot reads remain the caller's responsibility.
    pub(crate) fn take_task_projection(
        &mut self,
        path: &[ValueProjection],
        span: Span,
    ) -> KuResult<Option<Value>> {
        let mut selected = self;
        for projection in path {
            selected = match (projection, selected) {
                (
                    ValueProjection::Field(name),
                    Value::Object(fields) | Value::Struct { fields, .. },
                ) => {
                    let Some(value) = fields.get_mut(name) else {
                        return Ok(None);
                    };
                    value
                }
                (ValueProjection::Index(index), Value::Array(values)) => {
                    let Some(value) = values.get_mut(*index) else {
                        return Ok(None);
                    };
                    value
                }
                (ValueProjection::EnumField(index), Value::Enum { fields, .. }) => {
                    let Some(value) = fields.get_mut(*index) else {
                        return Ok(None);
                    };
                    value
                }
                _ => return Ok(None),
            };
        }
        if selected.contains_owned_task(span)? {
            Ok(Some(std::mem::replace(selected, Value::Null)))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn copy_value(&self) -> Option<Value> {
        match self {
            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => Some(self.clone()),
            _ => None,
        }
    }

    pub(crate) fn with_read<T>(
        &self,
        span: Span,
        read: impl FnOnce(&Value) -> KuResult<T>,
    ) -> KuResult<T> {
        match self {
            Value::Borrowed(value) => value.with_read(span, read),
            value => read(value),
        }
    }

    pub(crate) fn contains_borrowed(&self) -> bool {
        match self {
            Value::Borrowed(_) => true,
            Value::Array(values) | Value::Enum { fields: values, .. } => {
                values.iter().any(Value::contains_borrowed)
            }
            Value::Object(fields) | Value::Struct { fields, .. } => {
                fields.values().any(Value::contains_borrowed)
            }
            Value::Result { value, .. } => value.contains_borrowed(),
            _ => false,
        }
    }

    pub(crate) fn require_owned(&self, span: Span) -> KuResult<()> {
        if self.contains_borrowed() {
            return Err(KuError::runtime("borrowed value cannot escape, be stored, or be passed to an owning parameter; use clone to create an owned value", span));
        }
        Ok(())
    }

    /// Interpreter container construction and insertion validate every child
    /// before storing it. Such containers cannot contain borrowed descendants;
    /// checking their root avoids rescanning an entire growing tree each time.
    /// Public Env admission still uses the full recursive require_owned check.
    pub(crate) fn require_owned_root(&self, span: Span) -> KuResult<()> {
        if matches!(self, Value::Borrowed(_)) {
            return Err(KuError::runtime("borrowed value cannot escape, be stored, or be passed to an owning parameter; use clone to create an owned value", span));
        }
        Ok(())
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Borrowed(value) => value
                .with_read(Span::default(), |value| Ok(value.is_truthy()))
                .unwrap_or(false),
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            Value::String(value) => !value.is_empty(),
            Value::Array(values) => !values.is_empty(),
            Value::Object(fields) => !fields.is_empty(),
            Value::Null => false,
            _ => true,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::String(_) => "str",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Struct { .. } => "struct",
            Value::Enum { .. } => "enum",
            Value::Result { .. } => "result",
            Value::Function { .. } => "function",
            Value::Task(_) => "task",
            Value::HttpListenerLease(_) => "http_listener_lease",
            Value::Borrowed(value) => value
                .with_read(Span::default(), |value| Ok(value.type_name()))
                .unwrap_or("expired borrowed value"),
            Value::Null => "null",
        }
    }
}

enum OwnedValueChildren<'a> {
    Sequence(std::slice::Iter<'a, Value>),
    Fields(std::collections::hash_map::Values<'a, String, Value>),
    Result(Option<&'a Value>),
}

impl<'a> OwnedValueChildren<'a> {
    fn of(value: &'a Value) -> Option<Self> {
        match value {
            Value::Array(values) | Value::Enum { fields: values, .. } => {
                Some(Self::Sequence(values.iter()))
            }
            Value::Object(fields) | Value::Struct { fields, .. } => {
                Some(Self::Fields(fields.values()))
            }
            Value::Result { value, .. } => Some(Self::Result(Some(value))),
            _ => None,
        }
    }

    fn next(&mut self) -> Option<&'a Value> {
        match self {
            Self::Sequence(values) => values.next(),
            Self::Fields(fields) => fields.next(),
            Self::Result(value) => value.take(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(value) => write!(f, "{}", value),
            Value::Float(value) => write!(f, "{}", value),
            Value::Bool(value) => write!(f, "{}", value),
            Value::String(value) => write!(f, "{}", value),
            Value::Array(values) => {
                write!(f, "[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{value}")?;
                }
                write!(f, "]")
            }
            Value::Object(fields) => {
                write!(f, "{{ ")?;
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (field, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}: {value}")?;
                }
                write!(f, " }}")
            }
            Value::Struct { name, fields } => {
                write!(f, "{name} {{ ")?;
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (field, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}: {value}")?;
                }
                write!(f, " }}")
            }
            Value::Enum {
                name,
                variant,
                fields,
            } => {
                write!(f, "{name}.{variant}")?;
                if !fields.is_empty() {
                    write!(f, "(")?;
                    for (index, field) in fields.iter().enumerate() {
                        if index > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{field}")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Value::Result { ok, value } => {
                if *ok {
                    write!(f, "Ok({value})")
                } else {
                    write!(f, "Err({value})")
                }
            }
            Value::Function { .. } => write!(f, "<function>"),
            Value::Task(task) => write!(f, "<task:{}>", task.id()),
            Value::HttpListenerLease(_) => write!(f, "<http-listener-lease>"),
            Value::Borrowed(value) => value
                .with_read(Span::default(), |value| Ok(write!(f, "{value}")))
                .map_err(|_| fmt::Error)?,
            Value::Null => write!(f, "null"),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Borrowed(value), other) => value
                .with_read(Span::default(), |value| Ok(value == other))
                .unwrap_or(false),
            (value, Value::Borrowed(other)) => other
                .with_read(Span::default(), |other| Ok(value == other))
                .unwrap_or(false),
            (Value::Int(left), Value::Int(right)) => left == right,
            (Value::Float(left), Value::Float(right)) => left == right,
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::String(left), Value::String(right)) => left == right,
            (Value::Array(left), Value::Array(right)) => left == right,
            (Value::Object(left), Value::Object(right)) => left == right,
            (
                Value::Struct {
                    name: left_name,
                    fields: left_fields,
                },
                Value::Struct {
                    name: right_name,
                    fields: right_fields,
                },
            ) => left_name == right_name && left_fields == right_fields,
            (
                Value::Enum {
                    name: left_name,
                    variant: left_variant,
                    fields: left_fields,
                },
                Value::Enum {
                    name: right_name,
                    variant: right_variant,
                    fields: right_fields,
                },
            ) => {
                left_name == right_name
                    && left_variant == right_variant
                    && left_fields == right_fields
            }
            (
                Value::Result {
                    ok: left_ok,
                    value: left_value,
                },
                Value::Result {
                    ok: right_ok,
                    value: right_value,
                },
            ) => left_ok == right_ok && left_value == right_value,
            (Value::Function { .. }, Value::Function { .. }) => false,
            (Value::Task(left), Value::Task(right)) => left == right,
            (Value::HttpListenerLease(left), Value::HttpListenerLease(right)) => {
                Arc::ptr_eq(left, right)
            }
            (Value::Null, Value::Null) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod task_ownership_tests {
    use super::*;
    use crate::runtime::task::TaskRuntime;

    #[test]
    fn task_walk_covers_owned_wrappers_and_leaves_enum_siblings() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let task = runtime.spawn(|| Ok(Value::Int(1))).unwrap();
        let id = task.id();
        let mut value = Value::Struct {
            name: "Holder".into(),
            fields: HashMap::from([(
                "payload".into(),
                Value::Result {
                    ok: true,
                    value: Box::new(Value::Enum {
                        name: "Choice".into(),
                        variant: "Pair".into(),
                        fields: vec![Value::Task(task), Value::String("sibling".into())],
                    }),
                },
            )]),
        };
        let mut tasks = Vec::new();
        value.collect_owned_tasks(&mut tasks, span).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id(), id);
        let wrapped = value
            .take_task_projection(&[ValueProjection::Field("payload".into())], span)
            .unwrap()
            .unwrap();
        // Result is consumed/unwrapped as a value, just as the interpreter's `?`
        // path does; field/index projections do not invent a Result member.
        let Value::Result {
            value: mut payload, ..
        } = wrapped
        else {
            unreachable!()
        };
        let taken = payload
            .take_task_projection(&[ValueProjection::EnumField(0)], span)
            .unwrap()
            .unwrap();
        assert!(matches!(taken, Value::Task(task) if task.id() == id));
        assert!(!value.contains_owned_task(span).unwrap());
        let Value::Struct { fields, .. } = &value else {
            unreachable!()
        };
        assert_eq!(fields["payload"], Value::Null);
        let Value::Enum { fields, .. } = payload.as_ref() else {
            unreachable!()
        };
        assert_eq!(fields, &[Value::Null, Value::String("sibling".into())]);
    }

    #[test]
    fn task_walk_is_iterative_and_does_not_follow_closure_captures() {
        let span = Span::default();
        let runtime = TaskRuntime::new();
        let mut captures = Env::new();
        captures
            .define(
                "task".into(),
                Value::Task(runtime.spawn(|| Ok(Value::Int(1))).unwrap()),
                false,
                span,
            )
            .unwrap();
        let function = Value::Function {
            params: Vec::new(),
            param_modes: Vec::new(),
            body: Vec::new(),
            captures,
            self_name: None,
            is_async: false,
        };
        assert!(!function.contains_owned_task(span).unwrap());
        let mut value = Value::Int(1);
        for _ in 0..10_000 {
            value = Value::Result {
                ok: true,
                value: Box::new(value),
            };
        }
        assert!(!value.contains_owned_task(span).unwrap());
        // This test exercises the iterative visitor, not the existing recursive
        // destructor for arbitrary deeply nested Value trees.
        while let Value::Result { value: child, .. } = value {
            value = *child;
        }
        let wide = Value::Array(vec![Value::Int(0); 65_536]);
        assert!(!wide.contains_owned_task(span).unwrap());
    }
}

#[cfg(test)]
mod borrow_tests {
    use super::*;

    #[test]
    fn borrowed_read_checks_task_identity_even_on_the_same_thread() {
        let root = Arc::new(Value::String("owned".into()));
        let mut view = BorrowedValue::new(&root);
        view.owner_task_id = view.owner_task_id.wrapping_add(1);
        assert!(!view.is_current_owner());
        let error = view
            .with_read::<()>(Span::default(), |_| {
                panic!("another task must not read the root")
            })
            .unwrap_err();
        assert!(error.message.contains("across threads or async tasks"));
        assert_eq!(Arc::strong_count(&root), 1);
    }
}
