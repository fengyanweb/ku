# Ku native C comparison service

Build a copy of `server.ku` after replacing `__PEER_PORT__` with the controlled
peer's integer port. Record both template and expanded-source hashes. Use the
actual release CLI, its normal C backend and the existing PostgreSQL link input:

```sh
KU_PG_LIB=/absolute/libpq-directory ku build --backend c --profile release --emit-c -o /absolute/output/server /absolute/expanded/server.ku
ku build --backend c --profile release --emit-c -o /absolute/output/batch batch.ku
```

The server's private working directory contains `config.json`:

```json
{"addr":"127.0.0.1:19090","peer_port":19091,"db_url":"postgresql://user@127.0.0.1:19092/test?sslmode=disable"}
```

Use only the isolated benchmark database. The table, data, request corpus,
integer semantics and response validation are defined by `../protocol.json`.
For the frozen 1/2/4 core groups, set `KU_EVENT_LOOPS` accordingly,
`KU_BLOCKING_THREADS=17`, and `KU_THREAD_BUDGET=64`. All threads still share the
whole-service CPU and memory resource domain; 17 is a thread deployment choice,
not an extra CPU allowance. The HTTP synchronous pool has 16 workers and the
Task background pool has one. Keep the original JSON default and all ordinary
ownership, cancellation and budget checks enabled.

The `/ok`, `/work`, `/timer`, `/wait` and `/tcp` handlers use native async Tasks.
The current async frame lowering cannot hold decoded `KuValue` locals, so `/json`
uses the existing bounded synchronous handler path. `/db` also uses that path.
The CPU format helper is split to respect the existing 64-slot frame ABI. These
are product limitations and their complete costs belong in the measured result;
no compiler/runtime modifications or external JSON/CPU replacements are used.

The native HTTP helpers currently cannot emit a JSON-specific Content-Type:
`http.json` lacks native lowering and the response header map is not a writable
native assignment target. JSON bodies therefore retain the existing `http.text`
path and `text/plain; charset=utf-8`; other stacks use their ordinary JSON MIME.
Bodies are deeply equivalent, but the result must disclose this header/API gap
instead of claiming identical response-header semantics.

The HTTP limits are the existing maxima of 4,096 connections and 1,024 active
requests, with 128 pending requests and 64 MiB accounted HTTP buffers. The runner
must stop higher Ku waiting tiers at the actual active/Task limit; it must not
call rejected requests held Tasks. For wait counts and reclaiming resources,
collect the native management endpoint configured by `KU_SERVICE_PROBE_ADDR`
and `KU_SERVICE_CONTROL_TOKEN`; it is independent of ordinary application routes.

`batch.ku` reads `batch.json` with `iterations`, `seed`, and `repeat`. It prints
the complete numeric checksum array and internal monotonic `compute_ms`, whose
resolution is 1 ms. Read/parse/startup/output are outside that internal interval;
normal result-array construction is inside. The runner separately records
whole-process elapsed time. No benchmark results should be inferred from build
or functional-smoke timings.

`repeat=0` is supported only for the separate fresh-process startup observation.
It returns an empty checksum array and is never subtracted from `repeat=32768`.
