mod budget;
mod libpq;
mod sql;
mod typed;

use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    fmt,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

use budget::Reservation;
use libpq::{Connection, Error, Tabular};

type PgResult<T> = Result<T, Error>;

const MAX_SQL_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_PARAM_COUNT: usize = 65_535;
const INPUT_CHUNK: usize = 64 * 1024;
const WAIT_CHECK: Duration = Duration::from_millis(25);

const fn error(kind: &'static str, message: &'static str) -> Error {
    Error { kind, message }
}

const CLIENT_CLOSED: Error = error("client_closed", "PostgreSQL client is closed");
const ACQUIRE_TIMEOUT: Error = error(
    "acquire_timeout",
    "timed out waiting for a PostgreSQL client connection",
);
const QUERY_TIMEOUT: Error = error(
    "query_timeout",
    "PostgreSQL query budget expired before the requested statement was sent",
);
const CANCELLED: Error = error("cancelled", "PostgreSQL query was cancelled before sending");
const OUT_OF_MEMORY: Error = error("out_of_memory", "PostgreSQL allocation failed");

fn result_error(error: Error) -> Value {
    errors::err("pg", error.kind, error.message)
}

struct Config {
    conninfo: Conninfo,
    connections: usize,
    waiters: usize,
    connect_timeout: Duration,
    acquire_timeout: Duration,
    query_timeout: Duration,
}

impl Config {
    fn parse(fields: &HashMap<String, Value>, span: Span) -> PgResult<Self> {
        for key in fields.keys() {
            if !matches!(
                key.as_str(),
                "conninfo"
                    | "max_connections"
                    | "max_waiters"
                    | "connect_timeout_ms"
                    | "acquire_timeout_ms"
                    | "query_timeout_ms"
            ) {
                return Err(invalid_config(
                    "PostgreSQL client config contains an unknown field",
                ));
            }
        }
        let conninfo = fields.get("conninfo").ok_or_else(|| {
            invalid_config("PostgreSQL client config requires a string conninfo field")
        })?;
        let conninfo = conninfo
            .with_read(span, |value| {
                Ok(match value {
                    Value::String(text) if text.len() <= 65_536 => {
                        let nul = invalid_config("PostgreSQL conninfo contains a NUL byte");
                        check_for_nul(text, &|| Ok(()), nul)
                            .and_then(|()| copy_to_cstr(text, &|| Ok(())))
                            .map(|bytes| Conninfo(Some(bytes)))
                    }
                    _ => Err(invalid_config(
                        "PostgreSQL conninfo must be a string of at most 65536 bytes",
                    )),
                })
            })
            .map_err(|_| invalid_config("PostgreSQL conninfo is unavailable"))??;
        Ok(Self {
            connections: config_integer(fields, "max_connections", 8, 1, 256, span)? as usize,
            waiters: config_integer(fields, "max_waiters", 64, 0, 4096, span)? as usize,
            connect_timeout: config_timeout(fields, "connect_timeout_ms", 5000, span)?,
            acquire_timeout: config_timeout(fields, "acquire_timeout_ms", 5000, span)?,
            query_timeout: config_timeout(fields, "query_timeout_ms", 30_000, span)?,
            conninfo,
        })
    }
}

fn invalid_config(message: &'static str) -> Error {
    error("invalid_config", message)
}

fn config_integer(
    fields: &HashMap<String, Value>,
    name: &str,
    default: u64,
    min: i64,
    max: i64,
    span: Span,
) -> PgResult<u64> {
    let Some(value) = fields.get(name) else {
        return Ok(default);
    };
    value
        .with_read(span, |value| {
            Ok(match value {
                Value::Int(number) if (min..=max).contains(number) => Ok(*number as u64),
                _ => Err(invalid_config(
                    "PostgreSQL client integer config field is invalid or outside its allowed range",
                )),
            })
        })
        .map_err(|_| invalid_config("PostgreSQL client integer config field is unavailable"))?
}

fn config_timeout(
    fields: &HashMap<String, Value>,
    name: &str,
    default: u64,
    span: Span,
) -> PgResult<Duration> {
    config_integer(fields, name, default, 1, 300_000, span).map(Duration::from_millis)
}

struct Conninfo(Option<CString>);

impl Conninfo {
    fn as_c_str(&self) -> &CStr {
        self.0.as_deref().expect("live PG config has no conninfo")
    }
}

impl Drop for Conninfo {
    fn drop(&mut self) {
        if let Some(conninfo) = self.0.take() {
            for byte in conninfo.into_bytes_with_nul().iter_mut() {
                // SAFETY: each byte is exclusively owned and remains live until the Vec is dropped.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
        }
    }
}

pub struct Client {
    config: Config,
    state: Mutex<Pool>,
    changed: Condvar,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PgClient").finish_non_exhaustive()
    }
}

struct Pool {
    reservation: Option<Reservation>,
    idle: Vec<Connection>,
    active: usize,
    waiting: usize,
    connecting: bool,
    reconnect_at: Instant,
    failures: u32,
    closed: bool,
    closing_idle: bool,
}

impl Pool {
    fn release_if_closed(&mut self) {
        if self.closed && self.active == 0 && self.waiting == 0 && !self.closing_idle {
            self.reservation = None;
        }
    }
}

struct Lease<'a> {
    owner: &'a Client,
    connection: Option<Connection>,
    reusable: bool,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut pool = self.owner.pool();
        if self.reusable && !pool.closed {
            pool.idle.extend(self.connection.take());
        } else if let Some(connection) = self.connection.take() {
            // Retire the physical socket before a new checkout can claim its slot.
            drop(pool);
            drop(connection);
            pool = self.owner.pool();
        }
        pool.active -= 1;
        pool.release_if_closed();
        self.owner.changed.notify_one();
    }
}

fn min_deadline(deadline: Instant, caller_deadline: Option<Instant>) -> Instant {
    caller_deadline.map_or(deadline, |caller| deadline.min(caller))
}

struct QueryDeadline<'a> {
    end: Instant,
    cancelled: &'a dyn Fn() -> bool,
}

impl QueryDeadline<'_> {
    fn check_cancel(&self) -> PgResult<()> {
        if (self.cancelled)() {
            Err(CANCELLED)
        } else {
            Ok(())
        }
    }

    fn check(&self) -> PgResult<()> {
        self.check_cancel()?;
        if Instant::now() >= self.end {
            Err(QUERY_TIMEOUT)
        } else {
            Ok(())
        }
    }
}

impl Client {
    fn open(
        config: Config,
        caller_deadline: Option<Instant>,
        cancelled: &dyn Fn() -> bool,
    ) -> PgResult<Arc<Self>> {
        let reservation =
            Reservation::client(config.connections, config.waiters).ok_or_else(|| {
                error(
                    "resource_limit",
                    "database process connection/waiter budget exhausted or invalid",
                )
            })?;
        let mut idle = Vec::new();
        idle.try_reserve_exact(config.connections)
            .map_err(|_| OUT_OF_MEMORY)?;
        let started = Instant::now();
        let first = Connection::connect(
            config.conninfo.as_c_str(),
            min_deadline(started + config.connect_timeout, caller_deadline),
            &mut || if cancelled() { Err(CANCELLED) } else { Ok(()) },
        )?;
        idle.push(first);
        Ok(Arc::new(Self {
            config,
            state: Mutex::new(Pool {
                reservation: Some(reservation),
                idle,
                active: 0,
                waiting: 0,
                connecting: false,
                reconnect_at: started,
                failures: 0,
                closed: false,
                closing_idle: false,
            }),
            changed: Condvar::new(),
        }))
    }

    fn pool(&self) -> MutexGuard<'_, Pool> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn close(&self) {
        let idle = {
            let mut pool = self.pool();
            if pool.closed {
                return;
            }
            pool.closed = true;
            pool.closing_idle = true;
            self.changed.notify_all();
            std::mem::take(&mut pool.idle)
        };
        drop(idle);
        let mut pool = self.pool();
        pool.closing_idle = false;
        pool.release_if_closed();
    }

    fn acquire(
        &self,
        query_deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> PgResult<Lease<'_>> {
        let deadline = query_deadline.min(Instant::now() + self.config.acquire_timeout);
        let mut pool = self.pool();
        let mut waiting = false;
        let acquired = loop {
            let now = Instant::now();
            if pool.closed {
                break Err(CLIENT_CLOSED);
            }
            if cancelled() {
                break Err(CANCELLED);
            }
            if now >= deadline {
                break Err(ACQUIRE_TIMEOUT);
            }
            let has_priority = waiting || pool.waiting == 0;
            let can_connect = !pool.connecting && pool.active < self.config.connections;
            if has_priority {
                if let Some(conn) = pool.idle.pop() {
                    pool.active += 1;
                    break Ok(Some(conn));
                }
                if can_connect && now >= pool.reconnect_at {
                    pool.connecting = true;
                    pool.active += 1;
                    break Ok(None);
                }
            }
            if !waiting {
                if pool.waiting >= self.config.waiters {
                    break Err(error("pool_busy", "PostgreSQL client waiter limit reached"));
                }
                waiting = true;
                pool.waiting += 1;
            }
            let mut wake_at = deadline.min(now + WAIT_CHECK);
            if can_connect && now < pool.reconnect_at {
                wake_at = wake_at.min(pool.reconnect_at);
            }
            pool = self
                .changed
                .wait_timeout(pool, wake_at.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| error.into_inner())
                .0;
        };
        if waiting {
            pool.waiting -= 1;
            pool.release_if_closed();
            self.changed.notify_one();
        }
        drop(pool);
        let connection = acquired?;
        let mut lease = Lease {
            owner: self,
            connection,
            reusable: false,
        };
        if lease.connection.is_none() {
            lease.connection = Some(self.reconnect(deadline, cancelled)?);
        }
        Ok(lease)
    }

    fn reconnect(
        &self,
        acquire_deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> PgResult<Connection> {
        let connect_deadline = Instant::now() + self.config.connect_timeout;
        let result = Connection::connect(
            self.config.conninfo.as_c_str(),
            acquire_deadline.min(connect_deadline),
            &mut || if cancelled() { Err(CANCELLED) } else { Ok(()) },
        );
        let mut pool = self.pool();
        pool.connecting = false;
        let now = Instant::now();
        if result.is_ok() {
            pool.failures = 0;
            pool.reconnect_at = now;
        } else {
            // A failed connection attempt is reported to this caller. Backoff
            // only controls the next independent request; it never resends SQL.
            pool.failures = (pool.failures + 1).min(7);
            pool.reconnect_at = now + Duration::from_millis((25 << (pool.failures - 1)).min(1000));
        }
        let closed = pool.closed;
        self.changed.notify_one();
        drop(pool);
        if closed {
            return Err(CLIENT_CLOSED);
        }
        match result {
            Err(error)
                if error.kind == "connect_timeout" && acquire_deadline <= connect_deadline =>
            {
                Err(ACQUIRE_TIMEOUT)
            }
            Ok(_) if now >= acquire_deadline => Err(ACQUIRE_TIMEOUT),
            result => result,
        }
    }

    fn query(
        &self,
        sql: &str,
        params: &[Value],
        span: Span,
        caller_deadline: Option<Instant>,
        cancelled: &dyn Fn() -> bool,
    ) -> PgResult<Rows> {
        let timing = self.query_deadline(caller_deadline, cancelled);
        let check = || timing.check();
        validate_query_sql(sql, &check)?;
        let input_bytes = pending_input_bytes(sql, params, span, &check)?;
        let _budget = reserve_query_inputs(input_bytes)?;
        let sql = copy_to_cstr(sql, &check)?;
        let mut bound = Vec::new();
        bound
            .try_reserve_exact(params.len())
            .map_err(|_| OUT_OF_MEMORY)?;
        for parameter in params {
            bound.push(Some(with_parameter(parameter, span, |text| {
                copy_to_cstr(text, &check)
            })?));
        }

        self.run_query(&timing, |connection| {
            connection
                .execute(&sql, &bound, timing.end, &mut || timing.check_cancel())
                .map(|table| Ok(Rows(ResultData::Text(table))))
        })
    }

    fn query_typed(
        &self,
        sql: &str,
        arguments: typed::Arguments<'_>,
        span: Span,
        caller_deadline: Option<Instant>,
        cancelled: &dyn Fn() -> bool,
    ) -> PgResult<Rows> {
        let timing = self.query_deadline(caller_deadline, cancelled);
        let check = || timing.check();
        validate_query_sql(sql, &check)?;
        let _budget = reserve_query_inputs(arguments.input_bytes(sql.len(), span, &check)?)?;
        let sql = copy_to_cstr(sql, &check)?;
        let (bound, columns) = arguments.bind(span, &check)?;
        self.run_query(&timing, |connection| {
            let result =
                connection.execute_typed(&sql, &bound, &columns, timing.end, &mut || {
                    timing.check_cancel()
                })?;
            Ok(result.map(|cells| Rows(ResultData::Typed { columns, cells })))
        })
    }

    fn query_deadline<'a>(
        &self,
        caller_deadline: Option<Instant>,
        cancelled: &'a dyn Fn() -> bool,
    ) -> QueryDeadline<'a> {
        QueryDeadline {
            end: min_deadline(Instant::now() + self.config.query_timeout, caller_deadline),
            cancelled,
        }
    }

    fn run_query(
        &self,
        timing: &QueryDeadline<'_>,
        execute: impl FnOnce(&mut Connection) -> PgResult<PgResult<Rows>>,
    ) -> PgResult<Rows> {
        let mut lease = self.acquire(timing.end, timing.cancelled)?;
        let connection = lease
            .connection
            .as_mut()
            .expect("PG checkout has no connection");
        let result = execute(connection)?;
        // Completed typed errors also go through strict reset. An ambiguous
        // transport error above evicts the lease. A failed reset itself does
        // not replace the completed query's result or error.
        if !self.pool().closed {
            lease.reusable = connection
                .reset(timing.end, &mut || timing.check_cancel())
                .is_ok();
        }
        result
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

const SQL_NUL: Error = error("query_error", "SQL contains a NUL byte");
const PARAM_NUL: Error = error("query_error", "PostgreSQL parameter contains a NUL byte");

fn validate_query_sql(sql: &str, check: &impl Fn() -> PgResult<()>) -> PgResult<()> {
    check()?;
    if sql.len() > MAX_SQL_BYTES {
        return Err(error(
            "query_too_large",
            "PostgreSQL SQL text exceeds its limit",
        ));
    }
    check_for_nul(sql, check, SQL_NUL)?;
    sql::validate(sql, check)
}

fn reserve_query_inputs(bytes: usize) -> PgResult<Reservation> {
    Reservation::operation(bytes).ok_or_else(|| {
        error(
            "resource_limit",
            "database pending input byte budget exhausted or invalid",
        )
    })
}

fn check_for_nul(text: &str, check: &impl Fn() -> PgResult<()>, nul: Error) -> PgResult<()> {
    for chunk in text.as_bytes().chunks(INPUT_CHUNK) {
        check()?;
        if chunk.contains(&0) {
            return Err(nul);
        }
    }
    check()
}

// Inputs must first pass check_for_nul. A shared Value cannot change between
// validation and copying while this synchronous operation borrows its arguments.
fn copy_to_cstr(text: &str, check: &impl Fn() -> PgResult<()>) -> PgResult<CString> {
    let mut bytes = Vec::new();
    check()?;
    bytes
        .try_reserve_exact(text.len() + 1)
        .map_err(|_| OUT_OF_MEMORY)?;
    for chunk in text.as_bytes().chunks(INPUT_CHUNK) {
        check()?;
        bytes.extend_from_slice(chunk);
    }
    check()?;
    bytes.push(0);
    // SAFETY: callers already rejected embedded NUL bytes; the only terminator
    // is the appended byte. Avoid a second, unbounded validation pass.
    Ok(unsafe { CString::from_vec_with_nul_unchecked(bytes) })
}

fn with_parameter<T>(
    value: &Value,
    span: Span,
    read: impl FnOnce(&str) -> PgResult<T>,
) -> PgResult<T> {
    value
        .with_read(span, |value| {
            Ok(match value {
                Value::String(text) => read(text),
                _ => Err(error(
                    "query_error",
                    "PostgreSQL query parameters must be strings",
                )),
            })
        })
        .map_err(|_| error("query_error", "PostgreSQL query parameter is unavailable"))?
}

fn pending_input_bytes(
    sql: &str,
    params: &[Value],
    span: Span,
    check: &impl Fn() -> PgResult<()>,
) -> PgResult<usize> {
    if params.len() > MAX_PARAM_COUNT {
        return Err(error(
            "parameter_too_large",
            "PostgreSQL parameter count exceeds 65535",
        ));
    }
    let mut bytes = 0;
    for parameter in params {
        check()?;
        bytes += with_parameter(parameter, span, |text| {
            if text.len() > MAX_PARAM_BYTES - bytes {
                return Err(error(
                    "parameter_too_large",
                    "PostgreSQL query parameters exceed 64 MiB total UTF-8 bytes",
                ));
            }
            check_for_nul(text, check, PARAM_NUL)?;
            Ok(text.len())
        })?;
    }
    // Retained text, terminators, and descriptor storage for the Runner's
    // libpq binding arena. This input budget is not a libpq heap quota.
    Ok(sql.len()
        + 1
        + bytes
        + params.len() * (1 + size_of::<Option<CString>>() + size_of::<*const u8>()))
}

enum ResultData {
    Text(Tabular),
    Typed {
        columns: Vec<typed::Column>,
        cells: Vec<typed::Cell>,
    },
}

pub struct Rows(ResultData);

impl fmt::Debug for Rows {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PgResult")
            .field("rows", &self.row_count())
            .field("cols", &self.col_count())
            .finish()
    }
}

fn canonical_int(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty()
        || digits.len() > 19
        || (digits.starts_with('0') && digits.len() != 1)
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

impl Rows {
    fn row_count(&self) -> usize {
        match &self.0 {
            ResultData::Text(table) => table.rows,
            ResultData::Typed { .. } => 1,
        }
    }

    fn col_count(&self) -> usize {
        match &self.0 {
            ResultData::Text(table) => table.cols,
            ResultData::Typed { columns, .. } => columns.len(),
        }
    }

    fn read(&self, method: &str, row: i64, col: i64) -> PgResult<Value> {
        let bad_index = || error("value_error", "result row or column is out of bounds");
        let row = usize::try_from(row).map_err(|_| bad_index())?;
        let col = usize::try_from(col).map_err(|_| bad_index())?;
        if row >= self.row_count() || col >= self.col_count() {
            return Err(bad_index());
        }
        let table = match &self.0 {
            ResultData::Text(table) => table,
            ResultData::Typed { columns, cells } => {
                return read_typed_cell(method, columns[col], &cells[col]);
            }
        };
        let cell = table
            .cells
            .get(row * table.cols + col)
            .ok_or_else(bad_index)?;
        if method == "is_null" {
            return Ok(Value::Bool(cell.is_none()));
        }
        if method == "value_bool" {
            return Err(bad_index());
        }
        let value = cell.as_deref().ok_or_else(|| {
            error(
                "null_value",
                "PostgreSQL NULL must be checked with is_null()",
            )
        })?;
        if method == "value_int" {
            return canonical_int(value).map(Value::Int).ok_or_else(|| {
                error(
                    "integer_value_error",
                    "PostgreSQL value is not a canonical int64 integer",
                )
            });
        }
        copy_result_text(value)
    }
}

fn read_typed_cell(method: &str, column: typed::Column, cell: &typed::Cell) -> PgResult<Value> {
    use typed::Cell;
    match (method, cell) {
        ("is_null", cell) => Ok(Value::Bool(matches!(cell, Cell::Null))),
        ("value", _) if !column.is_text() => Err(typed::TYPE_ERROR),
        (_, Cell::Null) => Err(typed::NULL_ERROR),
        ("value", Cell::Text(value)) => copy_result_text(value),
        ("value_int", Cell::Int(value)) => Ok(Value::Int(*value)),
        ("value_bool", Cell::Bool(value)) => Ok(Value::Bool(*value)),
        _ => Err(typed::TYPE_ERROR),
    }
}

fn copy_result_text(value: &str) -> PgResult<Value> {
    let mut text = String::new();
    text.try_reserve_exact(value.len())
        .map_err(|_| OUT_OF_MEMORY)?;
    text.push_str(value);
    Ok(Value::String(text))
}

pub(crate) fn eval(
    function: &str,
    args: &[Value],
    span: Span,
    caller_deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> KuResult<Option<Value>> {
    if function != "client" {
        return Ok(None);
    }
    expect_arg_count("pg.client", args.len(), 1, span)?;
    args[0].with_read(span, |value| {
        let result = match value {
            Value::Object(fields) | Value::Struct { fields, .. } => Config::parse(fields, span)
                .and_then(|config| Client::open(config, caller_deadline, cancelled)),
            _ => Err(invalid_config("pg.client requires a config object")),
        };
        Ok(Some(result.map_or_else(result_error, |client| {
            errors::ok(Value::PgClient(client))
        })))
    })
}

pub(crate) fn eval_method(
    module: &str,
    function: &str,
    values: &[Value],
    span: Span,
    caller_deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> KuResult<Option<Value>> {
    let value = match (module, function, values) {
        ("pg_client", "query", [Value::PgClient(client), sql, params]) => {
            sql.with_read(span, |sql| {
                let Value::String(sql) = sql else {
                    return Err(expected_type("str", sql, span));
                };
                params.with_read(span, |params| {
                    let Value::Array(params) = params else {
                        return Err(expected_type("[str]", params, span));
                    };
                    Ok(client
                        .query(sql, params, span, caller_deadline, cancelled)
                        .map_or_else(result_error, |rows| {
                            errors::ok(Value::PgResult(Arc::new(rows)))
                        }))
                })
            })?
        }
        ("pg_client", "close", [Value::PgClient(client)]) => {
            client.close();
            Value::Null
        }
        (
            "pg_client",
            "query_one_typed",
            [Value::PgClient(client), Value::String(sql), Value::Array(params), Value::Array(param_types), Value::Array(result_types)],
        ) => client
            .query_typed(
                sql,
                typed::Arguments {
                    values: params,
                    param_types,
                    result_types,
                },
                span,
                caller_deadline,
                cancelled,
            )
            .map_or_else(result_error, |rows| {
                errors::ok(Value::PgResult(Arc::new(rows)))
            }),
        ("pg_result", "rows", [Value::PgResult(rows)]) => Value::Int(rows.row_count() as i64),
        ("pg_result", "cols", [Value::PgResult(rows)]) => Value::Int(rows.col_count() as i64),
        (
            "pg_result",
            method @ ("value" | "value_int" | "value_bool" | "is_null"),
            [Value::PgResult(rows), Value::Int(row), Value::Int(col)],
        ) => rows
            .read(method, *row, *col)
            .map_or_else(result_error, errors::ok),
        ("pg_client" | "pg_result", _, _) => {
            return Err(KuError::runtime(
                format!("invalid arguments to {module}.{function}"),
                span,
            ));
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}
