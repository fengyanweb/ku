# Ku 0.0.19 remediation results — 2026-10-29

Candidate implementation, bounded diagnosis and relevant regressions are complete. **Final performance acceptance is not complete; no Rust parity or resident-memory reduction is claimed.** Candidates remain on `codex/v0.0.19-json-pg-memory`. Main, published tags/defaults and the website were not changed.

Confirmed: request failures previously could fall behind driver-invalid classification. Functional validity, accounting, arrival quality, environment quality and candidate adoption are now independent. Late arrivals alone do not establish an external measurement fault.

Supported, not an endpoint win: identical 1048 B input, 16-byte RID, owned request and actual generated encode handler measured 5.188 µs at A,4.538 µs at independent J1 and4.362 µs at J1/J2/J3, in unsampled one-CPU-second mechanism windows. These are not HTTP scores and are not subtracted from HTTP cost.

Confirmed within scope: PG 20, JSON 21 and corresponding ASan/UBSan checks passed. A real async HTTP→Task→PG lifecycle covered 10000-row results, deadlines, disconnects and an uncertain committed write without replay. A/B each completed three 512-request TCP high-water/release cycles, 1536 exact responses, budget retirement and natural exit.

Not accepted: structural fixed charges fell 458752 B; the new fd index adds 131088 B of independent reserved charges, for a combined 327664 B reduction. Final anon was equal; B RSS/PSS were 4 KiB higher and observed peak 430080 B higher. This does not establish an RSS benefit.

Formal comparison is blocked: three 30s A controls each returned 15000 correct results, but CPU max/min 1.105059 exceeded 1.10 and one scheduled→attempt p99.9 was 12.4 ms against 10 ms. The frozen five-group A/B/R matrix was stopped. A separate same-ELF HTTP diagnostic measured 106.844→136.571→165.096 µs/correct; the final arrival window failed. Neither stable ranking nor profiler-only overhead attribution is justified.

| Work | Implementation | Correctness | Own A/B, diagnostic only | B/R | Memory/tails | Decision |
| --- | --- | --- | --- | --- | --- | --- |
| JSON full request | 7ba3064,7aec348,536242d,f6f0bdd |21 plus sanitizers; exact HTTP bodies|136.590→112.706 µs,3s windows|Formal run blocked|Arrival drift retained|Blocked; not adopted|
| PG defaults |00cfa41,efefff5,bd9d644,9ab85b1|20 plus real lifecycle|304.462→231.438 µs,600/window|Rust Fast fixed,not scored|p99.9 unavailable at this size|Blocked; not adopted|
| Strict reset |Same Ku,original DISCARD ALL|Lifecycle and retirement passed|Same Ku contract,no duplicate score|Strict Rust not formally run|No unified tail interval|Blocked|
| /ok and /work |Combined B|All bodies correct,natural exits|24.009→26.381 and2791.963→2979.192 µs|No formal comparison|Points exceed3%; stable regression unknown|Guard not accepted|
| Fixed layout |1e8fc46 plus other B changes|Three high-water cycles|Combined charges−327664B|No language memory ranking|Anon unchanged,RSS not lower|Resident target not met|
| Failure protections |Relevant permanent regressions|fd reuse,cancel,close,OOM,budget/ownership|Not applicable|Not applicable|Bounded evidence retained|Targeted checks complete|

## Implementation and semantics

J1 inlines width-only UTF8 validation. J2 scans only complete readable 8-byte ordinary-ASCII blocks, preserving control/quote/backslash/non-ASCII fallback and error precedence. J3 separates reserve slow paths from 1/2/6-byte writes. Output, depth, ownership and failure rules remain. No request-level JSON allocation-count reduction was demonstrated; no unbounded cache was added.

P1 adds an exact owner-local fd index with generation checks, failed kernel ADD rollback and correct old-alias retirement. Probing remains capacity-bounded, not an unconditional O(1) promise. Its actual two-owner charge is 131088 B.

P2 combines a small PG result's header/cells/bytes: three allocation requests totalling 440 B become one 448 B allocation. Inline tails remain charged while external growth is alive. P3 continues only a compact first row that fits the remaining 64 KiB/64-cell/32-protocol turn. At the removed yield it observes job cancellation, control phase and the original absolute deadline. Bare API and external growth retain their yield. The new short lock/atomic/clock costs still need qualified endpoint evaluation.

P4 retains the first failure, distinct semantic states and latest failure within a fixed cache, with drop/overwrite counters and no synchronous hot-path printing. The real fixture captured three cold slots with enqueue≤start≤finish; warm or not-submitted worker states remain missing/zero. The89 failures from the retired bitmap experiment remain unexplained; they do not establish a defect in the old default. Neither that bitmap nor the withdrawn single-notify experiment was restored.

M replaces nine KuValue config slots with three owned strings,five integers and boolean/presence fields. Actual NET+PG job size is 744→520B with FILES/DIRECTORY disabled;496B in the NET-only fixture is a different feature set. Fixed and reserved ledgers are disjoint and must be added. FD 1038→14, 21 threads and task/job/output retirement were observed each cycle. Both versions' final cycle grew 4 KiB resident; three short cycles neither establish a leak nor exclude a long-lived leak.

## HTTP and dynamic evidence

Sync /json does not execute the /work computation Task state machine. Whole-service sampling produced 121 running SELF samples,lost0: owner 76 and sync worker 45. Sample-window cgroup user/system CPU was 266.313/416.542 ms. schedstat execution was 470.244 ms in owners and 212.288 ms in sync workers; overlapping methods are not summed.

Top SELF __wake_up_sync_key accounted for 13/121 samples: 8 chains came from TCP loopback response writes and 5 from worker→owner pipe notification. These are not all internal wakeups, nor futex waiting wall time. The external observer used 0.325s CPU and 25399296 B peak RSS,separate from service CPU; external accounting does not exclude scheduling interference. Ku PCs have generated-C/actual-ELF maps; kernel symbols/offsets do not establish lock contention. No valid PMU counters were obtained; no IPC/cache-miss result is supplied. Idle CPU was only 4.5–4.8 ms over 10–11.4s and yielded zero CPU samples. There is no demonstrated idle busy loop or evidence sufficient to merge queues/remove handoffs. The next discriminating locations are write_limited/sendmsg versus wake_notify/pipe, while preserving cancellation/deadlines/cleanup.

Dense profiles contain 3988/3996 user CPU samples,lost0, recorded raw chains and PC→C/disassembly maps. A's independent UTF8 scalar SELF is 742/3988. J123 retains 546/3996 samples in the inlined width-validation source region, already included in parse/write attribution. Symbol disappearance does not mean validation disappeared. Sampled 8 CPU-second costs 5.983/4.540 µs are separate from unsampled 1 CPU-second points; profiler and duration effects are not conflated.

## Environment and remaining boundaries

The frozen ARM64 Linux VM contract is 4vCPU/3 GiB: service 2cores/1 GiB,driver CPU 2 one core/1 GiB,private DB CPU 3 one core/512 MiB; two HTTP owners, B17, 4096 connections, 1024 active, 128 pending, Task2048,PG pool 16. Services/languages run serially and their predecessors exit before successors. Quotas are not physical reservations; host and guest memory are not added twice.

Host samples confirm active swap-in/out and concurrent lint node activity around 191–194% CPU/496–499 MiB RSS. RSS includes shared pages. The project identity and Ku's own swapped pages are unknown; this does not explain every CPU fluctuation. Frozen qualification thresholds were not relaxed. Unknown cause is retained as unknown rather than automatically measurement_invalid.

The session entered at7aec348 with work-in-progress J1/J2 candidates. Comparison A is the last usable default988f32e, excluding unaccepted candidates; entry HEAD and control A are distinct. A already contains first64; it is not a new gain. Ordinary builds remain CLI Release/C11O2/helpers/text/async/int. The old JSON engine, deadlines and cancellation rules remain; yyjson/A3/A4 were not restored. Paused generic Value copy/drop and AST work, M:N,seven-language and 36-memory matrices were untouched. Rust reference ELF SHA is66b1d69287ca7356951a86e32db696524e2049abf24c6437d1ab580fdbc443e5. Default Fast and explicit reset-equivalent remain separate; no B/R ratio is supplied without valid windows.

## Checks, CI and cleanup

Local format,all-target clippy and 53 existing Python checks passed. PG 20,JSON 21,reactor1 and net-owner1 passed their related ASan/UBSan checks; local macOS does not establish Linux LSan equivalence. First CI6bdfb05 recorded 18 passed,one failed,three conditional skips. Its old metrics fixture slept 172 ms instead of 40 ms, exhausting the original 100 ms deadline before connecting. The product correctly rejected it. Commit67ece0c extracts the unchanged reader for an already-established response fixture; it does not increase any timeout. All 11 supervision tests and local lint passed. See ci-final.json/final CI receipt for the final run; the original failure is retained.

No schema,migration or dependency changes. Owned private DB/cgroups exited normally. VM returned to its original 8vCPU/4 GiB Stopped state. Temporary probes/source copies/builds were deleted; necessary raw samples,symbol-bearing ELF,generated C and bounded failure evidence are retained. Primary-source check found 442 unchanged files,the pre-existing README difference and zero missing files. The user's dirty multicore document SHA is unchanged.

## Reproduction and portable evidence

Permanent minimal entries live in tests/benchmarks/cross_stack: focus_cycle.py,json_dense.py/.c.inc,http_cost_diagnostic.py/http_thread_roles.c.inc,host_context.py. They reuse V5,waiting and existing PG lifecycle code. This task explicitly requested these reproducible entries; other one-off probes were deleted.

Use ordinary `ku build --backend c --profile release --emit-c -o OUTPUT server.ku`, frozen expanded fixture and KU_PG_LIB. Identities and exact build output are in raw/candidate-identities.json and per-version normal-build.log in the evidence ZIP.

For dense diagnosis run `python3 json_dense.py --generated-c NORMAL_C --output NEW_DIR --source-commit FULL_SHA --cli-sha256 CLI_SHA --link LIBPQ --debug-symbols --family http-object --context http --cpu-seconds 1`. Omitting timing arguments performs validation only. Other character families and inactive context remain separate.

For control run `sudo python3 focus_cycle.py --plan PRIVATE_PLAN --out NEW_DIR --phase control --workload json_object --names A`. Keep the frozen plan and failures; do not start final until qualification passes. Real PG uses `python3 tests/acceptance/http_pg_lifecycle.py --ku CLI --lane async --profile release --loops 4 --output NEW_JSON` against its own fresh local fixture.

High-water observation reused waiting.NetworkPeer/Cohort:hold 512 real /tcp jobs,observe net_wait,release K,verify all replies,wait for quiet and original budget,repeat 3 times before stopping; only then start B. Raw stage values are raw/layout-high-water-v2/*/observation.json.

Raw chains,PC maps,disassembly and SELF are in raw/dense-layout-{A,J123}/perf-* and raw/diag-thread-A-20261029/json-*; DWARF stack capture is capped at8KiB; an intact file does not guarantee arbitrary-depth unwinding. Inclusive chains overlap and are never added. analysis-results.json summarizes data; raw/evidence-files.json supplies per-file SHA. Paths refer to the extracted portable ZIP,not a required original machine layout.

Method references: [Google Benchmark variance](https://google.github.io/benchmark/reducing_variance.html),[LLVM benchmarking](https://llvm.org/docs/Benchmarking.html). These do not prove a Ku-specific cause. [PostgreSQL16 async API](https://www.postgresql.org/docs/16/libpq-async.html) supplies protocol context; local lifecycle records establish observed behavior.

## Dense running SELF Top10

A denominator3988; J1233996. Raw leaf attribution only; unlisted remainder retained.

| Rank | A symbol/samples | J123 symbol/samples |
| --- | --- | --- |
| 1 | ku_json_utf8_scalar 742/3988 | ku_json_write_string 732/3996 |
| 2 | ku_json_write_string 584/3988 | ku_json_parse_string 651/3996 |
| 3 | ku_json_parse_string 290/3988 | ku_json_parse_value 227/3996 |
| 4 | __memcpy_generic 248/3988 | ku_json_write_value 218/3996 |
| 5 | ku_json_buffer_append 214/3988 | __memcpy_generic 184/3996 |
| 6 | ku_json_parse_value 196/3988 | _int_malloc 166/3996 |
| 7 | _int_malloc 168/3988 | ku_json_buffer_prepare 156/3996 |
| 8 | ku_json_write_value 155/3988 | __GI___libc_malloc 115/3996 |
| 9 | __GI___libc_free 110/3988 | __GI___libc_free 110/3996 |
| 10 | encode 96/3988 | _int_free 106/3996 |

Final-code CI: [67ece0c run](https://github.com/fengyanweb/ku/actions/runs/40910009989). Measured B was built at6c81803; C backend,ABI and Cargo inputs are unchanged through67ece0c. The extracted private metrics reader is outside the generated-C service path. Final CI status is listed separately.

## CPU boundary and retirement cost

Main CPU uses whole-service cgroup cumulative CPU endpoints over the entire driver lifetime divided by all correct results,not the1Hz arrival-window interpolation. DB,driver and observers are separate. Full lifecycle additionally retains quiet/drain/exit CPU rather than moving cleanup outside accounting. B full-lifecycle CPU is112.771/231.852/26.403/2980.554µs per correct JSON/PG/ok/work result,versus112.706/231.438/26.381/2979.192 over driver lifetime. These remain unqualified diagnostic points.

Three-second p99 upper bucket edges for A/B areJSON1.55/1.50ms,PG2.72/2.41ms,ok1.21/1.23ms andwork7.92/7.12ms. PG/work sample counts do not support p99.9. Short windows do not replace formal30s comparisons or long-term acceptance.

Final CI completed at67ece0c:19 passed,3 conditional skips,zero failures,including the full macOS suite. See ci-summary.json/ci-final.json for exact jobs. CI success is independent of performance adoption.

Layered environment evidence:all three30s controls reported guest SwapTotal=0 and zero deltas in service Linux major faults and CPU quota throttling. Minimum guest MemAvailable is in environment-layer-summary.json. Guest swapping or service-domain memory exhaustion was not observed. Host single-Renderer RSS peaks were about1.13–1.26GiB and lint node496–499MiB; shared RSS is not additive exclusive memory. Host swapping was active,but whether host-backed VM pages were swapped remains unknown; zero guest major faults cannot exclude that layer.
