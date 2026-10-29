# Shared open-arrival client

This is the same client for all measured services. It uses only Go's standard
library. The raw TCP/parser/deadline/finite-plan pattern derives from
`tests/acceptance/capacity_client.go`, SHA-256
`4c8877f6435cee1f330eca009dd572b6f902238edd32e3b04e4f9a7d49eee857`.
The old acceptance driver is unchanged.

From this directory, with the benchmark's locked Go toolchain:

```sh
go test -race ./...
go vet ./...
go build -o /absolute/outside-repository/cross-stack-client .
/absolute/outside-repository/cross-stack-client \
  --protocol /absolute/repository/tests/benchmarks/cross_stack/protocol.json \
  --address 127.0.0.1:8080 --workload json_object \
  --duration 30 --rate 100 --concurrency 128 --queue 256 --timeout 3 \
  --rid-prefix 1 --output /absolute/new/report.json \
  --events /absolute/new/clocks.csv.gz
```

Supported workloads: `ok`, `json_object`, `json_array`, `json_escape`, `cpu`,
`timer`, `wait`, `tcp`, `db`, `mixed`, `task_mix`. `--timeout` is in seconds and is a single
absolute deadline from **scheduled arrival**, including the client queue, dial,
write and complete response. Long waiting uses its separately frozen deadline.
All corpus bytes are checked against the protocol SHA-256; responses are fully
read and checked, including nested JSON, the dynamic request ID and SQL fields.
CPU expected values are calculated before the measurement window. Request IDs
are unique within a window; the orchestrator should assign distinct numeric
`--rid-prefix` values across windows. Without it a random prefix is recorded.

## Independent short/CPU arrivals

`task_mix` merges two independent absolute plans: `--rate` supplies GET `ok`
requests/second and `--cpu-rate` supplies CPU requests/second. It uses the same
bounded queue, workers, connection pool, oracle and deadline handling. A short
plan wins an exact timestamp tie. Global RIDs remain unique; each stream has
its own zero-based `stream_index`, so changing short traffic does not change CPU
timestamps, inputs or cancellation selection. Total plans remain at most10million.

Freeze a separate protocol copy with CPU `work_units:[512]` and
`seed_lengths:[3,11]`; do not edit the public default. Set
`slo.task_mix_short_p99_ms:20` explicitly in that copy (20ms if absent). This is
the experiment's planned-send-to-complete SLO, not a shortened request timeout
or a language guarantee. The original `ok` workload keeps its original SLO.
Pass `--corpora-dir` when the copied protocol is outside the original corpus
directory; unchanged corpus hashes are still checked before measurement.

```sh
cross-stack-client --protocol /absolute/frozen-task-mix.json \
  --corpora-dir /absolute/repository/tests/benchmarks/cross_stack/corpora \
  --address 127.0.0.1:8080 --workload task_mix --duration 30 \
  --rate 100 --cpu-rate 20 --concurrency 128 --queue 256 --timeout 3 \
  --rid-prefix 1 --output /absolute/new/report.json \
  --events /absolute/new/events.csv.gz
```

Optional `--cpu-cancel-every 10 --cpu-cancel-delay-us 250` predesignates CPU
ordinals9,19,... (each complete group of10 has one reset). With alternating
seeds[3,11], these cancellation plans use seed11; normal CPU plans use both.
After the full request write, its existing worker waits until the first
positive write timestamp plus250us, then calls TCP `SetLinger(0)` and `Close`.
There is no per-request cancellation goroutine, deadline renewal or replay.
Actual issue time and scheduling delay remain observable: a late wake is not
backdated. Failed writes, reset setup/close errors and deadline expiry stay
explicit. Socket retirement decrements the existing live count exactly once.

`planned_reset` is a separate terminal outcome, never a correct response or
proof the server received the request, started/stopped computation, ran drops,
or made resources reusable. A successful local write may still contain unsent
kernel-buffer data, which a reset may discard; the server may also have
completed before reset. Qualification must correlate service RID events.
The original full ledger keeps all planned outcomes; task_mix `by_type.cpu`
adds `normal_planned`, `normal_correct`, normal failure fractions and a separate
`planned_cancellation` ledger. Predesignated cancellations are excluded only
from the normal CPU denominator. Unsent/failed cancellation plans remain in
that partition; original `not_started_*` still invalidates the driver window.
Expected resets have separate bounded samples and cannot fill `first_errors`.
`SLO_goodput_rps` counts only correct, in-window, within-SLO responses, by type.

Only task_mix CSVs append `stream_index`, `planned_cpu_cancel`, `reset_due_ns`,
`reset_issued_ns` and `request_write_completed`; original workload columns are
unchanged. These offsets use the existing Go monotonic origin and its one-time
kernel anchor/uncertainty, without per-request kernel-clock reads. All modes
also record `last_terminal_*` including unsent plans and `run_end_*` after
workers, socket retirement and EOF CSV export. Export timestamps are separate;
JSON serialization/process exit are outside `run_end_*`.
Neither timestamp means server physical completion.

One planned request has exactly one terminal category: correct, explicit
503/429 rejection, other HTTP, timeout, connection error, protocol error, wrong
body, or one of the client `not_started_*` reasons. There is no Transport,
redirect, compression, HTTP pipelining or retry; even a failed reused connection
is never replayed. A new plan can dial a new connection. All workers share a
bounded LIFO idle connection pool, so the next request reuses the most recently
returned connection even when another worker handled its previous request.
The worker limit still bounds in-flight requests; idle pool capacity equals that
same limit. A connection is exclusively checked out for one complete request,
then returned only after the entire response has been read and validated.
Old idle connections are retired before another distinct plan
(`--keepalive-idle-ms`, default 500), with no background probe or replay.
I/O, protocol, body-validation and timeout failures discard the connection;
`Connection: close` also discards it. Fully read 503/429 replies may keep their
connection. Checked-out connections receive the new plan's original absolute
deadline before writing; the pool never renews a request deadline. The pool is
closed after all workers finish, including the original-deadline completion
tail, and cleanup counters must return to zero.

This shared-pool policy remains in client schema `ku-cross-stack-client-v5`. Earlier v2
results retain their per-worker connection policy and are diagnostic evidence,
not interchangeable main-table results. Actual `resources.dials` and sent counts
must still be reported: keep-alive support alone does not prove connection reuse.

`attempted` means the worker began the operation. `sent` means at least one
request byte was written; `sent_at` is the timestamp before that first positive
write. Dial failure is attempted but unsent. Reports give both planned and sent
failure denominators, and CSV retains attempt start separately. Completion
includes complete-body validation on the driver. Errors and refusals stay in
the denominator. A fully read response beyond the request deadline has a late
event and one terminal outcome; an actual wrong body remains a body error.
An in-deadline success after the measurement window belongs to its original
plan ledger but contributes no in-window goodput.

`ledger` and `by_type` contain counters, goodput and latency. Primary quantiles
are `correct_scheduled_to_complete`; sent latency and scheduling lag are also
reported. Quantiles use conservative upper edges: 256 linear10us cells below
2.56ms, then128 cells per doubling (relative width at most1/128). Every int64
nanosecond duration has a cell. Each nonempty distribution allocates5760 uint64
cells; at the maximum3600-second window the cells remain bounded below170MiB,
independent of requests. This replaces v4's shared1,000,000 sparse-key bound,
which could lose samples and invalidated high-pressure Node observations.
p95/p99/p99.9 remain null below20/100/1,000 observations. A core pooled p99
conclusion additionally requires>=1,000 correct observations of each type and
>=7 interleaved windows. Counts are never dropped to satisfy these requirements.
The orchestrator rejects merging different driver/protocol/load/build identities.
It bootstraps complete paired windows with frozen seed20261023, not requests.
The95% interval estimates the geometric mean per-window ratio and retains all
ratios; independence/stationarity assumptions and failed gates remain explicit.
Per-second result/latency fields are **plan cohorts**; the separately named
`correct_by_completion_second` fields are actual completion-window counts.
On Linux, v5 retains v4 timing: it brackets one standard `clock_gettime(CLOCK_MONOTONIC)` syscall
between two `time.Now` calls at arrival origin. The later Go timestamp is the
origin; its kernel-monotonic estimate is the kernel read plus half the bracket's
Go-monotonic duration. `configuration.origin_monotonic_ns` records that estimate;
`origin_monotonic_uncertainty_ns` records the full bracket as a conservative error
bound. `origin_monotonic_status` is `available` or `unavailable`, with an explicit
`origin_monotonic_reason` when unavailable. No further kernel calls are added to
the request path. Planned cutoff, actual send/completion, and complete-response
`*_monotonic_ns` fields add their existing Go-monotonic offsets to this anchor.
These fields and CSV offsets share the Linux sampler's CLOCK_MONOTONIC domain;
cross-process association must check availability and the frozen uncertainty
gate. That gate is separate from request SLOs and `valid_driver`.
All `*_unix_ns` fields remain raw human-readable wall timestamps, which may jump;
they must never determine recovery or resource-window association. Other systems
emit null anchor/derived values with an explicit unsupported-platform reason.
macOS smoke results do not establish Linux cross-process timing accuracy.

CSV recording reserves a checked 128-byte slot per planned request plus a seen
bitmap before arrivals, with a 256-MiB backing limit. Request completion stores
one indexed record; it cannot overflow a streaming CSV queue. After requests
and socket retirement, all records are exported in plan-index order using the
existing formatter, gzip.BestSpeed and 1-GiB uncompressed limit. The original
15/21 fields and their timestamps are preserved. The 900-second frozen stage
requires about 236.64 MiB of backing; this is driver memory, not service memory.
Unrepresentable counts, labels or worst-case output sizes fail before arrivals.
Reports are capped at 16 MiB, error samples at 24, total
plans at 10 million, workers at 16,384, response headers at 32 KiB and bodies at
64 KiB. Output paths are exclusive-created to preserve prior evidence. Duplicate,
missing or rejected records, output errors, or any client plan not started make `valid_driver=false`;
such a run cannot establish service capacity. Service refusals/timeouts alone
do not make the driver invalid: they remain real failed capacity candidates.
Failed records are never silently deleted from the result. The arrival CPU gate
is retained, with an additional whole-driver-lifecycle gate covering startup,
CSV/gzip and JSON through process exit. Its denominator remains the original
arrival duration and the same 3CPU budget. Export CPU is a subset of this total,
not an extra cost to add again. These changes do not establish a service speedup.

The orchestrator separately records the entire driver's CPU/memory and event
writer cost, and checks that the driver has headroom. Service cgroup cost never
includes this process. Warmup, calibration, seven-round seeded ordering, service resource
limits, database/peer processes and SLO classification belong to the common
orchestrator, not this single-window client.


V5 also reports whole-ledger/by-type HTTP status and transport-stage counts,
scheduled-to-attempt and dial duration, cold/reused connection latency, idle
expirations and actual requests/dial. The500ms default is unchanged. The separate
`linux_harness.py --phase diagnose --case c4-m1073741824-ok --stacks control
--diagnostic-rates LOW MID HIGH` checks exactly three predeclared points under
500ms and disabled idle expiry. `client/control` is an independent dynamic
net/http `/ok` service for generator validation; it is never a language ranking.
All requests include `X-Request-ID` equal to the frozen CSV rid for correlation.
Internal Ku error reasons are read only through the protected management metrics.

Resource costs remain separate: arrival-window CPU estimate with bracket error,
whole-driver lifetime, and `service_cost.full_lifecycle` after observable runtime
quiescence or completed termination. The fixture5s result is retained separately;
`--physical-settle-seconds` defaults45s of additional observation after the driver,
without changing runtime request/handler deadlines. `post_driver_cpu_seconds` includes remaining
failed work. Forced termination fails natural reclamation; it cannot be a cleanup
pass. Post-driver CPU is not automatically cancellable useless work: that field
is null until a responsibility-level attribution proves it. Memory reports middle
60% median and observed peak of whole-cgroup current memory, not cumulative RSS
or an unreset cgroup lifetime peak. cgroup throttling and live-thread scheduler
snapshots are diagnostic context; low average driver CPU alone proves no absence
of burst bottlenecks.

Overload uses common absolute pressure and1.5x/2x/5x verified-point rates. If5x
still does not reach an observable boundary, bounded10x/20x/40x escalation is
permitted up to the frozen rate ceiling. Driver-invalid windows, unknown500s and
wrong responses never establish genuine service overload. Only valid boundary
observations plus the original planned-cutoff recovery gate can pass that claim.

`formal-summary.json` keeps `paired_common_costs` separate from
`paired_capacity_goodput`: the latter pairs seven qualified own offered-rate
windows for Ku/Go and Ku/Rust. Equal common arrival rates cannot establish a
maximum-throughput ratio. Unqualified points and censored upper exploration
boundaries remain ineligible for capacity claims.

Both `c4-m1073741824-mixed` and `c4-m268435456-mixed` are declared cases.
`--phase sustained --case c4-m268435456-mixed --stacks ku go rust node java dotnet cpp`
uses that exact case's seven-round verified rates, derives60% of their minimum,
and runs at least600seconds; it cannot borrow a1GiB capacity result. Added Java,
.NET and C++ cases are limited to four-core`ok`, `json_object` and`mixed`.
Dedicated waiting services read target+64 active and target+128 connection limits
from the same ordinary config as other stacks. Their receipts verify Ku's real
protected `limits`; neither an environment variable nor Task driver capacity is
proof that the HTTP admission limit changed.

## Constant HTTP diagnostic only

`--path-override /constant --response-literal 'h4:constant'` is an explicit,
paired diagnostic oracle. It accepts one existing workload, never `mixed` or
`task_mix`. The origin path is at most8KiB, must be URI-escaped, and cannot
contain a query or fragment; the original unique RID and workload query values
are still appended, with both request-ID headers retained. The response literal
is at most64KiB; an explicitly selected empty string validates only an empty body.
The normal method/body, HTTP200 requirement, complete-body read, connection pool,
absolute deadlines, accounting, rejection classification and no-retry policy stay
in effect. The full response must equal the literal; a matching prefix fails.

```sh
cross-stack-client --protocol /absolute/frozen/protocol.json \
  --corpora-dir /absolute/repository/tests/benchmarks/cross_stack/corpora \
  --address 127.0.0.1:8080 --workload ok \
  --path-override /constant --response-literal 'h4:constant' \
  --duration 10 --rate 500 --concurrency 32 --queue 128 --timeout 3 \
  --rid-prefix 1 --output /absolute/new/constant-diagnostic.json
```

The report adds `diagnostic_only:true`, `normal_score_eligible:false`, the exact
path and response length/SHA-256. It does not validate an echoed RID in the fixed
body. These windows belong only to the separately identified H4 constant-route
comparison, never the original `/ok` or real-business main tables. Services do
not register `/constant` by default: create isolated benchmark-source copies,
retain the normal admission/deadline/cancellation/resource paths, and record
expanded-source/generated-C/ELF identities. A generated-C registry-off control
must be labelled a diagnostic artifact, not a normal product build or release.
Without both flags, request generation, response validation and report fields
retain their original behavior.

固定响应入口仅用于 H4 常量路由的独立诊断。必须成对传入路径与完整预期正文，
混合负载禁止使用；响应多一个字节也算错误。报告会标注不能进入正式业务主表，
不能把常量路由结果冒充原 `/ok` 或真实业务性能。
