# 0.0.19 continuation: JSON, PostgreSQL and fixed memory

[简体中文](json-pg-memory-followup.md) · 2026-10-29

**One same-capacity fixed-memory improvement is retained. JSON and PostgreSQL CPU-parity targets remain unpassed.** Three CPU candidates were withdrawn because end-to-end evidence did not support adoption. Runtime work stays on `codex/v0.0.19-json-pg-memory`: no runtime merge to main, release, website deployment or production-database operation.

## Outcomes

| Area | Confirmed | Boundary |
| --- | --- | --- |
| JSON profile | Actual generated handler,7212 cpu-clock SELF samples,zero loss,zero unresolved SELF symbols; string writing,UTF8,append/copy are major items | Dense execution is not HTTP scoring; inclusive stacks overlap; not every phase is attributed |
| JSON candidate | Removing unused scalar computation removes real O2 work; one cheap A/U pair was146.180→138.814 us/correct | Five groups did not confirm benefit; width candidate reverted in93a0a77. Old JSON implementation and engine remain, necessary raw-byte regression retained |
| PostgreSQL | Bitmap reduces empty-table scanning; normal async signaling demonstrably wakes4 waiters for one released connection | Bitmap screen had89 HTTP500 errors; single-notify screen301.614→316.002 us/correct showed no gain. Both reverted; neither a proven stable regression nor a fix for89timeouts |
| Fixed memory | ab051e8 reduces status-line buffer256→64B/connection. Same4096capacity,23threads and3000correct connected requests: current−507904B,anon/RSS−573440B after high water and close | Local resident saving, not all-workload or Rust-memory parity; capacity was not reduced |
| Metrics fixture | Added accept/header/first-body/stage/error witnesses; retain result and join before asserting | Original100ms deadline,40ms wait,400ms bound unchanged; historic main failure still not uniquely reconstructed |
| Rust gap | One prearranged five-group JSON comparison retained in full | Nonstationarity and send delays prevent stable ranking. Final PG normal/strict-reset comparison was not completed; historical scores do not fill the gap |

Final product source93a0a77 differs fromc9ce1dc in the backend only by the status-line buffer. PG,reactor and JSON runtime experiments are restored. Paused generic Value copy/drop and AST work, withdrawn A3/A4 and yyjson remain untouched. No longer timeout, lower check cadence or SQL retry.

## JSON CPU evidence and bounded experiments

The dense harness called the actual generated `encode(KuStruct___ku_http_request)` fromc9ce1dc C corresponding to the normal CLI. It legally created/transferred an owned body,query map and RID on every iteration and used normal cleanup. Twenty varying cases cover objects,arrays,escaping,Unicode/emoji,NUL and numeric boundaries;16-byte HTTP RID and32-byte diagnostic RID are separate. There were1,783,808 complete output comparisons,15.003814 thread-CPU seconds and15.552275 wall seconds. cpu-clock497Hz,DWARF8192 produced7212samples and0lost. Matching libc6-dbg2.39-0ubuntu8.9 resolved SELF symbols without upgrading libc.

Logical SELF uses the first rendered frame per sample,possibly inline; denominator7212:

| Function | Samples | Fraction |
| --- | ---: | ---: |
| ku_json_write_string |1188|16.4725%|
| ku_json_utf8_scalar |1088|15.0860%|
| ku_json_buffer_append |907|12.5763%|
| __memcpy_generic |657|9.1098%|
| ku_json_parse_string |615|8.5275%|
| ku_json_parse_value |400|5.5463%|
| ku_json_write_value |303|4.2013%|
| _int_malloc |206|2.8564%|
| memcpy@plt |126|1.7471%|
| encode |121|1.6778%|

First non-inline physical-frame attribution is different,including1204 write_string,1044 append and480 parse_value; it cannot be added to this table. Raw perf,captured chains,PC→generated C/DWARF mappings and disassembly are preserved. Resolved symbols do not establish complete attribution. Dense loops alter cache locality,allocator reuse and scheduling; their CPU cannot be subtracted from HTTP CPU.

The existing allocation ledger remains:decode28/35 allocation calls and7800requested bytes;encode4 calls and3264requested bytes. These are neither CPU fractions nor resident memory. The width-only experiment retained all legal-byte boundaries,invalid-byte errors,OOM precedence and cleanup. Its cheap HTTP pair had15000correct responses each and no errors,about5.04% lower CPU at one point,but264192B higher median memory. Five-group results did not support adoption; the old scalar helper is again default.

The capacity-hint experiment started root containers of at least2 fields at128 rather than64B. All76 cases matched bytes/semantics and349 OOM positions cleaned up/recovered. The fixed object used4→3 allocation calls,but small objects doubled peak64→128B and a large single span requested more bytes. It was rejected before product/HTTP scoring. There was no second full tree traversal,permanent large cache or borrowed short-lived input.

## Five groups after ordinary native builds

A is a reused verified normalc9ce1dc CLI Release artifact;B is the subsequently withdrawn UTF8-width+first64 combination;R is Rust normal. Modes are helpers/text/async/int,C11O2,clang18.1.3,same protocol/data/DB/resource limits. Each window warms10s and scores30s at500/s. A fresh instance starts only after its predecessor exits and removes its domain; no frozen other-language services or overlapping builds. Order:A-B-R,B-R-A,R-A-B,A-R-B,B-A-R. No probes,sampling,diagnostic counters orO3 in scores.

Service CPU us/correct;every window15000correct,valid_driver=true,zero body/transport/HTTP errors:

| Group | A | B(withdrawn) | R |
| --- | ---: | ---: | ---: |
| 1 | 239.825 | 123.593 | 97.745 |
| 2 | 228.594 | 241.517 | 97.145 |
| 3 | 141.643 | 241.834 | 103.344 |
| 4 | 119.396 | 232.378 | 102.544 |
| 5 | 147.438 | 499.484 | 351.173 |

All225000responses were correct. Paired-window geometric means and descriptive bootstrap bounds:B/A1.4371[0.7928,2.4020],B/R1.8836[1.4642,2.3821],A/R1.3107[0.7268,2.1476],20000 resamples,seed20261028. Independent stationary-window assumptions are visibly doubtful. These are not stable causal estimates,proof of slowdown/parity or a language ranking. B is not the final default.

The completed `/ok` guard had60000correct per side and94.064→120.051us/correct,failing the intended3% point guard. The coordinator interrupted CPU-guard A during scoring;final PG comparison never started. Interruption/cleanup is retained as operator action,not a product failure or pass. No replacement windows hide results. Full performance acceptance did not pass.

## Environment: confirmed,supported,unknown

This task launched no host Node process. Each completed window had one guest service,one Go driver and the private PG fixture;previous services exited. Limits:service4cores/1GiB,driver CPUs4–6/1GiB,DB CPU7/512MiB;guest memory about4.093GB,host8GiB. Host and guest memory cannot be added.

Host monitoring captured several unrelated high-CPU Node processes and active swap traffic. Around JSON group5B,the32s host bracket recorded1020723200B swap-in and1447165952B swap-out;observed summed Node RSS peaked around946MB and may include shared pages. Guest available memory stayed at least about3.50GB,guest swap total was0,and service/driver major-fault deltas were0. Host paging does not prove Ku was swapped out;minor faults are not automatically swap faults.

That window's actual driver send-delay p99 bucket was337.92ms,max1023.338ms;correct completion p99 bucket481.28ms,max1197.564ms. All JSON valid_driver flags remain true:preparation/accounting passed,but that flag does not establish timely arrivals or stationarity. Environmental confounding is supported;which host pages were paged,exact thread ready/wait time and each request's cause remain unknown. The89PG errors remain service/candidate failures,not discarded as environmental invalidity.

Remaining scoring was stopped rather than repeated. No seven-language,history504,30min or36-memory-recovery rerun. The next decision requires reproducible isolated windows with stable send timing,or host/guest scheduling evidence sufficient to separate cost drift. Current data cannot justify a default CPU-speedup claim.

## PostgreSQL minimum cost table and decisions

| Component | Confirmed dynamic/work evidence | Unknown |
| --- | --- | --- |
| Pool | Separate diagnostic3000claims:2992reuse,8connect,97enqueue,3000healthy release. Coarse create→claim wall sum1148ms,max175ms includes scheduling/connect,not pure queue wait | Lock CPU,precise queue wait,phase CPU |
| Query |3000business sends;9000materialize calls;3000growth yields;78451validated/copied bytes each;3000result/cell/byte allocation or growth calls each | CPU fraction of conversion/allocation;counts are not time |
| Reset |3000DISCARD ALL sends and3000terminal successes,0failures;original deadline,same lease,complete drain/session validation | Reset-only CPU versus shared libpq work |
| Shared libpq/Task/reactor | Old1142samples:service335,kernel581,libc155,libpq46,vdso25;44alias SELF from Task-net. New counters:3247NEW,10088429scanned slots,0retired aliases | Old sampling was perturbed;not precise current CPU shares;query/reset/kernel/scheduling cannot be uniquely split |
| DB server | Independent domain/CPU/time-window raw records;query/reset counts separate | SQL versus network/wakeup/background CPU;must not add DB CPU to service CPU |

Bitmap e142c3d's first real PG screen had89HTTP500s,all`database_error:acquire_timeout`,with valid driver. It was reverted in710a288 and preserved on an experimental branch. Only12full failure snapshots remain:the first4retained snapshots areCONNECT,the other8ACQUIRE and consumed14–25notifications. Full thread CPU/stacks were not captured;root cause is unknown. Static equivalence review does not turn this failed screen into a pass.

The subsequent single-notify candidate dec9a47 reduced one-release wake/ready4→1. Fourteen isolated old/new cases and20candidate regressions passed,with1live opt-in not enabled;coverage included real interest-driven handoff,cancel/expiry,multiple idle leases,128-waiter close and callback pinning. Ordinary CLI N/Q each returned6000correct,valid driver and normal cleanup;301.6135→316.0018us/correct showed no gain. It was reverted in27d9d57 and kept in history/experimental branch. One pair is not a stable regression and notification amplification has not explained89timeouts. The previously rejected simple-query-reset experiment remains withdrawn;business SQL is never retried.

## Actual fixed-memory change

Linux ARM64 KuHttpConnection1112→920B removes786432requested bytes at unchanged4096capacity. After3000correct connected requests and close:

| Metric | first256 | first64 | Delta |
| --- | ---: | ---: | ---: |
| memory.current |7426048|6918144|-507904|
| anon |6270976|5697536|-573440|
| RSS |12636160|12062720|-573440|

Connections finally reached0,threads remained23,and natural exit/domain reclamation passed. Status is normalized100..599;longest built-in reason431 is31B,complete line including NUL47B<64. Existing negative/truncated formatting cleanup remains.

Other fixed objects include blocking jobs/ring1540096B,Task slots638976B,reactor registrations596544B and hash524288B. They are allocated at startup and released after corresponding owner/handler/Task physical retirement;complete size/first-touch/lifetime tables are retained. The source-level65536B portable poll snapshot is already O2-eliminated and cannot be claimed again. No thread,queue or connection capacity was cut. Virtual stacks,requested bytes,RSS and cgroup charges remain distinct.

Final93a0a77's ordinary Linux CLI artifact passed `/ok`200,JSON200,PG100,CPU20:520correct with physical quiet,natural exit and domain reclamation. Its full `.text` matches the independent M memory-screen artifact. This is functional verification,not CPU scoring. Normal CLI,C and ELF identities/logs are in the evidence receipt.

## Validation,delivery and open scope

Final Rustfmt,clippy/type check anddiff check passed;20JSON regressions passed again after withdrawal. Status-line/partial-vector and3metrics-witness tests passed. Detector results identify candidate versus final source;Linux final CI is the actual submitted revision's Checks,not an old run.

Necessary permanent regressions extend existing test files. Temporary probes/scripts/copies/builds/private DB are cleaned;compact raw/failure evidence remains. No schema/migration or production data change. Paused modules and old JSON default remain. No promise turns every third-party-library OOM into an ordinary Result.

Primary workspace recheck against443existing-file baseline found442unchanged,README separately modified,0missing. NewLICENSE/NOTICE/README.en.md are preserved. The user's pre-existing dirty multicore document in the development checkout retains its SHA.

**Open:JSON/PG CPU parity,final default/strict-reset PG five groups,four-path memory versus Rust,and stable-tail/full performance acceptance.** This delivery confirms a local memory improvement and bounded diagnostic/candidate decisions,not completion of allv0.0.19performance goals.

## Transferable evidence

[Structured Git summary](evidence/json-pg-memory-20261029.json). The private delivery folder `性能续执行_2026-10-29` and its self-contained archive contain bilingual reports,index,compressed rawperf/chains/PC/C mapping/disassembly,per-window client/events/resources/runtime/exit records,environment alignment,candidate/OOM/regression and final-functional receipts. Compression preserves uncompressed SHA. No private DB config/URI or diagnostic scripts are uploaded to Git;raw stacks/host inventory stay in the local package. Older2026-10-28results remain separate. CI link and cleanup receipt are updated in the delivery evidence.
