# Rust / Tokio / Axum comparison service

Pinned toolchain: the repository's existing **Rust 1.89.0**. Its MSRV-compatible
resolver and `Cargo.lock` fix the entire graph. Direct dependencies are Axum
0.8.9, Tokio 1.54.1, serde 1.0.229, serde_json 1.0.154, deadpool-postgres 0.14.2
and tokio-postgres 0.7.18. Normal release optimization is used, without LTO or
special allocator tuning. This package is its own Cargo workspace and changes
no Ku production dependencies.

```sh
CARGO_TARGET_DIR=/absolute/cache/cross-stack-rust \
  cargo +1.89.0 build --release --locked
BENCH_ADDR=127.0.0.1:8080 BENCH_WORKERS=4 BENCH_CPU_WORKERS=4 \
  BENCH_DB_URL=postgres://... BENCH_PEER_PORT=9000 \
  /absolute/cache/cross-stack-rust/release/ku-cross-stack-rust
/absolute/cache/cross-stack-rust/release/ku-cross-stack-rust --batch ../batch.json
```

See `../protocol.json` for the frozen endpoints, corpora and SLO/resource rules.
`BENCH_WORKERS` configures Tokio async workers; `BENCH_CPU_WORKERS` separately
sets both the spawn-blocking pool maximum and CPU concurrency semaphore. Both
are 1/2/4 in the formal groups. Timer, TCP and PG operations remain asynchronous;
long CPU work runs through `spawn_blocking`, never on an async event worker.
A bounded CPU queue prevents Tokio's default unbounded blocking queue from being
used as an application backlog. Already-running CPU work retains its permit
until actual completion, including after an HTTP future is dropped.

`BENCH_MAX_ACTIVE` (1024) and `BENCH_MAX_PENDING` (128) configure shared admission
semaphores. Queue admission plus execution have one 45s service deadline; clients
use the separate frozen ordinary/long-wait absolute deadlines. Axum/Hyper does
not expose a native whole-server accepted-socket cap in this implementation;
`BENCH_MAX_CONNECTIONS` is not enforced here, and the observed connections plus
client cap must be recorded instead of claiming identical admission semantics.
Response output and idle keep-alive behavior use the normal Axum/Hyper stack.
Body decode is bounded to 64KiB. There is no compression, TLS, proxy, response
cache or automatic retry.

Database pooling uses one whole-service deadpool pool, max `BENCH_DB_POOL` (16),
1s acquisition/connection deadlines and one parameterized query per request with
a 1s query deadline. Fast recycling does not add a unique per-request health
query. `query_typed` supplies the known INT4 parameter type in the normal extended
protocol, avoiding the separate prepare/execute/close round trips of the generic
`query(&str, ...)` convenience method. No application result or statement cache
is used. The
entire service process, all async/blocking/background threads, allocator costs
and PostgreSQL driver tasks belong to the same cgroup.

`/metrics` reads bounded atomic actual-active/waiting counters; this is an
observational snapshot, not a globally atomic transaction. `--batch` emits
`{checksums:[integer],compute_ms:number}`; input/process startup are excluded from
its internal timer and still belong in the driver's outer cost measurement.

`actual_cpu_jobs` counts accepted CPU jobs until their real completion, including
when the HTTP future is cancelled. It must be zero before a process is frozen.
Repeat zero is allowed only for the separate cold-start observation; its elapsed
time is never subtracted from the computation workload.

Official references checked 2026-10-22:

- [Axum 0.8.9](https://docs.rs/axum/0.8.9/axum/)
- [Tokio worker and blocking limits](https://docs.rs/tokio/1.54.1/tokio/runtime/struct.Builder.html)
- [deadpool-postgres 0.14.2](https://docs.rs/deadpool-postgres/0.14.2/deadpool_postgres/)

Do not commit the Cargo target directory or one-time probes. The benchmark source,
locked dependency graph, configuration and compact evidence are permanent.

`BENCH_PG_MODE=reset-equivalent` selects a separately named same-lease session
reset comparison. The unset/normal path keeps the behavior and deadlines above.
See [the two PG comparison contracts](../pg-comparison.md) for source identities,
absolute deadlines, cancellation/eviction rules and the private-fixture tests.
