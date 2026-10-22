# Shared open-arrival client

This is the same client for all four services. It uses only Go's standard
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
`timer`, `wait`, `tcp`, `db`, `mixed`. `--timeout` is in seconds and is a single
absolute deadline from **scheduled arrival**, including the client queue, dial,
write and complete response. Long waiting uses its separately frozen deadline.
All corpus bytes are checked against the protocol SHA-256; responses are fully
read and checked, including nested JSON, the dynamic request ID and SQL fields.
CPU expected values are calculated before the measurement window. Request IDs
are unique within a window; the orchestrator should assign distinct numeric
`--rid-prefix` values across windows. Without it a random prefix is recorded.

One planned request has exactly one terminal category: correct, explicit
503/429 rejection, other HTTP, timeout, connection error, protocol error, wrong
body, or one of the client `not_started_*` reasons. There is no Transport,
redirect, compression, HTTP pipelining or retry; even a failed reused connection
is never replayed. A new plan can dial a new connection. Old idle connections
are retired only before a new distinct plan (`--keepalive-idle-ms`, default 500).

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
reported. Quantiles are upper edges of 10-microsecond bins, not averages of
round percentiles. p95/p99/p99.9 are null below 20/100/1,000 observations.
There is a shared hard limit of 1,000,000 distinct sparse histogram buckets.
Per-second result/latency fields are **plan cohorts**; the separately named
`correct_by_completion_second` fields are actual completion-window counts.
The report's `origin_unix_ns` associates its relative monotonic CSV clocks with
wall time; do not directly subtract unsynchronized clocks from different hosts.

CSV output is streamed through an 8,192-record nonblocking queue to gzip, with a
1-GiB uncompressed limit. Reports are capped at 16 MiB, error samples at 24, total
plans at 10 million, workers at 16,384, response headers at 32 KiB and bodies at
64 KiB. Output paths are exclusive-created to preserve prior evidence. CSV or
histogram overflow and any client plan not started make `valid_driver=false`;
such a run cannot establish service capacity. Service refusals/timeouts alone
do not make the driver invalid: they remain real failed capacity candidates.
Histogram limits/queue loss are never silently deleted from the result.

The orchestrator separately records the entire driver's CPU/memory and event
writer cost, and checks that the driver has headroom. Service cgroup cost never
includes this process. Warmup, calibration, five-round ordering, service resource
limits, database/peer processes and SLO classification belong to the common
orchestrator, not this single-window client.
