# Two explicit PostgreSQL comparison contracts

[简体中文](pg-comparison.md)

These modes belong to the private benchmark. `BENCH_PG_MODE` accepts only an
empty value, `normal`, or `reset-equivalent`; any other value fails startup.
The equal-reset mode must not be presented as Go or Rust's normal default cost.

| Table | Ku | Go | Rust |
| --- | --- | --- | --- |
| A normal | libpq text/single-row results copied into an owned Ku table; `value_int` for id/score and owned `value` for label; one `DISCARD ALL` per lease | pgx text execution, scalar scan and encoding/json; ordinary pool return without added reset | query_typed binary INT4/String and serde_json; ordinary Fast recycling without added reset |
| B reset-equivalent | Same Ku path, without a second reset | Acquire one lease, QueryRow/Scan, execute DISCARD ALL on that lease, drain, release | Acquire one Object, query_typed, materialize owned fields, batch_execute DISCARD ALL on that Object, release after ReadyForQuery |

In table B, Go and Rust share a two-second absolute operation deadline, with
acquisition limited to 250 ms. Reset receives only the original remainder.
Go's HTTP context may shorten that deadline; Rust's outer HTTP timeout can
cancel the whole future but cannot extend it. Table A retains the original
defaults: Go's HTTP context and two-second connect budget; Rust's one-second
pool wait/create and query budgets. These tables do not imply identical wire
protocols or result APIs.

Each business query is sent once. Query/conversion failure evicts the lease;
reset failure evicts it while preserving the already owned result. Go detaches
with Hijack before closing; Rust's guard detaches with Object::take on failure
or future cancellation, and ClientWrapper drop aborts the connection task.
Closing a connection does not prove rollback or justify retrying a write.
The benchmark SQL is a read-only SELECT over a private 1,024-row fixture,
with pool size 16 and unchanged output verification.

## Current Ku default and the legacy diagnostic

The default is `--ku-pg-execution async --ku-pg-read int`: the named async HTTP
handler reads id/score with `value_int`, and label with owned `value`.
This was already shipped in 0.0.19 and is not a new optimization.

`--ku-pg-execution sync --ku-pg-read value` preserves the legacy numeric
`value → json.decode → as_int` conversion for diagnosis. Async rejects value
mode. Compare value/int under the same sync execution mode when isolating that
conversion; record source/binary hashes, `pg_read_mode`, and `pg_execution_mode`.
Do not attribute combined direct-format, JSON-response or async changes to the
integer getter. Neither comparison adds redundant conversions to Go or Rust.

## Rejected fixed-reset protocol candidate

Commit `f2ff379` tried simple query for the constant DISCARD ALL while retaining
lease exclusivity, deadlines, terminal-NULL drain, IDLE/UTF8 checks and eviction.
Business queries remained parameterized. One normal CLI Release A/B screen
produced 317.843 µs/correct request for original Ku and 319.273 for the candidate
(2,000 correct requests each, zero errors; B/A 1.00450). This single pair is not
repeated acceptance and did not establish a reliable gain. The default has
therefore reverted to the original extended reset. The experiment remains on
`codex/v0.0.19-pg-reset-experiment`; release tags and main were not changed.
Two disturbed JSON diagnostic attempts stopped that score; final five-group
comparison was not started. JSON/PG parity with Rust is not established.
See `docs/json-pg-memory-followup.en.md` for evidence identities and boundaries.

## Derived manifests and existing checks

Rebuild Go/Rust using the frozen source's `build_linux.py --only go rust` before
using `pg_variants.py`. Old binaries without explicit mode support are rejected.
The derivation verifies source/lock and binary hashes and does not overwrite
the original manifest:

```sh
python3 pg_variants.py --manifest /absolute/new-build/manifest.json \
  --output /absolute/pg-normal --mode normal
python3 pg_variants.py --manifest /absolute/new-build/manifest.json \
  --output /absolute/pg-reset-equivalent --mode reset-equivalent
```

Use the existing Linux harness with the same DB plan and paired windows.
`build_complete=false` means this is not a new seven-stack build;
`pg_comparison_ready=true` attests identities, not live correctness or speed.

Reuse the Go/Rust tests and manifest-derivation tests listed in the Chinese
document. Live tests require an explicit private `BENCH_PG_TEST_URL`; skipped
or ignored tests do not count as live passes. They cover same-session cleanup,
owned results after reset expiry and eviction after cancelled Rust futures.
Do not infer that DISCARD ALL uniquely explains the historical PG CPU gap.
