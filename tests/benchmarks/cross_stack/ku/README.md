# Ku native C comparison service

Build a copy of `server.ku` after replacing `__PEER_PORT__` with the controlled
peer's integer port. Record both template and expanded-source hashes. On a
supported native release target, place the expanded `server.ku` and `batch.ku`
in a private `ku-native` directory beside `bin`, then use the actual release CLI
and the existing PostgreSQL link input:

```sh
KU_PG_LIB=/absolute/libpq-directory ku build --backend c --profile release --emit-c -o /absolute/run-root/bin/ku-server /absolute/run-root/ku-native/server.ku
ku build --backend c --profile release --emit-c -o /absolute/run-root/bin/ku-batch /absolute/run-root/ku-native/batch.ku
```

The native filesystem base is the source directory located relative to the
executable, rather than the process working directory. Keep `config.json` and
`batch.json` in `/absolute/run-root/ku-native` for the layout above. The server
configuration is:

```json
{"addr":"127.0.0.1:19090","peer_port":19091,"db_url":"postgresql://user@127.0.0.1:19092/test?sslmode=disable","max_connections":4096,"max_active_requests":1024,"max_pending_requests":128}
```

The controlled Linux ARM64 research build is separate: `build_linux.py` retains
the CLI's unsupported-link diagnostic and explicitly compiles its emitted C
with Clang 18, C11, `-O2`, pthreads and the private libpq input. It does not claim
Linux ARM64 is an officially validated installation target. Preserve the actual
commands, generated C and binary identities from that build manifest.

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
uses the existing bounded synchronous handler path. `/db` defaults to the public
checked async capture of the original shared PG pool, with typed integer reads
and the original same-lease `DISCARD ALL`. The build runner can separately select
the old synchronous PG path; its execution mode and expanded source are recorded
in the build manifest and must not be mixed with async results.
The CPU format helper is split to respect the existing 64-slot frame ABI. These
are product limitations and their complete costs belong in the measured result;
no benchmark-only runtime or external JSON/CPU replacements are used. Product
compiler/runtime changes are identified by their actual source and binary hashes.

The current formal candidate explicitly selects `--ku-json response`, using the
public `http.json` response path for `/json` and yielding
`application/json; charset=utf-8`; the manifest records this choice and the
expanded source. The build helper's default remains `--ku-json text`, matching
the checked-in template's explicit `json.encode` plus `http.text` diagnostic
variant. The PG
route uses `http.text` in both PG execution modes. Inspect the actual expanded
source and response headers instead of assigning the old template's MIME to
the formal candidate binary.

Normal HTTP cases configure 4,096 connections, 1,024 active requests, 128 pending
requests and 64 MiB accounted HTTP buffers. Dedicated waiting cases write the
same ordinary configuration used by the other stacks: `target+128` connections,
`target+64` active requests and 128 pending requests. These are configured limits,
not a measured capacity; the runner checks the actual protected admission
limits before claiming a wait tier and never calls rejected requests held Tasks.
For wait counts and reclaiming resources,
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
