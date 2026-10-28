# Ku 0.0.19: one-round seven-stack performance diagnostic

[中文](performance-v0.0.19.md) · [Public measurement data](evidence/performance-v0.0.19.json) · [Release limitations](v0.0.19-known-issues.en.md)

**This is one strictly serial, fixed-arrival-rate short diagnostic, not maximum-capacity testing, the seven-round formal matrix or 30-minute stability acceptance.** All 583,800 of 583,800 requests across 28 scored windows were correct, and all seven stacks passed their golden checks. These observations describe specific implementations and this environment, not a stable language ranking or production endorsement.

## What this round establishes

- For the same integer-computation HTTP kernel, Ku service CPU per correct request was **2.608 ms**, versus Rust's **3.849 ms**, a descriptive ratio of **0.678**. This is a single normal-Release observation with fixed inputs, not a general speedup promise.
- Ku's simple-response, JSON and PG service CPU costs were respectively **1.037, 1.260 and 1.350 times** Rust's. Rust had a lower cgroup-memory median in all four workloads. The results do not establish universal superiority for Ku.
- Correct responses do not eliminate tails. Ku JSON reached **219.773 ms** and PG **230.509 ms**. Other stacks also had tails; Node PG's client-recorded p99 was **236.8 ms**. Without same-RID server stages or dynamic profiling in this round, unique causes remain unknown.
- Actual host swap-in and swap-out occurred. Guest measurements did not show memory-limit events, major-fault increments or memory-PSI increments. This neither proves zero host impact on tails nor proves that Ku itself was swapped out.

## Targets and equivalent-work boundaries

The candidate commit was `d8c33bca3c629ce82d0d35709c6931f16c86d05b`. Ku used **helpers/text/async/int, normal Release C11 O2**, not direct or Fast O3. The coordinator verified that `src`, `crates`, `Cargo.toml`, `Cargo.lock` and `build.rs` remained unchanged after the frozen candidate; subsequent documentation, packaging metadata and existing test-contract changes are separate. This report still does not replace final CI or package verification.

The environment was a Linux ARM64 VM running Ubuntu 24.04, kernel 6.8.0-147-generic: Rust 1.89.0, Clang 18.1.3, Go 1.27.2, Node 24.18.0, Java 25.0.4.1, .NET SDK 10.0.402/runtime 10.0.13, Drogon 1.9.14 and JsonCpp 1.9.5. Each stack used its normal optimizing build and GC/JIT settings; C++ used O3 and Ku O2, rather than a uniform optimization level. Java/.NET heap maxima were 65% of their service cgroups, not whole-process limits.

The service domain had 4 cores (0–3)/1 GiB; the separate driver had 3 cores (4–6)/1 GiB; private PostgreSQL had 1 core (7)/512 MiB. They still shared one VM and physical host. Limits are not reserved physical memory; host and guest memory must not be added twice. Every stack/workload received 10 seconds of warmup and 30 seconds of measurement. Order was Ku→Go→Rust→Node→Java→.NET→C++; each stack actually stopped before the next started, without retaining frozen previous services.

HTTP/1.1 keep-alive was used without TLS, compression or a proxy. Client concurrency was capped at 256, idle connections expired after 500 ms, the absolute deadline was 3 seconds from scheduled arrival, and requests were not retried.

- **CPU:** 480/512/544/576 × 1,000 iterations, with seed-string lengths 1/3/7/11; `total=(total*33+i+seed)%100000000`, initialized to seed, with the eight-digit checksum validated. Integers remain within the stated bounds.
- **JSON:** a fixed 1,048-byte object is decoded; counter increases from 7 to 8; a new envelope includes the request ID and decoded data subtree, is encoded, then semantically validated. This round's Ku handler first reads the scalar `counter` through ordinary indexing, then consumes the root with `decoded.take("data")?` to transfer the subtree; it does not use the old indexed-copy path for the whole data subtree. Full decoding, the scalar lookup, remaining-root cleanup, new-object construction and encoding remain. Rust moves and Go/Node retain references, but transfer alone does not establish equal ownership, allocation or cleanup work across stacks. Ku's text API also differs from the other stacks' JSON MIME types.
- **PG:** the same 1,024-key dataset, parameterized SQL and pool cap of 16, without result caching or SQL retries. Ku runs `DISCARD ALL` on return; counterparts use their default reset policies. These are not equal-reset-work measurements.

The frozen [Ku fixture](https://github.com/fengyanweb/ku/blob/d8c33bca3c629ce82d0d35709c6931f16c86d05b/tests/benchmarks/cross_stack/ku/server.ku#L25-L32) and retained generated C confirm `ku_value_take_result` at the handler call site. An older descriptive ownership field in the protocol was stale; the source hashes, scalar-lookup and transfer locations are retained in the public JSON. This corrects the ownership description only; no measurement changes.

See the candidate's [protocol](https://github.com/fengyanweb/ku/tree/d8c33bca3c629ce82d0d35709c6931f16c86d05b/tests/benchmarks/cross_stack/protocol.json) for the complete algorithm, corpus hashes and contracts. Its seven-round/30-second-warmup settings describe the full formal matrix; **this round explicitly used a separate single-round plan with 10-second warmup**.

## Measurement definitions and results

CPU is the **service cgroup CPU increment per all correct requests over the driver process lifetime**, including service work while the driver starts and finishes its terminal tail. It excludes service cold start, later final teardown, and driver/observer/PG CPU; it is not full-service-lifecycle cost. PostgreSQL server cost is retained separately in the raw records.

p99 is the **client histogram's conservative upper bucket edge**, from scheduled arrival through the complete correct response, including client queueing, connection establishment, I/O and validation. Resolution is 10 µs below 2.56 ms and relative bucket width is at most 0.78125% above it; it is not an exact sorted percentile. Maxima are observed values. Memory is `memory.current` sampled at 1 Hz during arrivals, taking the median of the middle 60% of samples: **not RSS**. Threads are the sampled whole-service-tree range, including runtime/support threads.

### GET /ok — 2,000/s

| Stack | Correct | CPU µs / correct | Client p99 ms | Max ms | Cgroup median MiB | Threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ku | 60,000 | 44.456 | 1.42 | 10.551 | 4.090 | 23 |
| Go | 60,000 | 82.743 | 3.12 | 170.688 | 9.094 | 10 |
| Rust | 60,000 | 42.889 | 1.68 | 16.590 | 1.709 | 5 |
| Node.js | 60,000 | 105.125 | 11.68 | 229.722 | 137.182 | 39 |
| Java | 60,000 | 48.874 | 1.41 | 21.644 | 104.104 | 24 |
| .NET | 60,000 | 86.744 | 1.39 | 9.946 | 45.203 | 22–23 |
| C++ | 60,000 | 40.667 | 1.65 | 11.390 | 4.625 | 17 |

### POST /json — 500/s

| Stack | Correct | CPU µs / correct | Client p99 ms | Max ms | Cgroup median MiB | Threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ku | 15,000 | 180.154 | 8.72 | 219.773 | 4.508 | 23 |
| Go | 15,000 | 272.399 | 1.73 | 10.076 | 8.346 | 10 |
| Rust | 15,000 | 143.021 | 3.76 | 126.375 | 2.473 | 5 |
| Node.js | 15,000 | 232.578 | 2.10 | 38.331 | 148.850 | 39 |
| Java | 15,000 | 203.938 | 14.80 | 337.436 | 109.352 | 24 |
| .NET | 15,000 | 375.787 | 1.88 | 41.595 | 46.488 | 21–22 |
| C++ | 15,000 | 136.994 | 1.79 | 8.862 | 4.570 | 17 |

### GET /work — 80/s

| Stack | Correct | CPU µs / correct | Client p99 ms | Max ms | Cgroup median MiB | Threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ku | 2,400 | 2608.107 | 6.28 | 10.260 | 4.484 | 23 |
| Go | 2,400 | 4329.287 | 6.72 | 12.720 | 8.783 | 10 |
| Rust | 2,400 | 3848.638 | 8.00 | 15.014 | 3.043 | 6–7 |
| Node.js | 2,400 | 9393.204 | 12.48 | 69.438 | 154.881 | 39 |
| Java | 2,400 | 3745.845 | 6.60 | 14.304 | 110.334 | 24 |
| .NET | 2,400 | 3886.961 | 6.64 | 31.698 | 46.154 | 21–22 |
| C++ | 2,400 | 3806.060 | 9.40 | 74.216 | 4.568 | 17 |

### GET /db — 200/s

| Stack | Correct | CPU µs / correct | Client p99 ms | Max ms | Cgroup median MiB | Threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ku | 6,000 | 358.082 | 4.06 | 230.509 | 4.924 | 23 |
| Go | 6,000 | 571.679 | 15.36 | 327.339 | 8.740 | 10 |
| Rust | 6,000 | 265.282 | 26.40 | 375.319 | 3.273 | 5 |
| Node.js | 6,000 | 992.544 | 236.80 | 445.888 | 177.953 | 39 |
| Java | 6,000 | 525.478 | 3.10 | 20.578 | 121.467 | 24 |
| .NET | 6,000 | 933.528 | 3.60 | 10.421 | 54.078 | 20–22 |
| C++ | 6,000 | 333.591 | 3.16 | 66.229 | 6.350 | 17 |


All client ledgers were marked qualified, and event rows reconciled to terminal counts. This establishes only the applicable checks in these short windows. `scheduled_to_generated` contained **zero samples**, so a measured generator p99 cannot be claimed. Available scheduled-to-attempt p99 was 110.72 ms for Node PG and 3.24 ms for Java JSON. The driver used at most approximately 7.2% of its three-core quota on average, which does not exclude bursts, stalls or throttling. All seven controlled stops completed with empty service domains and no force-kill. Java returned the expected SIGTERM exit code 143; the others returned 0.

## Retained tails and attribution limits

This table lists windows containing requests above 100 ms and the client segments of each window's slowest correct request. The 100-ms cutoff is **for displaying tails, not a common business threshold**. Protocol p99 thresholds for ok/JSON, CPU and PG are respectively 100, 1,000 and 250 ms.

| Stack / case | Maximum ms | Above 100 ms | Scheduled → attempt ms | Attempt → written ms | Written → complete ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ku / json_object | 219.773 | 57 | 42.875 | 0.026 | 176.872 |
| Ku / db | 230.509 | 24 | 0.071 | 14.148 | 216.290 |
| Go / ok | 170.688 | 170 | 10.913 | 18.802 | 140.973 |
| Go / db | 327.339 | 26 | 3.580 | 26.764 | 296.995 |
| Rust / json_object | 126.375 | 6 | 13.516 | 66.070 | 46.788 |
| Rust / db | 375.319 | 37 | 0.663 | 71.105 | 303.551 |
| Node.js / ok | 229.722 | 292 | 55.977 | 101.916 | 71.829 |
| Node.js / db | 445.888 | 120 | 199.220 | 5.278 | 241.390 |
| Java / json_object | 337.436 | 89 | 0.142 | 104.439 | 232.854 |

These segments locate elapsed time, not why a thread stalled. Written→complete includes transport, service, database and client reading/scheduling; it is not Ku CPU time. For example, the slowest Ku JSON request reused its connection but spent 176.872 ms after writing. The slowest Ku PG request was the window's first cold connection, with about 13.780 ms of dialing. The remainder is unknown and is not directly evidence of a PG execution defect.

Accounting also recorded 213.151 ms of driver cgroup CPU throttling in Java JSON and 26.323 ms in Node PG's database domain; corresponding events were 0.044 ms for the Ku JSON driver and 1.106 ms for Ku PG's database. These events are confirmed within their windows, without per-request causality, and do not explain all tails. With no server stacks, per-request server stages or dynamic hotspots, this round does not justify a runtime patch.

## Environment: actual paging without exclusive attribution

Across the interior host samples at **18:12:56–18:33:36 UTC**, spanning about 1,239.7 seconds, 16-KiB host-page counters increased by **208,489 pages/3,415,883,776 bytes swap-in** and **208,724 pages/3,419,734,016 bytes swap-out**. This is activity, not merely allocated swap. These per-stack sampled spans include golden checks, warmup, measurement and cleanup:

| Stack | Sampled span s | Swap-in MiB | Swap-out MiB |
| --- | ---: | ---: | ---: |
| Ku | 172.2 | 418.95 | 364.69 |
| Go | 171.9 | 450.77 | 425.75 |
| Rust | 172.2 | 195.31 | 264.00 |
| Node.js | 177.0 | 783.06 | 951.50 |
| Java | 172.0 | 158.31 | 159.69 |
| .NET | 172.0 | 628.41 | 508.56 |
| C++ | 172.0 | 157.33 | 177.38 |

Host samples were approximately five seconds apart, with a maximum gap of 5.121 seconds. Their Unix timestamps approximately align them to guest windows; the host/guest clock-offset bound was not measured, preventing exact per-request attribution. Page-ins and swap-ins are reported separately; file faults are not treated as swap.

Across 28 windows, sampled guest `MemAvailable` stayed above approximately **3,147 MiB** with zero guest swap. Service, driver and PG domains had zero `memory.events` increments and zero `pgmajfault` increments; service memory/I/O PSI increments were zero. These guest counters do not observe the host swapping VM pages, and minor faults are not evidence of swapping. One-Hz sampling cannot exclude subsecond stalls. **Host activity is confirmed; its precise impact and individual tail causes remain unknown. p99 ordering must not be generalized as a stable ranking.**

## Evidence and remaining work

The [public JSON](evidence/performance-v0.0.19.json) contains all 28 windows, accounting definitions, threads/limits/pressure, client tail segments and binary identities. Ku CLI SHA256 is `d9ab7422f3cd037f097f61de83f0493784b60b04187e663840b75d41fe7982b3`; service ELF SHA256 is `d1e0b719e542172a6806c0398f87e28bc533b4ca743dd342581b2151910eed56`. The privately retained raw archive SHA256 is `5eafab92a5ae4addb2b84d3230e3d771682ba6d28eb5c99fe8c3d2bc90695709`. It contains temporary database connection information and **is not committed with this public report**.

Maximum capacity, seven-round paired intervals, long-term memory trends, formal cancellation/overload acceptance and a real production canary are not established here. Historical failures remain in [known issues](v0.0.19-known-issues.en.md). The old JSON default, two paused areas and existing deadline/cancellation/budget semantics were not changed for these measurements.
