# Go standard HTTP comparison service

Use the locked Go 1.27.2 module and normal GC/build optimizations:

```sh
go mod download
go mod verify
go build -trimpath -o /absolute/output/go-server .
BENCH_ADDR=127.0.0.1:19090 BENCH_WORKERS=4 BENCH_CPU_WORKERS=4 BENCH_DB_POOL=16 BENCH_PEER_PORT=19091 BENCH_DB_URL=postgresql://user@127.0.0.1:19092/test?sslmode=disable /absolute/output/go-server
```

All environment variables are private test configuration. The database must be
the isolated fixture specified by `../protocol.json`. `BENCH_WORKERS` sets
`GOMAXPROCS`; it does not constrain Go's total OS thread count. Count the entire
service inside its CPU/memory resource domain. Do not disable GC or omit helper
threads from resource measurements.

HTTP uses `net/http`; JSON uses ordinary `encoding/json` decoding into native
values and fresh encoding, without `RawMessage` or serialization-result caches.
CPU requests use a bounded pool of `BENCH_CPU_WORKERS` goroutines and a 128-entry
channel; a full channel returns an explicit 503. Ordinary HTTP concurrency uses
Go's standard server behavior, without an invented queue to reproduce Ku's
internal admission scheme. Timer/TCP waits suspend goroutines. `/metrics` reports
actual `waiting_timer`, `waiting_tcp`, and active business handlers, excluding
the metrics query itself.

`actual_cpu_jobs` retains queued and running work after an HTTP request cancels.
The worker releases this count only when computation finishes or an already
cancelled queued job is actually removed. A rejected full queue refunds its
brief publication reservation immediately. The comparison runner must observe
this count, rather than treating an empty HTTP handler count as completed CPU
work before freezing the service.

The same metrics request reads cumulative standard `runtime/metrics` GC-cycle,
heap allocation/free bytes and objects, live heap, heap objects, and GC CPU
counters under `runtime`. Unsupported names are listed in `runtime_unavailable`.
These describe the Go runtime heap and GC, while the cgroup remains the whole
service memory measurement. No business-path allocation instrumentation,
`gctrace`, or explicit GC call is added.

PostgreSQL uses locked pgx 5.7.6 / `pgxpool`, a total pool size of 16 by default,
and `QueryExecModeExec` extended-protocol parameters without prepared statement
caching. There is no application retry or replay. `database/sql` is deliberately
not used because its `ErrBadConn` retry behavior would obscure that distinction.

Run `go-server --batch` from the directory containing `batch.json` to execute the
same numeric algorithm independently of HTTP. Input/startup/output are outside
the internal `compute_ms` interval; the runner records whole-process time too.
Functional smoke timings are not performance results.

`repeat=0` is supported for separate fresh-process startup observations. Its
empty-array result and startup time are never subtracted from `repeat=32768` runs.

`BENCH_PG_MODE=reset-equivalent` selects a separately named same-lease session
reset comparison. The unset/normal path keeps the behavior and deadlines above.
See [the two PG comparison contracts](../pg-comparison.md) for source identities,
absolute deadlines, cancellation/eviction rules and the private-fixture tests.
