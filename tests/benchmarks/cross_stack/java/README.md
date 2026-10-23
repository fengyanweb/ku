# Java full-stack comparison

OpenJDK 25 LTS (Ubuntu's maintained OpenJDK package), Vert.x 5.2.2 (Netty),
Jackson dynamic `JsonNode`, Vert.x PostgreSQL pool. Framework versions derive
from the exact Vert.x BOM; `dependencies.lock.json` records SHA256 of every
resolved runtime JAR and the build rejects changes. Maven/Ubuntu package
versions and full commands are in the build receipt. No shaded/preencoded body.

Each configured event loop shares the same listener and **one** total PG pool
(default16). CPU jobs use a normal bounded executor (cores workers,128 queued).
The real JSON tree is decoded, its counter modified, then a new envelope encoded.
PG uses the common parameterized query and typed INT4 fields; cached prepared
statements are off. It does not add Ku's `DISCARD ALL` contract; this difference
must remain explicit in the idiomatic-PG table. No SQL retries.

`java -XX:ActiveProcessorCount=N -XX:MaxRAMPercentage=65 ... ku.bench.Main`
uses normal tiered JIT and normal GC. Heap settings do not cap total process
memory: GC, Netty, native memory and every helper thread remain inside the common
cgroup. `--batch INPUT` uses the same serial integer recurrence as all stacks;
fresh-process JIT/startup is included, not subtracted. Steady JIT warmup belongs
to the common harness, and unresolved warmup is not a steady result.

Version sources checked 2026-10-23:
- https://openjdk.org/projects/jdk/25/
- https://vertx.io/blog/eclipse-vert-x-5-2-2/
- https://vertx.io/docs/vertx-pg-client/java/

Scope: HTTP/JSON/mixed (including PG+timer), batch. This implementation does not
claim the original four-stack TCP-waiting matrix. Metrics expose actual active,
waiting timer, CPU reservations and pending DB operations; unavailable full-domain
allocation counts are not zero.
