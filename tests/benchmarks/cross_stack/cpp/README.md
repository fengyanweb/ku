# C++ full-stack comparison

Clang18, C++17, normal Release `-O3 -DNDEBUG`, Drogon1.9.14/Trantor exact submodule,
JsonCpp1.9.5 dynamic values and Drogon asynchronous PostgreSQL. Source commit,
archive SHA256 and distribution JSON package are in `dependencies.lock.json`.
Provisioning verifies downloads before extraction; the build records installed
Drogon/Trantor/JsonCpp/libpq/Clang identities. Normal framework code is used.

Drogon event loops equal cores; a bounded CPU executor has cores workers and128
queued jobs. A single PG client pool has16 total connections. Query uses extended
protocol parameter binding, typed INT4 conversion and fresh JSON response; no
SQL retries or cached response. Unlike Ku, Drogon's pool does not run `DISCARD
ALL` after every query; retain this difference in the idiomatic PG table.

JSON is decoded into JsonCpp's real tree, counter changed, the data subtree moved
from the dead input into a new envelope, then freshly serialized. Gzip and Brotli
are disabled for shared uncompressed HTTP semantics. No benchmark URL gets a
preencoded or precomputed answer. All helper threads and libraries count in the
whole cgroup. `cpp-server --batch INPUT` uses the same serial integer recurrence,
monotonic internal timing and fresh-process full-domain accounting.

Official source checked 2026-10-23:
https://github.com/drogonframework/drogon/releases/tag/v1.9.14

Scope: HTTP/JSON/mixed (PG+timer), batch. No additional TCP waiting claim. Metrics
show actual active, waiting timer, CPU reservations and DB-pending counts; the
framework does not expose complete allocation/byte attribution here.

The earlier O2 SDK golden is functional preparation only. Normal comparison
rebuilds both Drogon/Trantor and the application with O3; compile_variants.py
keeps O2, ThinLTO and trained PGO in distinct artifact roots and receipts.
