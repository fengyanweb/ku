# Node.js comparison service

Pinned runtime: **Node.js v24.18.0 LTS** (the existing supported runtime; the
formal Linux environment must install the same version). Normal V8/GC remain
enabled. `pg` is exactly 8.23.1 and every transitive dependency/integrity is fixed
in `package-lock.json`. No HTTP framework or automatic database retry is used.

```sh
npm ci --ignore-scripts --no-audit --no-fund
node --check server.cjs
BENCH_ADDR=127.0.0.1:8080 BENCH_WORKERS=4 BENCH_CPU_WORKERS=4 \
  BENCH_DB_URL=postgres://... BENCH_PEER_PORT=9000 node server.cjs
node server.cjs --batch ../batch.json
```

The public protocol, corpora and resource/SLO freeze are in `../protocol.json`.
The service also accepts `BENCH_MAX_ACTIVE` (1024), `BENCH_MAX_PENDING` (128),
`BENCH_MAX_CONNECTIONS` (4096) and `BENCH_DB_POOL` (16). These are **whole-service
totals**, split by quotient/remainder across cluster workers; the primary owns
no database connections and does no business work. All primary/worker processes,
CPU worker threads, libuv/V8/GC threads and IPC costs belong to the service cgroup.
For one worker this is a single HTTP process, without a redundant cluster primary.

Each HTTP worker starts one persistent `worker_thread` for CPU jobs; a finite
local queue is bounded by its share of pending requests. A queued request can be
cancelled; a CPU job already running retains its slot until bounded computation
finishes. Timer/TCP/PG operations use asynchronous APIs. Admission is per-worker
with whole-service bounded totals: cluster distribution can strand capacity in
one worker, unlike a shared global queue, and this cost must remain in results.

The 45s request deadline covers admission and execution, while ordinary benchmark
clients retain their frozen 3s absolute deadline. Long waits are 35s and have the
separate 50s client deadline. Keep-alive idle is 1s, body is bounded to 64KiB,
headers to 8KiB; connection-limit rejection follows `node:http` behavior and may
close transport instead of returning 503. Do not relabel such closes as success.
JSON always parses the input and constructs a fresh envelope, and CPU results
are produced by the shared numerical algorithm, not a cached response.

`/metrics` reports actual active/timer/TCP/CPU/DB counts. In cluster mode only a
metrics request asks all workers for one snapshot via IPC; there is no periodic
per-request IPC or hot-path logging. Management snapshots have bounded deadlines.
Snapshots include V8 heap/external memory and standard cumulative GC counts and
duration for every HTTP/CPU isolate plus the cluster primary. RSS is recorded
once per process and explicitly remains a sum that may duplicate shared pages;
V8 heap limits are not resident memory. Normal GC stays enabled. `actual_cpu_jobs`
counts the actual queue plus running CPU job until completion, even if its HTTP
client disconnects. Snapshot queues have finite caps, including during overload.
All database requests are independent single parameterized read queries. The
pool is not shared across processes and its local maxima sum to 16.

`--batch` reads JSON from a named file (or stdin), performs the calculation, and
returns `{checksums:[integer],compute_ms:number}`. Input decoding and process
startup are outside the internal computation timer; the driver must also record
whole-process elapsed time and resource cost.
Repeat zero is a separate cold-start observation and is not subtracted from the
nonzero computation runs.

Official references checked 2026-10-22:

- [Node release lifecycle](https://nodejs.org/en/about/previous-releases)
- [Node 24 cluster](https://nodejs.org/docs/latest-v24.x/api/cluster.html)
- [Node 24 worker threads](https://nodejs.org/docs/latest-v24.x/api/worker_threads.html)
- [node-postgres pooling](https://node-postgres.com/features/pooling)

Build/dependency directories are rebuildable and must remain outside committed
source. `npm ci` is required after copying these locked manifests to a temporary
or cache directory; `NODE_PATH=<that-directory>/node_modules` permits running this
source without retaining `node_modules` in the checkout. Preserve these permanent
benchmark sources, manifests and compact evidence.
