use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct Budget {
    connections: usize,
    waiters: usize,
    pending: usize,
}

static USED: Mutex<Budget> = Mutex::new(Budget {
    connections: 0,
    waiters: 0,
    pending: 0,
});
static LIMITS: OnceLock<Option<Budget>> = OnceLock::new();

#[cfg(test)]
pub(super) fn test_usage() -> (usize, usize, usize) {
    let used = USED.lock().unwrap_or_else(|error| error.into_inner());
    (used.connections, used.waiters, used.pending)
}

fn limit(name: &str, fallback: usize, max: usize) -> Option<usize> {
    let Some(value) = std::env::var_os(name) else {
        return Some(fallback);
    };
    let value = value.to_str()?;
    if value.is_empty() {
        return Some(fallback);
    }
    if value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = value.parse::<usize>().ok()?;
    (value != 0 && value <= max).then_some(value)
}

pub(super) struct Reservation(Budget);

impl Reservation {
    pub(super) fn client(connections: usize, waiters: usize) -> Option<Self> {
        Self::reserve(Budget {
            connections,
            waiters,
            pending: 0,
        })
    }

    pub(super) fn operation(input_bytes: usize) -> Option<Self> {
        Self::reserve(Budget {
            pending: input_bytes.checked_add(64)?,
            ..Budget::default()
        })
    }

    fn reserve(charge: Budget) -> Option<Self> {
        let limits = LIMITS
            .get_or_init(|| {
                Some(Budget {
                    connections: limit("KU_DB_MAX_CONNECTIONS", 256, 65_536)?,
                    waiters: limit("KU_DB_MAX_WAITERS", 4096, 1_048_576)?,
                    pending: limit("KU_DB_MAX_PENDING_BYTES", 64 * 1024 * 1024, usize::MAX / 2)?,
                })
            })
            .as_ref()?;
        let mut used = USED.lock().unwrap_or_else(|error| error.into_inner());
        if charge.connections > limits.connections - used.connections
            || charge.waiters > limits.waiters - used.waiters
            || charge.pending > limits.pending - used.pending
        {
            return None;
        }
        used.connections += charge.connections;
        used.waiters += charge.waiters;
        used.pending += charge.pending;
        Some(Self(charge))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = USED.lock().unwrap_or_else(|error| error.into_inner());
        used.connections -= self.0.connections;
        used.waiters -= self.0.waiters;
        used.pending -= self.0.pending;
    }
}
