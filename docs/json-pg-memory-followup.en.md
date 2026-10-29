# Post-0.0.19: JSON, PostgreSQL and fixed memory

[简体中文](json-pg-memory-followup.md) · 2026-10-29

**Performance targets remain incomplete.** One reproduced startup-configuration bug is fixed. A PostgreSQL protocol experiment showed no service CPU benefit and was reverted. JSON keeps the old implementation. Paused Value/AST work, release tags and website deployment remain unchanged.

## Scope and identities

The relevant runtime paths in frozen `d8c33bca3c629ce82d0d35709c6931f16c86d05b` and release `73e9f2fef6861b0bc1ce441471726b926a8a174b` match. A is built through the ordinary Release CLI: helpers/text/async/int, C11 O2. No withdrawn A3/A4, direct/O3 or yyjson experiment was enabled.

Development stays on `codex/v0.0.19-json-pg-memory`, away from main. Existing dirty source in the primary checkout is preserved. Website-only [PR #9](https://github.com/fengyanweb/ku/pull/9) merged as `6aedaf4b0c4e68612c49a23a8607f0c9901bb4ff`; its source SHA inventory matches the retained 930-page build/link/asset/mobile/example checks. It was not deployed.

## JSON cost ledger

The JSON route is synchronous HTTP. Its actual path transfers body ownership, decodes, reads the scalar counter, takes the data subtree, releases the remaining root, builds the envelope, encodes, moves into http.text, and releases owned data. Subtree take, UTF8 spans, integer formatting and body transfer were already implemented.

The existing allocation hook measures the actual generated Transform using a 1,048-byte object and a 32-byte owned RID. The HTTP driver's RID length is recorded separately; this is a mechanism fixture, not the whole HTTP process allocation ledger.

| Stage | Allocation/reallocation calls | Requested bytes | Explicit memcpy bytes |
| --- | ---: | ---: | ---: |
| Decode | 28 | 7,800 | 892 |
| Counter, take, remaining-root cleanup | 0 | 0 | 0 |
| Envelope including owned RID read | 3 | 696 | 32 |
| Encode | 4: first allocation plus three growths | 3,264 | 1,087 |
| Total | 35 | 11,760 | 2,011 |

Peak tracked payload is 8,152 bytes; object, array and escaped variants finish with zero live tracked allocations. Requested bytes include the full new realloc request. Copy counts exclude libc relocation, struct assignments, HTTP projection and transport. These figures are neither RSS nor proof of process-wide leak absence.

Normal and symbol-bearing profile ELF `.text` SHA is `b5aacdd91c550ae738b4f137fd82beefb424b18b69d5f6afd23f58dd2a9a5d68`; generated C SHA is `d2456e0c438a221d950f3f01744193f0af74cd4c6bf144404a996695719a5073`. Ordinary A ELF SHA is `d1e0b719e542172a6806c0398f87e28bc533b4ca743dd342581b2151910eed56`, profile ELF `c2f7d6b55cc8390d7ad29d0942fae8506a2315e201d74b98c6a8e9ab63283170`. An early objcopy dump rewrote normal ELF metadata; the ordinary CLI restored the original binary before A/B screening. Earlier captured text was identical.

The low-overhead profile has 95 SELF samples and no loss: kernel 45, Ku 26, libc 24. Only nine captured stacks are inside JSON, insufficient for ranking small candidates; unresolved libc/callers remain unknown. The first collection lost 11.42%; the second after-control had an invalid driver and timeouts. Both failed attempts are preserved. Before/sample figures were not selected as a valid pair. JSON was not patched from this evidence.

## PostgreSQL: rejected protocol experiment

Commit `f2ff379` tried simple query for constant DISCARD ALL while retaining parameterized business SQL, exclusive lease, original deadline, terminal-NULL drain, IDLE/UTF8 checks, eviction and owned result lifetime. Rust defaults use deadpool-postgres Fast without this reset. Rust's explicit equal-reset mode must remain a separate comparison.

One ordinary CLI Release screen used 200 requests/s, five-second warmup and ten-second measurement. Service, driver and private database had separate resource domains; A exited and its domain was removed before B started.

| Metric | Original A | Candidate B |
| --- | ---: | ---: |
| Correct / planned | 2,000 / 2,000 | 2,000 / 2,000 |
| Service CPU µs/correct | 317.843 | 319.273 |
| PG CPU µs/correct, separate sampled boundary | 294.115 | 286.603 |
| Steady median service memory.current bytes | 4,085,760 | 4,044,800 |
| Scheduled-to-complete p99 bucket ceiling ms | 3.14 | 2.86 |
| Maximum send delay ms | 7.957292 | 8.743190 |
| Supplemental post-driver CPU seconds | 0.000117 | 0.000149 |

Both drivers were valid with zero errors, quiet resource counters, normal exit and successful domain removal. B/A=1.00450 did not show service CPU improvement. One pair cannot establish significance, stable memory savings or a Rust ranking. `bee69ea` restores the original extended-reset default; the experiment remains in history and on `codex/v0.0.19-pg-reset-experiment`. It was not repeatedly rerun for a favorable score.

Reused regression command: 21 passed, one opt-in live test not enabled there. The existing LIVE_LOOPBACK fixture then passed separately against the candidate's ordinary generated C and private PostgreSQL, covering parameters, owned Unicode/NULL, multiple results, errors, timeout eviction and later queries. Protocol-call counts come from the existing stub and are not claimed as a live wire trace. Business SQL is never automatically retried.

A separate PG profile has 1,142 on-CPU samples and zero loss. Its CPU cost was materially above the controls, so it is diagnostic only. Query/reset CPU, pool waits and extra state-machine progression cannot be uniquely attributed from totals. Database CPU and poll counts are not service CPU attribution.

## Fixed resources and retained bug fix

The 23 threads are main 1, output 1, event loops 4, synchronous HTTP workers 16, and one Task background worker. The external Task driver creates no additional pool. JSON/ordinary routes need the handler pool; disabling it would change capability. Sleeping threads do not establish busy polling.

An idle sample has memory.current 3,448,832 bytes: anon 2,211,840 and kernel 974,848, including kernel_stack 360,448, pagetables 204,800 and slab 369,576. Do not add included kernel components twice; kernel counters may update in batches. RSS is 8,368 KiB and PSS 4,002 KiB under different accounting. Guest pswpin/out deltas were zero and service smaps Swap was zero during the first attempt, despite substantial host swapping. This does not attribute a particular delay to Ku swap-out. Reserved capacities, allocator retention and resident bytes remain distinct; no speculative pool trimming or memory-saving claim was made.

**Fix `b8f3497`:** explicit two event loops with an omitted blocking-thread count previously defaulted to two and then deducted the Task background worker, rejecting startup. HTTP now includes that reservation in its implicit default under the existing total budget. Explicitly insufficient counts still fail; ordinary Task-root behavior and benchmark B17 are unchanged. Six configuration combinations and existing startup-failure, pure-native 200/404/405, mixed async/file/deadline/shutdown checks passed across seven test functions. Format and diff checks passed. No production database migration was needed.

## Remaining work and evidence boundaries

| Goal | Status | Next required evidence |
| --- | --- | --- |
| JSON CPU/memory at or below Rust | Not established; no new default optimization | Sufficient stable on-CPU samples before selecting a candidate |
| PG default and equal-reset parity | Not established; protocol candidate reverted | Dynamic query/reset, libpq/Task/reactor cost supporting a new candidate |
| Fixed-memory reduction | Not established; audit and startup fix completed | Actual resident structure/allocator attribution with equal capabilities |
| Final five-group A/B/R and shared-path guards | Not started | An environment without the demonstrated driver failure; screening is not acceptance |
| Necessary CI | Inspect actual development commit checks | Main's separate macOS fixture-precondition failure must not be labeled success |

Website main [CI](https://github.com/fengyanweb/ku/actions/runs/40828377118) passed Windows/Linux suites, lint, sanitizer and package/PG checks. Its one macOS lib failure asserts that the slow body was never exercised; evidence upload subsequently failed because expected files were absent. Error-return and the 400ms upper-bound assertions had passed. Accept/header/first-byte detail was not retained, so the exact precondition failure remains unknown. No product deadline bug is established and no unrelated product refactor was added.

Local evidence is under `/Users/mac/Documents/Ku/实施/JSON_PG_后续_2026-10-28/`: plan, allocation ledger, profile/PC analyses, host time series and `raw/` request/perf/C/ELF/window/checksum records. Compressed originals retain their decompressed SHA. Raw stack dumps remain private rather than being uploaded unreviewed to Git; database connection details are excluded. Temporary scripts, probes and duplicate build products are removed after verification. Existing source, tests, dirty work and necessary failure evidence are preserved. Historical scores, structural counts and short screens are not presented as final performance or production acceptance.
