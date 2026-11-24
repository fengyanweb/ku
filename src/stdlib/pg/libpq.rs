//! The interpreter loads libpq only when a PostgreSQL connection is needed.
//! A connection belongs to one caller while leased. Results hold no libpq data.

#[path = "libpq_platform.rs"]
mod platform;

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::ErrorKind;
use std::ptr::{null, null_mut, NonNull};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::typed::{self, Cell, Column, Param};
use platform::{Library, POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};

const CONNECTION_OK: c_int = 0;
const CONNECTION_BAD: c_int = 1;
const POLLING_READING: c_int = 1;
const POLLING_WRITING: c_int = 2;
const POLLING_OK: c_int = 3;
const TRANSACTION_IDLE: c_int = 0;
const RESULT_EMPTY_QUERY: c_int = 0;
const RESULT_COMMAND_OK: c_int = 1;
const RESULT_TUPLES_OK: c_int = 2;
const RESULT_FATAL_ERROR: c_int = 7;
const RESULT_SINGLE_TUPLE: c_int = 9;
const ERROR_SQLSTATE: c_int = b'C' as c_int;

// Keep the native backend's bounds. libpq still buffers a complete wire row.
const MAX_ROWS: usize = 1_000_000;
const MAX_COLUMNS: usize = 4_096;
const MAX_CELLS: usize = 1_000_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const COPY_CHUNK_BYTES: usize = 64 * 1024;
const CANCEL_CHECK_INTERVAL_MS: u128 = 25;

#[derive(Clone, Copy, Debug)]
pub(super) struct Error {
    pub(super) kind: &'static str,
    pub(super) message: &'static str,
}

const LIBRARY_ERROR: Error = Error {
    kind: "library_unavailable",
    message: "could not load a compatible thread-safe libpq; set KU_PG_RUNTIME_DIR to its absolute directory",
};
const CONNECT_ERROR: Error = Error {
    kind: "connect_error",
    message: "PostgreSQL connection failed",
};
const CONNECT_TIMEOUT: Error = Error {
    kind: "connect_timeout",
    message: "PostgreSQL connection deadline expired",
};
const QUERY_TIMEOUT: Error = Error {
    kind: "query_timeout",
    message: "PostgreSQL query deadline expired before SQL was sent",
};
const CONNECTION_ERROR: Error = Error {
    kind: "query_error",
    message: "PostgreSQL connection is no longer usable",
};
const QUERY_ERROR: Error = Error {
    kind: "query_error",
    message: "PostgreSQL query failed",
};
const EXECUTION_UNKNOWN: Error = Error {
    kind: "execution_unknown",
    message:
        "PostgreSQL statement may have executed; outcome is unknown; never retry automatically",
};
const COMPLETED_WITHOUT_RESULT: Error = Error {
    kind: "execution_completed_without_result",
    message: "PostgreSQL statement completed but its result could not be delivered; never retry automatically",
};
const OUT_OF_MEMORY: Error = Error {
    kind: "out_of_memory",
    message: "could not allocate PostgreSQL operation storage",
};
const SESSION_STATE_ERROR: Error = Error {
    kind: "session_state_unsupported",
    message: "PostgreSQL statement completed or may have completed; session state is unsupported and its payload was discarded; never retry automatically",
};

#[repr(C)]
struct PgConn {
    _opaque: [u8; 0],
}

#[repr(C)]
struct PgResult {
    _opaque: [u8; 0],
}

type NoticeProcessor = unsafe extern "C" fn(*mut c_void, *const c_char);

// API signatures match libpq-fe.h. Retain the loader handle for the lifetime
// of every function pointer; failure to resolve any required symbol rejects it.
macro_rules! libpq_api {
    ($($field:ident : $symbol:literal => $signature:ty),+ $(,)?) => {
        struct Api {
            _library: Library,
            $($field: $signature),+
        }

        impl Api {
            fn load() -> Option<Self> {
                let library = Library::find()?;
                // These are the symbol names and exact ABI types from libpq-fe.h.
                $(let $field = unsafe {
                    std::mem::transmute::<*mut c_void, $signature>(
                        library.symbol($symbol)?.as_ptr(),
                    )
                };)+
                Some(Self { _library: library, $($field),+ })
            }
        }
    };
}

libpq_api! {
    is_thread_safe: c"PQisthreadsafe" => unsafe extern "C" fn() -> c_int,
    connect_start: c"PQconnectStartParams" => unsafe extern "C" fn(*const *const c_char, *const *const c_char, c_int) -> *mut PgConn,
    connect_poll: c"PQconnectPoll" => unsafe extern "C" fn(*mut PgConn) -> c_int,
    socket: c"PQsocket" => unsafe extern "C" fn(*const PgConn) -> c_int,
    status: c"PQstatus" => unsafe extern "C" fn(*const PgConn) -> c_int,
    transaction_status: c"PQtransactionStatus" => unsafe extern "C" fn(*const PgConn) -> c_int,
    set_nonblocking: c"PQsetnonblocking" => unsafe extern "C" fn(*mut PgConn, c_int) -> c_int,
    parameter_status: c"PQparameterStatus" => unsafe extern "C" fn(*const PgConn, *const c_char) -> *const c_char,
    set_notice_processor: c"PQsetNoticeProcessor" => unsafe extern "C" fn(*mut PgConn, Option<NoticeProcessor>, *mut c_void) -> Option<NoticeProcessor>,
    finish: c"PQfinish" => unsafe extern "C" fn(*mut PgConn),
    send_query: c"PQsendQueryParams" => unsafe extern "C" fn(*mut PgConn, *const c_char, c_int, *const u32, *const *const c_char, *const c_int, *const c_int, c_int) -> c_int,
    single_row_mode: c"PQsetSingleRowMode" => unsafe extern "C" fn(*mut PgConn) -> c_int,
    flush: c"PQflush" => unsafe extern "C" fn(*mut PgConn) -> c_int,
    consume_input: c"PQconsumeInput" => unsafe extern "C" fn(*mut PgConn) -> c_int,
    is_busy: c"PQisBusy" => unsafe extern "C" fn(*mut PgConn) -> c_int,
    get_result: c"PQgetResult" => unsafe extern "C" fn(*mut PgConn) -> *mut PgResult,
    result_status: c"PQresultStatus" => unsafe extern "C" fn(*const PgResult) -> c_int,
    result_error_field: c"PQresultErrorField" => unsafe extern "C" fn(*const PgResult, c_int) -> *mut c_char,
    result_cols: c"PQnfields" => unsafe extern "C" fn(*const PgResult) -> c_int,
    result_rows: c"PQntuples" => unsafe extern "C" fn(*const PgResult) -> c_int,
    result_format: c"PQfformat" => unsafe extern "C" fn(*const PgResult, c_int) -> c_int,
    result_oid: c"PQftype" => unsafe extern "C" fn(*const PgResult, c_int) -> u32,
    result_null: c"PQgetisnull" => unsafe extern "C" fn(*const PgResult, c_int, c_int) -> c_int,
    result_length: c"PQgetlength" => unsafe extern "C" fn(*const PgResult, c_int, c_int) -> c_int,
    result_value: c"PQgetvalue" => unsafe extern "C" fn(*const PgResult, c_int, c_int) -> *mut c_char,
    clear: c"PQclear" => unsafe extern "C" fn(*mut PgResult),
}

fn api() -> Result<&'static Api, Error> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let api = Api::load()?;
        // Connections can move between worker threads after a lease ends.
        let thread_safe = unsafe { (api.is_thread_safe)() } != 0;
        thread_safe.then_some(api)
    })
    .as_ref()
    .ok_or(LIBRARY_ERROR)
}

pub(super) struct Connection {
    raw: NonNull<PgConn>,
    api: &'static Api,
}

// API loading requires a thread-safe libpq. A pool transfers ownership of a
// connection between workers, and every operation requires exclusive access.
unsafe impl Send for Connection {}

impl Connection {
    pub(super) fn connect(
        conninfo: &CStr,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<Self, Error> {
        check_deadline(deadline, CONNECT_TIMEOUT, check)?;
        let api = api()?;
        let keys = [c"dbname".as_ptr(), c"client_encoding".as_ptr(), null()];
        let values = [conninfo.as_ptr(), c"UTF8".as_ptr(), null()];
        check_deadline(deadline, CONNECT_TIMEOUT, check)?;
        // The strings and both terminated arrays outlive the call. Like native
        // libpq, synchronous DNS inside this call cannot be preempted here.
        let raw = unsafe { (api.connect_start)(keys.as_ptr(), values.as_ptr(), 1) };
        let conn = Self {
            raw: NonNull::new(raw).ok_or(OUT_OF_MEMORY)?,
            api,
        };
        check_deadline(deadline, CONNECT_TIMEOUT, check)?;
        // Do not send default libpq notices, which can contain private data, to stderr.
        unsafe { (api.set_notice_processor)(raw, Some(ignore_notice), null_mut()) };
        if unsafe { (api.status)(raw) } == CONNECTION_BAD {
            return Err(CONNECT_ERROR);
        }

        let mut events = POLLOUT;
        loop {
            conn.wait(events, deadline, CONNECT_TIMEOUT, CONNECT_ERROR, check)?;
            // A new socket may replace the old one on each connect poll.
            let state = unsafe { (api.connect_poll)(raw) };
            check_deadline(deadline, CONNECT_TIMEOUT, check)?;
            events = match state {
                POLLING_READING => POLLIN,
                POLLING_WRITING => POLLOUT,
                POLLING_OK => break,
                _ => return Err(CONNECT_ERROR),
            };
        }
        if unsafe { (api.set_nonblocking)(raw, 1) } != 0 || !conn.reusable() {
            return Err(CONNECT_ERROR);
        }
        check_deadline(deadline, CONNECT_TIMEOUT, check)?;
        Ok(conn)
    }

    pub(super) fn execute(
        &mut self,
        sql: &CStr,
        params: &[Option<CString>],
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<Tabular, Error> {
        check_deadline(deadline, QUERY_TIMEOUT, check)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(params.len())
            .map_err(|_| OUT_OF_MEMORY)?;
        values.extend(
            params
                .iter()
                .map(|value| value.as_ref().map_or(null(), |s| s.as_ptr())),
        );
        self.query(
            sql,
            Bindings::text(&values),
            Rows::default(),
            deadline,
            check,
        )?
    }

    pub(super) fn execute_typed(
        &mut self,
        sql: &CStr,
        params: &[Param],
        columns: &[Column],
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<Result<Vec<Cell>, Error>, Error> {
        if columns.len() > MAX_COLUMNS {
            return Err(typed::SCHEMA_ERROR);
        }
        let mut values = Vec::new();
        let mut oids = Vec::new();
        let mut lengths = Vec::new();
        let mut formats = Vec::new();
        let count = params.len();
        values.try_reserve_exact(count).map_err(|_| OUT_OF_MEMORY)?;
        oids.try_reserve_exact(count).map_err(|_| OUT_OF_MEMORY)?;
        lengths
            .try_reserve_exact(count)
            .map_err(|_| OUT_OF_MEMORY)?;
        formats
            .try_reserve_exact(count)
            .map_err(|_| OUT_OF_MEMORY)?;
        for param in params {
            check_deadline(deadline, QUERY_TIMEOUT, check)?;
            oids.push(param.oid);
            formats.push(1);
            match param.bytes() {
                Some(bytes) => {
                    // Binary values use explicit lengths. An empty slice's
                    // non-null pointer denotes empty text, while null denotes NULL.
                    values.push(bytes.as_ptr().cast::<c_char>());
                    lengths.push(c_int::try_from(bytes.len()).map_err(|_| typed::SCHEMA_ERROR)?);
                }
                None => {
                    values.push(null());
                    lengths.push(0);
                }
            }
        }
        let bindings = Bindings {
            values: &values,
            oids: &oids,
            lengths: &lengths,
            formats: &formats,
            result_format: 1,
        };
        self.query(
            sql,
            bindings,
            TypedRow { columns, row: None },
            deadline,
            check,
        )
    }

    pub(super) fn reset(
        &mut self,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let result = self.query(
            c"DISCARD ALL",
            Bindings::text(&[]),
            Rows::default(),
            deadline,
            check,
        )??;
        if result.rows == 0 && result.cols == 0 && self.reusable() {
            return Ok(());
        }
        Err(CONNECTION_ERROR)
    }

    fn reusable(&self) -> bool {
        let raw = self.raw.as_ptr();
        // All queries use this exclusively leased connection. libpq owns the
        // returned NUL-terminated encoding string until another operation.
        unsafe {
            if (self.api.status)(raw) != CONNECTION_OK
                || (self.api.transaction_status)(raw) != TRANSACTION_IDLE
            {
                return false;
            }
            let encoding = (self.api.parameter_status)(raw, c"client_encoding".as_ptr());
            !encoding.is_null() && CStr::from_ptr(encoding) == c"UTF8"
        }
    }

    fn query<R: RowCollector>(
        &mut self,
        sql: &CStr,
        bindings: Bindings<'_>,
        rows: R,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<Result<R::Output, Error>, Error> {
        let raw = self.raw.as_ptr();
        let api = self.api;
        let count = c_int::try_from(bindings.values.len()).map_err(|_| QUERY_ERROR)?;
        check_deadline(deadline, QUERY_TIMEOUT, check)?;
        if !self.reusable() || unsafe { (api.consume_input)(raw) } == 0 || !self.reusable() {
            return Err(CONNECTION_ERROR);
        }
        check_deadline(deadline, QUERY_TIMEOUT, check)?;

        // The slices and their backing values stay live through the single
        // send. A failed send can follow a partial write.
        #[cfg(test)]
        super::tests::observe_send(sql);
        let sent = unsafe {
            (api.send_query)(
                raw,
                sql.as_ptr(),
                count,
                data_or_null(bindings.oids),
                data_or_null(bindings.values),
                data_or_null(bindings.lengths),
                data_or_null(bindings.formats),
                bindings.result_format,
            )
        };
        if sent == 0
            // This call must immediately follow the send, before any input is consumed.
            || unsafe { (api.single_row_mode)(raw) } == 0
        {
            return Err(EXECUTION_UNKNOWN);
        }

        loop {
            check_deadline(deadline, QUERY_TIMEOUT, check).map_err(|_| EXECUTION_UNKNOWN)?;
            match unsafe { (api.flush)(raw) } {
                0 => break,
                1 => {
                    let ready = self
                        .wait(
                            POLLIN | POLLOUT,
                            deadline,
                            QUERY_TIMEOUT,
                            EXECUTION_UNKNOWN,
                            check,
                        )
                        .map_err(|_| EXECUTION_UNKNOWN)?;
                    if ready & (POLLIN | POLLERR | POLLHUP) != 0
                        && unsafe { (api.consume_input)(raw) } == 0
                    {
                        return Err(EXECUTION_UNKNOWN);
                    }
                }
                _ => return Err(EXECUTION_UNKNOWN),
            }
        }
        self.receive(rows, deadline, check)
    }

    fn receive<R: RowCollector>(
        &mut self,
        rows: R,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<Result<R::Output, Error>, Error> {
        let raw = self.raw.as_ptr();
        let api = self.api;
        let mut terminal = None;
        let mut rows = Ok(rows);
        loop {
            let delivery_error = if terminal == Some(true) {
                COMPLETED_WITHOUT_RESULT
            } else {
                EXECUTION_UNKNOWN
            };
            check_deadline(deadline, QUERY_TIMEOUT, check).map_err(|_| delivery_error)?;
            // PQgetResult can block even in nonblocking mode unless PQisBusy
            // first confirms that a result or the terminal null is buffered.
            if unsafe { (api.is_busy)(raw) } != 0 {
                // libpq may already have input at its TLS layer, so consume
                // nonblocking input before depending on socket readiness.
                if unsafe { (api.consume_input)(raw) } == 0 {
                    return Err(delivery_error);
                }
                if unsafe { (api.is_busy)(raw) } != 0 {
                    self.wait(POLLIN, deadline, QUERY_TIMEOUT, delivery_error, check)
                        .map_err(|_| delivery_error)?;
                    continue;
                }
            }
            check_deadline(deadline, QUERY_TIMEOUT, check).map_err(|_| delivery_error)?;
            let next = unsafe { (api.get_result)(raw) };
            let Some(next) = NonNull::new(next) else {
                check_deadline(deadline, QUERY_TIMEOUT, check).map_err(|_| delivery_error)?;
                let healthy = if R::TYPED {
                    (unsafe { (api.status)(raw) }) == CONNECTION_OK
                } else {
                    self.reusable()
                };
                if !healthy {
                    return Err(delivery_error);
                }
                return match terminal {
                    Some(true) => {
                        let result = rows.and_then(RowCollector::finish);
                        if R::TYPED
                            && result.is_ok()
                            && unsafe { (api.transaction_status)(raw) } != TRANSACTION_IDLE
                        {
                            return Ok(Err(SESSION_STATE_ERROR));
                        }
                        Ok(result)
                    }
                    Some(false) => Ok(Err(QUERY_ERROR)),
                    None => Err(EXECUTION_UNKNOWN),
                };
            };
            let result = ResultHandle { raw: next, api };
            // Extended query protocol accepts exactly one statement.
            if terminal.is_some() {
                return Err(delivery_error);
            }
            match unsafe { (api.result_status)(next.as_ptr()) } {
                status @ (RESULT_SINGLE_TUPLE | RESULT_TUPLES_OK | RESULT_COMMAND_OK) => {
                    if status != RESULT_SINGLE_TUPLE {
                        terminal = Some(true);
                    }
                    if let Ok(current) = &mut rows {
                        match current.collect(&result, status, deadline, check) {
                            Ok(()) => {}
                            Err(RowFailure::Invalid(error)) => {
                                // Remember the first error and keep draining. A
                                // later server error supersedes partial rows.
                                rows = Err(error);
                            }
                            Err(RowFailure::Interrupted) => {
                                return Err(if terminal == Some(true) {
                                    COMPLETED_WITHOUT_RESULT
                                } else {
                                    EXECUTION_UNKNOWN
                                });
                            }
                        }
                    }
                }
                RESULT_FATAL_ERROR if result.has_sqlstate() => {
                    rows = Err(QUERY_ERROR);
                    terminal = Some(false);
                }
                RESULT_EMPTY_QUERY => terminal = Some(false),
                // COPY and malformed streams cannot use ordinary result drain.
                _ => return Err(EXECUTION_UNKNOWN),
            }
        }
    }

    fn wait(
        &self,
        events: i16,
        deadline: Instant,
        timeout: Error,
        failure: Error,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<i16, Error> {
        let socket = unsafe { (self.api.socket)(self.raw.as_ptr()) };
        if socket < 0 {
            return Err(failure);
        }

        loop {
            check_deadline(deadline, timeout, check)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining
                .as_nanos()
                .div_ceil(Duration::from_millis(1).as_nanos());
            let timeout_ms = millis.min(CANCEL_CHECK_INTERVAL_MS) as c_int;
            match platform::poll(socket, events, timeout_ms) {
                Ok(0) => continue,
                Ok(ready) if ready & POLLNVAL == 0 && ready & (events | POLLERR | POLLHUP) != 0 => {
                    check_deadline(deadline, timeout, check)?;
                    return Ok(ready);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                _ => return Err(failure),
            }
        }
    }
}

struct Bindings<'a> {
    values: &'a [*const c_char],
    oids: &'a [u32],
    lengths: &'a [c_int],
    formats: &'a [c_int],
    result_format: c_int,
}

impl<'a> Bindings<'a> {
    fn text(values: &'a [*const c_char]) -> Self {
        Self {
            values,
            oids: &[],
            lengths: &[],
            formats: &[],
            result_format: 0,
        }
    }
}

fn data_or_null<T>(values: &[T]) -> *const T {
    if values.is_empty() {
        null()
    } else {
        values.as_ptr()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Closing discards any outstanding exchange, without claiming its SQL
        // rolled back. No connection with an incomplete exchange is reused.
        unsafe { (self.api.finish)(self.raw.as_ptr()) };
    }
}

unsafe extern "C" fn ignore_notice(_: *mut c_void, _: *const c_char) {}

fn check_deadline(
    deadline: Instant,
    timeout: Error,
    check: &mut impl FnMut() -> Result<(), Error>,
) -> Result<(), Error> {
    check()?;
    if Instant::now() >= deadline {
        return Err(timeout);
    }
    Ok(())
}

struct ResultHandle {
    raw: NonNull<PgResult>,
    api: &'static Api,
}

impl ResultHandle {
    fn has_sqlstate(&self) -> bool {
        // Read only whether a server SQLSTATE exists. Never expose libpq's
        // server-supplied message, details, hints, or connection strings.
        !unsafe { (self.api.result_error_field)(self.raw.as_ptr(), ERROR_SQLSTATE) }.is_null()
    }
}

impl Drop for ResultHandle {
    fn drop(&mut self) {
        unsafe { (self.api.clear)(self.raw.as_ptr()) };
    }
}

#[derive(Default)]
pub(super) struct Tabular {
    pub(super) rows: usize,
    pub(super) cols: usize,
    pub(super) cells: Vec<Option<String>>,
}

#[derive(Default)]
struct Rows {
    table: Tabular,
    bytes: usize,
}

enum RowFailure {
    Invalid(Error),
    Interrupted,
}

trait RowCollector {
    type Output;
    const TYPED: bool;

    fn collect(
        &mut self,
        result: &ResultHandle,
        status: c_int,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<(), RowFailure>;

    fn finish(self) -> Result<Self::Output, Error>;
}

impl RowCollector for Rows {
    type Output = Tabular;
    const TYPED: bool = false;

    fn collect(
        &mut self,
        result: &ResultHandle,
        status: c_int,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<(), RowFailure> {
        self.append(result, status, deadline, check)
            .then_some(())
            .ok_or(RowFailure::Invalid(COMPLETED_WITHOUT_RESULT))
    }

    fn finish(self) -> Result<Self::Output, Error> {
        Ok(self.table)
    }
}

impl Rows {
    fn append(
        &mut self,
        result: &ResultHandle,
        status: c_int,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> bool {
        let raw = result.raw.as_ptr();
        let api = result.api;
        let rows = unsafe { (api.result_rows)(raw) };
        let cols = unsafe { (api.result_cols)(raw) };
        if rows != c_int::from(status == RESULT_SINGLE_TUPLE)
            || !(0..=MAX_COLUMNS as c_int).contains(&cols)
            || (status == RESULT_COMMAND_OK && (self.table.rows != 0 || cols != 0))
            || (self.table.rows != 0 && self.table.cols != cols as usize)
        {
            return false;
        }
        let next_rows = self.table.rows + rows as usize;
        let next_cells = next_rows * cols as usize;
        if next_rows > MAX_ROWS || next_cells > MAX_CELLS {
            return false;
        }
        self.table.cols = cols as usize;
        let capacity = self.table.cells.capacity();
        if next_cells > capacity {
            let target = (capacity * 2).max(16).max(next_cells).min(MAX_CELLS);
            if self
                .table
                .cells
                .try_reserve_exact(target - self.table.cells.len())
                .is_err()
            {
                return false;
            }
        }
        for col in 0..cols {
            // Column indexes are in range. Text mode and UTF-8 are part of
            // Runner's ordinary result contract, including for empty results.
            if check_deadline(deadline, QUERY_TIMEOUT, check).is_err()
                || unsafe { (api.result_format)(raw, col) } != 0
            {
                return false;
            }
            if rows == 1 {
                let Some(value) = self.copy_cell(result, col, deadline, check) else {
                    return false;
                };
                self.table.cells.push(value);
            }
        }
        self.table.rows = next_rows;
        true
    }

    fn copy_cell(
        &mut self,
        result: &ResultHandle,
        col: c_int,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Option<Option<String>> {
        let raw = result.raw.as_ptr();
        let api = result.api;
        // Called only for a single tuple and a valid column index.
        if unsafe { (api.result_null)(raw, 0, col) } != 0 {
            return Some(None);
        }
        let length = usize::try_from(unsafe { (api.result_length)(raw, 0, col) }).ok()?;
        if length > MAX_BYTES - self.bytes {
            return None;
        }
        let data = NonNull::new(unsafe { (api.result_value)(raw, 0, col) })?;
        // libpq guarantees `length` readable bytes until ResultHandle clears it.
        let data = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), length) };
        let mut owned = Vec::new();
        owned.try_reserve_exact(length).ok()?;
        for chunk in data.chunks(COPY_CHUNK_BYTES) {
            check_deadline(deadline, QUERY_TIMEOUT, check).ok()?;
            owned.extend_from_slice(chunk);
        }
        self.bytes += length;
        String::from_utf8(owned).ok().map(Some)
    }
}

struct TypedRow<'a> {
    columns: &'a [Column],
    row: Option<Vec<Cell>>,
}

impl RowCollector for TypedRow<'_> {
    type Output = Vec<Cell>;
    const TYPED: bool = true;

    fn collect(
        &mut self,
        result: &ResultHandle,
        status: c_int,
        deadline: Instant,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<(), RowFailure> {
        use RowFailure::{Interrupted, Invalid};
        let raw = result.raw.as_ptr();
        let api = result.api;
        let has_row = status == RESULT_SINGLE_TUPLE;
        // Single-row mode always ends a regular query with a zero-row result.
        // Its column types and formats are still part of the contract.
        if unsafe { (api.result_rows)(raw) } != c_int::from(has_row)
            || unsafe { (api.result_cols)(raw) } != self.columns.len() as c_int
        {
            return Err(Invalid(typed::SCHEMA_ERROR));
        }
        if has_row && self.row.is_some() {
            return Err(Invalid(typed::ROWS_ERROR));
        }
        let mut owned = Vec::new();
        if has_row {
            owned
                .try_reserve_exact(self.columns.len())
                .map_err(|_| Interrupted)?;
        }
        let mut bytes = 0;
        for (index, column) in self.columns.iter().enumerate() {
            let col = index as c_int;
            check_deadline(deadline, QUERY_TIMEOUT, check).map_err(|_| Interrupted)?;
            if unsafe { (api.result_oid)(raw, col) } != column.oid {
                return Err(Invalid(typed::OID_ERROR));
            }
            if unsafe { (api.result_format)(raw, col) } != 1 {
                return Err(Invalid(typed::FORMAT_ERROR));
            }
            if !has_row {
                continue;
            }
            if unsafe { (api.result_null)(raw, 0, col) } != 0 {
                if !column.nullable {
                    return Err(Invalid(typed::NULL_ERROR));
                }
                owned.push(Cell::Null);
                continue;
            }
            let length = usize::try_from(unsafe { (api.result_length)(raw, 0, col) })
                .map_err(|_| Invalid(typed::LENGTH_ERROR))?;
            if length > MAX_BYTES - bytes {
                return Err(Invalid(typed::TYPE_ERROR));
            }
            bytes += length;
            let data = if length == 0 {
                &[][..]
            } else {
                let data = NonNull::new(unsafe { (api.result_value)(raw, 0, col) })
                    .ok_or(Invalid(typed::LENGTH_ERROR))?;
                // libpq owns this validated cell until ResultHandle clears it.
                unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), length) }
            };
            let cell = match column.oid {
                typed::INT4 => Cell::Int(i64::from(i32::from_be_bytes(
                    data.try_into().map_err(|_| Invalid(typed::LENGTH_ERROR))?,
                ))),
                typed::INT8 => Cell::Int(i64::from_be_bytes(
                    data.try_into().map_err(|_| Invalid(typed::LENGTH_ERROR))?,
                )),
                typed::BOOL => match data {
                    [0] => Cell::Bool(false),
                    [1] => Cell::Bool(true),
                    [_] => return Err(Invalid(typed::TYPE_ERROR)),
                    _ => return Err(Invalid(typed::LENGTH_ERROR)),
                },
                typed::TEXT | typed::VARCHAR => Cell::Text(copy_typed_text(data, deadline, check)?),
                _ => return Err(Invalid(typed::SCHEMA_ERROR)),
            };
            owned.push(cell);
        }
        if has_row {
            self.row = Some(owned);
        }
        Ok(())
    }

    fn finish(self) -> Result<Self::Output, Error> {
        self.row.ok_or(typed::ROWS_ERROR)
    }
}

fn copy_typed_text(
    mut data: &[u8],
    deadline: Instant,
    check: &mut impl FnMut() -> Result<(), Error>,
) -> Result<String, RowFailure> {
    use RowFailure::{Interrupted, Invalid};
    let mut owned = String::new();
    owned
        .try_reserve_exact(data.len())
        .map_err(|_| Interrupted)?;
    while !data.is_empty() {
        check_deadline(deadline, QUERY_TIMEOUT, check).map_err(|_| Interrupted)?;
        let mut end = data.len().min(COPY_CHUNK_BYTES);
        if end < data.len() {
            // A UTF-8 scalar crosses at most three trailing continuation bytes.
            while data[end] & 0xc0 == 0x80 {
                if COPY_CHUNK_BYTES - end == 3 {
                    return Err(Invalid(typed::TYPE_ERROR));
                }
                end -= 1;
            }
        }
        let (chunk, rest) = data.split_at(end);
        if chunk.contains(&0) {
            return Err(Invalid(typed::TYPE_ERROR));
        }
        let chunk = std::str::from_utf8(chunk).map_err(|_| Invalid(typed::TYPE_ERROR))?;
        owned.push_str(chunk);
        data = rest;
    }
    Ok(owned)
}
