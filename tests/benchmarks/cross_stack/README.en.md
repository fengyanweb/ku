# Cross-language business and resource comparison

This directory contains the permanent Ku, Go, Rust, Node.js, Java, .NET and C++
benchmark sources and their shared client. It is not a performance conclusion.
See `docs/multicore-capacity-acceptance.md` and the corresponding evidence JSON
for results; the Chinese guide is [`README.md`](README.md). Each run must identify
its candidate commit, sources, CLI, actual service binaries and protocol hashes.
A version or documentation update does not change historical run identities or
prove that a feature was included in a public installation bundle.

## Frozen business workload

`protocol.json` freezes corpus hashes, algorithms, database tables, SLOs, resource
profiles and measurement windows. The seven services use Ku native C, Go net/http,
Rust Tokio/Axum, Node node:http, Java Vert.x, .NET Kestrel and C++ Drogon. Node uses
cluster for multiple cores, with long computations on its normal bounded workers.
Each stack's locks and build receipts identify dependencies and toolchains.

The original Ku/Go/Rust/Node matrix retains its declared 1/2/4-core cases. Formal
HTTP cases for Java/.NET/C++ are four-core `ok`, `json_object` and `mixed`; the
256 MiB profile includes `json_object` and `mixed`. These three stacks do not
automatically inherit the original TCP-waiting matrix. Batch execution is checked
separately against the common computation kernel.

JSON must be fully decoded, placed in a fresh envelope and freshly encoded. The
current Ku source explicitly uses `decoded.take("data")?` to consume the root and
transfer the subtree; the old indexing API returns an owned copy. Bind old and new
results to their actual source identities rather than relabeling indexing results
as `take`. PostgreSQL 16 uses a private 1,024-row fixture and a total pool of 16,
with no automatic business-query retry. Ku keeps same-lease `DISCARD ALL`;
Java/.NET/C++ retain their actual pool-reset behavior without an artificial matching
statement. Identical SQL and golden values do not imply identical reset costs.

Ku `/json` uses text/plain with `--ku-json text`; `response` selects the public
`http.json` API and application/json. Ku `/db` remains text/plain in both execution
modes. Go/Java/.NET/C++ JSON responses include a UTF-8 charset; Rust/Node use
application/json without an explicit charset. Golden body rules match, while
actual response-header differences remain disclosed.

The shared `client/` validates HTTP and computation results. It generates planned
arrivals without retrying failed requests. Ordinary absolute deadlines start at
the planned arrival and include client queueing and full response validation.
At the window end no new plans are created; existing plans may still start and
finish within their original deadlines. Tail completions do not count toward
arrival-window goodput. Client v1 dropped some such plans early; its preparation
results remain invalid.

Client v2 gave each of 256 workers a connection. At low rates, rotation exceeded
the 500 ms idle deadline and produced reconnect traffic; those diagnostic results
must not enter the persistent-connection main table. V3 kept concurrency, queue,
deadlines and idle expiry unchanged, using a shared bounded LIFO pool with one
in-flight request per connection. Failed or closed connections are not reused;
requests are not replayed. Report actual dials and requests per connection rather
than assuming reuse from a keep-alive header.

V3 preparation observed guest realtime moving backwards. V4 therefore captures
Linux CLOCK_MONOTONIC anchors and uncertainty at the arrival origin. Resources and
recovery use that clock; Unix timestamps are for reading only. Missing anchors or
uncertainty above 1 ms make measurement indeterminate without changing service
SLOs. The primary five-second recovery boundary conservatively includes both
anchor errors; clock changes or the last actual send do not renew it.

Terminal events are retained in bounded gzip CSV, with planned, sent, correct,
rejected, timed-out and unsent counts. Current v5 also records HTTP status,
transport stage, send delay, dials, and cold/reused connection distributions.
Insufficient tail samples remain insufficient. Formal comparisons require at
least seven windows; merge distributions with matching identities/configurations,
never average per-window p99s. See [`client/README.md`](client/README.md) for the
complete generator and cost gates. Driver-invalid or environmentally unresolved
windows cannot establish service capacity or a language ranking.

## Isolated Linux environment

The reference guest is ARM64 Ubuntu 24.04 with 8 vCPUs and 4 GiB. Virtual CPUs do
not imply exclusive physical cores. Formal comparisons run in the same Linux
environment; macOS golden-response checks are not ranking evidence.

The current CLI supports native linking on Linux ARM64. `build_linux.py` uses its
actual `--backend c --profile release --emit-c` output and retains generated C,
ELF and dynamic-dependency identities. It no longer accepts a historical refused
link as success or separately recompiles generated C. Old explicit Clang-link
results retain their original build identities. This benchmark capability does
not add an official full SDK archive for Linux ARM64.

Ku resolves its filesystem base using the program path and compile-time locator.
Keep the independent `bin/` and `ku-native/` layout with its inputs; do not treat
cwd as that base or edit generated C to bypass language behavior.

- Whole service tree: CPUs 0 through cores-1, 1/2/4 cores, 1 GiB main or 256 MiB
  small profile, swap disabled.
- Ordinary Go client tree: CPUs 4–6, three cores and 1 GiB per domain. Overload
  traffic and its independent normal-request probe use two domains; their combined
  allowance is not 1 GiB.
- Private database and children: CPU 7, one core and 512 MiB.
- Observe the root coordinator separately; service limits do not cover the whole
  guest. Waiting-group Python selectors, controlled TCP peers and observer threads
  use CPUs 4–6 without a separate 1 GiB hard memory cap. Do not conflate these costs
  with the ordinary Go client domain.

All measured services enter cgroup v2 before execution. Cluster parents, workers,
GC and background threads inherit the whole-service domain. Observe cgroup CPU,
peak/current memory, anon/file pages, OOM, PSS, FDs, threads and real Task metrics.
Summed RSS may count shared pages repeatedly and is not exclusive physical memory.
Admission mechanisms differ across frameworks; record them without adding an
artificial layer solely to slow a counterpart.

```sh
# Use a disposable Linux guest, never a production host or real database.
sudo env BENCH_ROOT=/opt/ku-bench BENCH_USER=mac bash tests/benchmarks/cross_stack/provision-linux.sh
# Copy the exact candidate into /opt/ku-bench/src without caches or node_modules.
python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/build_linux.py --root /opt/ku-bench \
  --candidate "$(git -C /opt/ku-bench/src rev-parse HEAD)" \
  --ku-format helpers --ku-json text --ku-pg-execution async --ku-pg-read int \
  --only ku go rust node java dotnet cpp client
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/database.py --root /opt/ku-bench start
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/linux_harness.py \
  --root /opt/ku-bench --output /opt/ku-bench/results/golden --phase smoke \
  --stacks ku go rust node java dotnet cpp
# Explicitly select an authorized formal case; this runs the protocol's formal rounds.
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/linux_harness.py \
  --root /opt/ku-bench --output /opt/ku-bench/results/formal-json --phase formal \
  --case c4-m1073741824-json_object --stacks ku go rust node java dotnet cpp
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/database.py --root /opt/ku-bench stop
```

The build helper defaults to all seven stacks plus the shared client. The harness
still defaults to the original four: pass `--stacks` explicitly for seven. The
formal command is a usage example, not required work for each documentation or
version update. One fixed-load round validates that window only; it is not a
seven-round formal comparison, a complete matrix or long-term acceptance.

The fixture builder uses uid 501/gid 1000; pass matching `--uid`/`--gid` explicitly
elsewhere. It must run without root, while database/cgroup orchestration needs
root. Dependencies and generated files stay in ROOT/build and ROOT/tools, with
auditable source identities. Provisioning defaults are the environment variables
at the script's beginning. Save the environment, manifest, source inventory and
binary hashes before measurement. A changed frozen input requires a new result
directory; do not merge rounds from different candidates or configurations.

## Ku build modes

Defaults are `--ku-format helpers --ku-json text --ku-pg-execution async --ku-pg-read int`.
`helpers` retains the template's formatting calls. `direct` uses public `str(int)`
with the same explicit eight-digit zero padding and has a separate identity.
`text` uses `json.encode` plus `http.text`; `response` uses public `http.json`.
Record expanded source, generated C, ELF, MIME and complete call costs separately.
Do not present direct/response results as default helpers/text or relabel old
results. Keep cancellation, deadlines, budgets, ordinary value ownership and
cleanup checks enabled; do not restore withdrawn experiments.

## PostgreSQL execution comparison

The default source uses a named local `async fn database` capturing the original
initialized pool. Public `query()` suspends, `value_int()`/`value()` read an
independent result, and existing `json.encode` plus `http.text` produce the output.
The `mixed` workload calls the same `/db` route without another client or a
benchmark-only database API. SQL, the total pool of 16, DISCARD ALL, original
absolute deadlines and no-query-retry rules remain unchanged. Both PG paths use
text MIME, not application/json.

`--ku-pg-execution sync --ku-pg-read value` selects the old bounded HTTP-thread
path with string-to-JSON numeric reads; `sync` plus `int` isolates the getter.
Defaults are `async` plus `int`. Async frames cannot hold dynamic KuValue, so
`async` plus `value` is rejected. Use isolated benchmark roots for the three
combinations. Manifests record execution/getter modes, original and expanded
sources, generated C and actual ELF hashes. Never relabel an old baseline async.

## Phases and cleanup

The harness supports `smoke`, `diagnose`, `explore`, `formal`, `overload`,
`sustained`, `waiting`, `batch` and `all`. `--case c4-m1073741824-json_object`
selects one exact matrix entry; other defaults follow the frozen protocol.
Waiting cases observe real timer/TCP suspension and completion, not QPS as a
substitute for held concurrency. Batch cases separately measure fresh-process
startup and fixed computation; startup is not subtracted from computation results.

Formal L1 uses `iterations=512000, seed=3, repeat=32768` for all seven stacks and
checks every checksum. Historical `repeat=64` results remain R&D, outside the
formal table. Each process group must run for at least 30 seconds of wall time and
every process must report internal `compute_ms >= 30000`. Retain a short window
and stop later formal rounds for that stack. Do not add sleep, automatically
increase work or select favorable reruns. `repeat=0` measures startup only and has
no 30-second minimum. From common release to last natural exit, the group has a
600-second absolute cap. Each stdout/stderr is capped at 1 MiB, with at most 6,016
100 ms resource snapshots spanning the window. Sampling may miss momentary
RSS/current-memory peaks; OS `memory.peak` remains separately recorded. Correct
results, natural exits, OOM checks and resource-domain/temp-directory cleanup
requirements remain. Complete golden validation first and retain reasons for
failed preparation or invalid windows.

Shared-client regression commands are `go test ./...` and `go test -race ./...`.
Cross-process clock/recovery counterexamples use
`python3 -B -m unittest discover -s tests/benchmarks/cross_stack -p test_linux_harness.py`.
`test_compile_variants.py` uses small synthetic results for batch-duration,
output-cap and stop rules; it does not execute 32,768 computations or establish
performance conclusions.

Keep permanent sources, locks, compact raw results and necessary failure evidence.
Remove one-off probes, duplicate packages, temporary builds, `node_modules` and
`__pycache__` after use. Cleanup touches only this run's private directories,
processes and cgroups. The benchmark does not deploy the website, implicitly rerun
the old 36 memory checks, or resume the paused generic-value copy/release and AST
analyses. Website edits and packaging require separate scope; they do not replace
runtime evidence.
