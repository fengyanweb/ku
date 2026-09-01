use std::{
    collections::HashMap,
    net::TcpListener,
    sync::{
        atomic::{AtomicI64, Ordering},
        Mutex, OnceLock,
    },
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
};

static HTTP_LISTENERS: OnceLock<Mutex<HashMap<i64, TcpListener>>> = OnceLock::new();
static NEXT_HTTP_LISTENER_ID: AtomicI64 = AtomicI64::new(1);

pub(crate) fn insert(listener: TcpListener) -> Result<i64, String> {
    let id = NEXT_HTTP_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
    listeners()
        .lock()
        .map_err(|_| "http listener registry is poisoned".to_string())?
        .insert(id, listener);
    Ok(id)
}

pub(crate) fn take(id: i64, span: Span) -> KuResult<TcpListener> {
    listeners()
        .lock()
        .map_err(|_| KuError::runtime("http listener registry is poisoned", span))?
        .remove(&id)
        .ok_or_else(|| KuError::runtime("http listener was already consumed", span))
}

pub(crate) fn close(id: i64, span: Span) -> KuResult<()> {
    take(id, span).map(drop)
}

/// Release a listener whose last interpreter `Value` owner disappeared without
/// calling `run` or `close`. `Drop` cannot report errors, so recover a poisoned
/// registry and make removal idempotent: an explicit take/close already owns or
/// dropped the socket and leaves nothing for this finalizer to do.
pub(crate) fn release_best_effort(id: i64) {
    let Some(registry) = HTTP_LISTENERS.get() else {
        return;
    };
    let mut listeners = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    listeners.remove(&id);
}

fn listeners() -> &'static Mutex<HashMap<i64, TcpListener>> {
    HTTP_LISTENERS.get_or_init(|| Mutex::new(HashMap::new()))
}
