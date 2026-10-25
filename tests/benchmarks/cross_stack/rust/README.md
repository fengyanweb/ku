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
the legacy CPU mode runs through `spawn_blocking`, never on an async event worker.
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

## Explicit CPU coexistence comparison modes

The unset `BENCH_CPU_MODE=legacy-blocking` path preserves the original comparison
and its 45s deadline. It is not the qualified opponent for the mixed short/long
request experiment. That experiment selects between these two implementations:

- `BENCH_CPU_MODE=tokio-coop`: CPU work runs on Tokio workers in bounded integer
  chunks, with cancellation and an absolute deadline checked before every chunk.
  `yield_now().await` separates chunks. A yield does not promise that another
  task or I/O will run next; qualification measures the observed behavior.
- `BENCH_CPU_MODE=bounded-cancel`: the same chunks/checks run in the existing
  bounded `spawn_blocking` pool. A dropped HTTP future publishes cancellation;
  the running worker retains its CPU permit and actual-job record until it
  observes cancellation or finishes and releases its resources.

Both explicit modes use one **3s absolute deadline from admission**, covering
both queue stages and execution. It is not renewed on acquiring a CPU permit.
There is no database requirement for `/ok` and `/work`; omit `BENCH_DB_URL` for
the CPU experiment. Other routes and the frozen legacy batch remain available.
No Rust dependency, global allocator or optimization setting is changed.

`BENCH_CPU_CHUNK` is 1..65536 (default 4096). `BENCH_EVENT_INTERVAL`, if set,
sets Tokio's positive event interval; unset retains the pinned runtime default.
`BENCH_WORKERS` and `BENCH_CPU_WORKERS` still set async and bounded CPU parallelism.
`BENCH_MAX_ACTIVE` and `BENCH_MAX_PENDING` bound admission and the CPU queue.
The initial exploration set is frozen before measurement: Tokio workers=2,
CPU permits=2, chunks 512/4096, event interval unset/1 (four combinations); CPU
pool async workers=2, pool workers=1/2, chunks 512/4096, event interval unset
(four combinations). Exploration and independently confirmed results are separate.
All runtime threads, pool jobs and queues remain in the service cgroup.

The common integer kernel has `iterations = work.len() * 1000`,
`seed = seed.len()`, `total = seed`, and for every `i` from zero through
`iterations - 1` evaluates `(total * 33 + i + seed) % 100000000`. Both modes keep
the same `rid:{checksum:08}` response. 512000 iterations with seed 3 returns
22944003; the RID never selects a scheduling policy.

For separate cancellation qualification, `BENCH_CPU_TRACE=1` enables a fixed
16384-event ring; ordinary scored builds leave it unset. `/metrics` drains at
most 256 events per response and reports pending/dropped events. Overflow makes
an incomplete timeline explicit. Events record RID, sequence, current iteration,
whether execution started, outcome and Linux/macOS `CLOCK_MONOTONIC` nanoseconds
(null where unavailable). Events are `queued`, `started`, `cancel_observed`,
`stopped` and `reusable`; the last records after the CPU permit is returned and
the actual-job counter decremented. Available CPU permits/actual jobs are
observational snapshots, not one globally atomic transaction. Metrics also expose
current admission/pending permits. A response deadline, connection closure, or
cancel event alone does not prove that a worker stopped or a permit was returned.

Client cancellation uses RST (`SO_LINGER {1,0}`), not a normal FIN half-close.
The common driver preassigns 10% by RID and schedules notification 250µs after
the actual send, retaining actual notification lag. The measured cancellation
interval starts at **client notification T0**, not at `cancel_observed`. Determine
whether each request was queued, running, already finished or not observed.
An HTTP future drop is the cancellation publication mechanism; real RST delivery
and drop behavior require transport qualification and are not inferred from the
unit tests. Stopping the worker within 100ms and physical resource reuse within
500ms are experiment acceptance requirements, not hard realtime guarantees.

The permanent unit tests cover the frozen kernel, cancellation before work and
ownership of a running pool worker's permit after its waiter is dropped. They do
not replace transport, queue-overload, physical-reuse or SLO qualification.
