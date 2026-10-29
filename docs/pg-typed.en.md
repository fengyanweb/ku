# PostgreSQL typed single-row queries

This capability is on the performance development branch and follows PostgreSQL's existing native C backend support. The interpreter and LLVM do not execute database modules. The existing `client.query(sql, [str])` text API remains available.

```ku
row = client.query_one_typed(
    "SELECT $1::int4, $2::bool, $3::text, $4::text",
    [17, true, "中文🙂", null],
    ["int4", "bool", "text", "text?"],
    ["int4", "bool", "text", "text?"]
)?
id = row.value_int(0, 0)?
enabled = row.value_bool(0, 1)?
label = row.value(0, 2)?
absent = row.is_null(0, 3)?
```

See the complete [pg_typed_demo.ku](../examples/pg_typed_demo.ku). Supply a private libpq connection string in `db.conn`; do not commit credentials. The query reads its parameter array. Constructing that array still follows Ku's existing string ownership rules. A mixed literal is contextually converted only in this query argument; the example does not imply that an arbitrary heterogeneous array declaration is accepted.

## Declarations and results

`query_one_typed(sql, params, parameter_types, result_types): PgResult!` accepts SQL, `[KuValue]`, parameter declarations and result-column declarations. Parameter declarations must match the parameter count, and result declarations must match the actual column count. Names are case-sensitive:

| Declaration | Ku parameter | PostgreSQL binary representation | Getter |
|---|---|---|---|
| `int4` | `int`, -2147483648…2147483647 | INT4, 4 bytes, network order | `value_int(0, col)?` |
| `int8` | Signed 64-bit `int` | INT8, 8 bytes, network order | `value_int(0, col)?` |
| `bool` | `bool` | BOOL, exactly one byte containing 0 or 1 | `value_bool(0, col)?` |
| `text` | UTF-8 `str` | TEXT | `value(0, col)?` |
| `varchar` | UTF-8 `str` | VARCHAR | `value(0, col)?` |

A `?` suffix permits NULL, for example `int8?`. Non-null declarations reject NULL. Check nullable fields with `is_null(0, col)?` before reading them. NULL and an empty string remain distinct. Text parameters reject embedded NUL, and text results undergo UTF-8 validation. Unsupported types—including numeric, floating-point, arrays and domains—are rejected without guessing or implicit text fallback.

The query requires **exactly one row**. Zero or multiple rows return `pg/row_count_error`; later errors are not hidden by taking only the first row. `rows()`, `cols()` and `is_null()` remain available. For typed rows, `value_int()` accepts INT4/INT8, `value_bool()` accepts BOOL, and `value()` accepts TEXT/VARCHAR. `value()` still returns a fresh owned string, not a zero-copy view. The text result's existing `value_int()` behavior is unchanged.

## Task usage

[pg_typed_http.ku](../examples/pg_typed_http.ku) shows an HTTP Task binding dynamic text, an integer, a boolean and NULL. The Task bridge accepts a local, flat parameter literal with scalar `int`/`bool`/`str`/`null` elements. This does not enable general `KuValue` Task arguments or returns, nested JSON parameters, or arbitrary dynamic parameter arrays. Type declarations in the example are fixed arrays. The native synchronous API can also borrow an existing `[KuValue]` array whose elements already have that type. Neither API moves or drops its inputs.

## Errors and ownership

Invalid parameters, declarations, ranges, OIDs, formats, lengths or UTF-8 produce explicit errors, usually `typed_value_error`; NULL uses `null_value`. Declaration and parameter errors are rejected before sending SQL. Result errors can occur after the statement has executed, so they do not justify replaying a write.

A dedicated single-row owner stores binary scalars and Ku-owned string bytes independently of `PGresult`. The row survives result disposal, connection reset and pool closure. Integer and boolean getters do not construct strings or invoke JSON parsing. The Task ledger includes actual owned storage, copied parameters and metadata. libpq's internal buffers remain covered by the process limit, not exact per-Task accounting.

Acquisition, sending, reading and reset share the original absolute deadline. A query owns one exclusive lease and drains its results before same-lease `DISCARD ALL`. Failed cleanup or unverified state evicts the connection. A result already detached from libpq remains available if reset fails, following the existing rule. Cancellation, disconnects and expiry may report `execution_unknown`; closing a connection does not prove rollback, and statements are never retried automatically.

See [libpq parameter execution](https://www.postgresql.org/docs/16/libpq-exec.html) for binary bindings and [asynchronous result handling](https://www.postgresql.org/docs/16/libpq-async.html) for draining.
