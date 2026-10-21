---
title: Database clients
description: PostgreSQL, Redis, MySQL and MariaDB configuration, queries, results, errors, and lifecycle.
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/backend/c.rs',
    'src/cli.rs',
    'docs/protocol-foundation.md',
  ]
sidebar:
  order: 9
---

Ku provides bounded, pooled clients through `std.pg`, `std.redis`, and `std.mysql`. These are **native C APIs**. The interpreter and its default packaged runner do not implement database clients. Constructors return Results; a connected client is move-only and is released at scope exit or by consuming `client.close()`.

## Build and dependencies

Save one complete example below to its stated filename. Check it with `ku check file.ku`, then use:

```sh
ku build --backend c -o database-example file.ku
./database-example
```

The examples require your own local database server and credentials. They are not offline demos. Use a disposable database and Redis key namespace. Windows output files need an `.exe` suffix.

| Module      | Native dependency                          | Runtime requirements                                                                                                                |
| ----------- | ------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| `std.pg`    | Shared libpq                               | Target-compatible libpq and its dependencies must be available to the loader. `KU_PG_LIB` can select an absolute library directory. |
| `std.redis` | Built-in RESP2 implementation              | A Redis server; no hiredis dependency.                                                                                              |
| `std.mysql` | Oracle MySQL client or MariaDB Connector/C | Matching shared library and headers from the same installation; select with `KU_MYSQL_LIB` and `KU_MYSQL_INCLUDE` when needed.      |

These variables configure native linking, not database credentials. MySQL automatic linking supports host builds; an explicit non-host target has no automatic MySQL library contract. The compiler's library checks do not install or bundle a database server or guarantee the complete server/client/platform matrix.

## Pool configuration shared by all clients

| Field                    | Default | Allowed range / purpose                                                 |
| ------------------------ | ------- | ----------------------------------------------------------------------- |
| `max_connections:int`    | 8       | `1..256`; maximum pool connections.                                     |
| `max_waiters:int`        | 64      | `0..4096`; maximum waiting operations. Zero disables waiting when busy. |
| `connect_timeout_ms:int` | 5000    | `1..300000`; connection budget.                                         |
| `acquire_timeout_ms:int` | 5000    | `1..300000`; wait for a pool connection.                                |
| `query_timeout_ms:int`   | 30000   | SQL clients, `1..300000`; query budget.                                 |
| `command_timeout_ms:int` | 5000    | Redis, `1..300000`; command budget.                                     |

Only use fields applicable to that client. Unknown fields and invalid types or ranges return `invalid_config`. Pool operations can report `pool_busy`, `acquire_timeout`, `connect_timeout`, `connect_error`, `client_closed`, or `out_of_memory`, with the appropriate module domain. Limits do not give a hard deadline for every synchronous DNS or external library call, and a result-size cap is not a process memory cap.

## PostgreSQL

Import `pg` from `std.pg`. In addition to pool fields, `pg.client(config)` requires `conninfo:str`, a libpq connection string or URI. No separate `pg.connect` or raw connection API is provided.

| Method                                    | Returns       | Meaning                                                                         |
| ----------------------------------------- | ------------- | ------------------------------------------------------------------------------- |
| `client.query(sql:str, parameters:[str])` | result Result | Parameterized SQL using `$1`, `$2`, …; pass `[]` when no parameters are needed. |
| `result.rows()` / `result.cols()`         | `int`         | Detached result dimensions.                                                     |
| `result.value(row:int, column:int)`       | `str!`        | Copy a non-NULL cell as text.                                                   |
| `result.is_null(row:int, column:int)`     | `bool!`       | Distinguish SQL NULL from an empty string.                                      |
| `client.close()`                          | `null`        | Consume and close the client.                                                   |

Place your connection string in a private `db.conn` next to `postgres.ku`. For a local test, use your local host, database and account; for a remote connection, configure `sslmode=verify-full` and a trusted `sslrootcert` in conninfo. Do not commit this file or print it.

```ku
import pg from "std.pg"
import fs from "std.fs"

fn main(): null! {
    client = pg.client({
        conninfo: fs.read("db.conn")?,
        max_connections: 4,
        max_waiters: 16,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 10000
    })?
    result = client.query("SELECT $1::text, NULL::text", ["中😀"])?
    println(result.rows())
    println(result.cols())
    println(result.value(0, 0)?)
    println(result.is_null(0, 1)?)
    client.close()
    return ok(null)
}
```

With a reachable PostgreSQL server, output is:

```text
1
2
中😀
true
```

`query` returns owned results detached from the pool connection, so result reads do not hold a pool slot. `value` on SQL NULL returns `pg/null_value`; invalid coordinates return a recoverable error. Explicit transactions, session state, COPY and exclusive connections are not supported by this shared client.

## Redis

Import `redis` from `std.redis`. `redis.client(config)` requires `host:str`; `port:int` defaults to 6379. Optional `password:str` enables per-connection AUTH; optional `username:str` requires a password and selects ACL-style AUTH. Do not send real credentials to an untrusted or public plaintext network.

| Method                           | Returns | Meaning                                             |
| -------------------------------- | ------- | --------------------------------------------------- |
| `client.ping()`                  | `null!` | Check the connection with PING.                     |
| `client.set(key:str, value:str)` | `null!` | Set a text value.                                   |
| `client.get(key:str)`            | `str!`  | Fetch text; a missing key is `redis/key_not_found`. |
| `client.exists(key:str)`         | `bool!` | Check key existence.                                |
| `client.del(key:str)`            | `int!`  | Number of keys deleted, zero or one.                |
| `client.close()`                 | `null`  | Consume and close the client.                       |

Save as `redis.ku`. This example expects an unprotected local disposable Redis at `127.0.0.1:6379`; it creates and deletes only `ku:docs:greeting`. Adjust configuration for your own instance.

```ku
import redis from "std.redis"

fn main(): null! {
    client = redis.client({
        host: "127.0.0.1",
        port: 6379,
        max_connections: 4,
        max_waiters: 16,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        command_timeout_ms: 5000
    })?
    client.ping()?
    client.set("ku:docs:greeting", "中😀")?
    println(client.get("ku:docs:greeting")?)
    println(client.exists("ku:docs:greeting")?)
    println(client.del("ku:docs:greeting")?)
    try {
        client.get("ku:docs:greeting")?
    } catch (error) {
        println(error.code)
    }
    client.close()
    return ok(null)
}
```

```text
中😀
true
1
key_not_found
```

Redis values are text and must be valid UTF-8; missing keys are not silently converted to empty strings. A bulk value is limited to 64 MiB. This client supports the listed RESP2 commands, without TLS, arbitrary command dispatch, RESP3, cluster, or sentinel. `std.net` TLS support does not make this Redis client encrypted.

## MySQL and MariaDB

Import `mysql` from `std.mysql`. `mysql.client(config)` requires `host:str`, `user:str`, `password:str`, and `database:str`; password may be empty, the other strings may not. `port:int` defaults to 3306.

| Method                                      | Returns       | Meaning                                                           |
| ------------------------------------------- | ------------- | ----------------------------------------------------------------- |
| `client.query(sql:str, parameters:[str])`   | result Result | Prepared statement returning a detached result; `?` placeholders. |
| `client.execute(sql:str, parameters:[str])` | `int!`        | Prepared statement returning the affected-row count.              |
| `result.rows()` / `result.cols()`           | `int`         | Result dimensions.                                                |
| `result.value(row:int, column:int)`         | `str!`        | Read a non-NULL cell as owned text.                               |
| `result.is_null(row:int, column:int)`       | `bool!`       | Check SQL NULL with the same coordinate validation.               |
| `client.close()`                            | `null`        | Consume and close the client.                                     |

Save as `mysql.ku`. Create a disposable local database and account named `ku_docs`, and put only its password in a private `mysql.pw` beside the source, without a trailing newline. This example reads data only and creates no tables.

```ku
import mysql from "std.mysql"
import fs from "std.fs"

fn main(): null! {
    client = mysql.client({
        host: "127.0.0.1",
        port: 3306,
        user: "ku_docs",
        password: fs.read("mysql.pw")?,
        database: "ku_docs",
        max_connections: 4,
        max_waiters: 16,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 10000
    })?
    result = client.query("SELECT ?, NULL", ["中😀"])?
    println(result.rows())
    println(result.cols())
    println(result.value(0, 0)?)
    println(result.is_null(0, 1)?)
    client.close()
    return ok(null)
}
```

Expected output matches the PostgreSQL example. All queries use prepared statements, including those with no parameters (`[]`). Parameter values are `[str]`, without typed or SQL NULL binding. SQL NULL reads return `mysql/null_value`; use `is_null` first. Transactions, session-dependent operations, exclusive connections and top-level `CALL` are not supported. This release exposes no configuration that requires encrypted MySQL transport with verified certificate and hostname; do not claim otherwise.

## Errors, cleanup, and side effects

Use `?` to propagate an error, or `try/catch` to inspect its `domain`, `code`, and `message`. Clients close at the end of their owning scope, including ordinary error unwinding. `close()` consumes a client, so subsequent use is rejected. SQL results own their data independently and are released when their owner ends; strings returned from `value` are independently owned.

The SQL clients do **not automatically retry statements**. `execution_unknown` means a statement was sent but its final outcome is not known. `execution_completed_without_result` means execution completed but a usable result could not be returned. `session_state_unsupported` can be detected before sending or after execution; its message distinguishes the stage. Do not infer rollback or retryability from a timeout, a worker exit, a reset, or these error codes. Confirm uncertain writes through business identifiers and database state before any manual retry.

A pool reset clears supported per-connection state; it does not undo committed changes or external effects. Keep SQL application-controlled, bind values as parameters, and use a database account limited to the required operations. This page describes the v0.0.18 API; it does not claim a completed database platform matrix.
