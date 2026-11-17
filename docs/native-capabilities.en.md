# Native C capabilities and platforms

[中文](native-capabilities.md)

Native C uses a target C compiler to produce a standalone program, without a Runner rereading Ku sources. Current source builds default to native C; published 0.0.20 packages retain their Runner default. Choose a backend explicitly across versions:

```sh
ku build --backend c --release hello.ku -o hello
ku build --backend runner --release hello.ku -o hello-runner
```

Emitting C, linking an executable and running it on the target are separate steps. The compatibility command `ku build --native hello.ku`, without `-o`, only emits C.

## Existing capabilities

This table describes the native backend, rather than promising every interpreter feature. Regression links identify concrete behavior; existing tests do not establish that every development combination has passed platform acceptance.

| Capability                  | Scope and regressions                                                                                                                                                                                                                                               |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Types and operations        | Checked integers, floats, booleans, null, strings and comparisons; [integer boundaries](../tests/native_sync_integer_matrix_test.rs).                                                                                                                               |
| Containers and custom types | Arrays, dynamic objects, tagged values, nonrecursive struct/enum layouts and complex Results; [collections](../tests/native_collection_test.rs), [aggregate lifetimes](../tests/native_sync_aggregate_owners_test.rs).                                              |
| Functions and modules       | Function values, capturing closures, imports and existing generic specialization; [closure cleanup](../tests/native_closure_drop_test.rs), [generic imports](../tests/native_generic_import_test.rs).                                                               |
| Ownership and errors        | Owned values, read-only borrowing, explicit cloning, `?` and `try/catch/finally`; [borrowing](../tests/borrow_native_test.rs), [exit cleanup](../tests/native_cleanup_escape_test.rs).                                                                              |
| Files and time              | Native `std.fs`/`std.time` and bounded directory listing; [file, time and JSON regressions](../tests/native_run_test.rs).                                                                                                                                           |
| JSON                        | Synchronous DOM/typed APIs and the existing default engine; local Task JSON can survive `await`. See [Task JSON](native-task-json.en.md) for its exact subset.                                                                                                      |
| Tasks                       | The restricted source Task path, suspension, timers, cancellation, deadlines and resource accounting; [concurrency contract](concurrency.md), [Task regressions](../tests/native_task_source_test.rs). This is not complete async/await or complete M:N scheduling. |
| HTTP                        | Synchronous and supported named async handlers, persistent connections, bounded pipelining and cooperative timeouts; [HTTP regressions](../tests/native_http_test.rs).                                                                                              |
| Databases                   | Native PostgreSQL, typed PG, Redis and MySQL clients; pools, independent result lifetimes and cleanup follow the [protocol contract](protocol-foundation.md). Business SQL is not automatically retried.                                                            |
| Networking and TLS          | Owned bytes, move-only `net.client`, TLS clients and target-matched TLS packs; [installation requirements](cli-runtime-bundle.en.md), [network regressions](../tests/native_net_test.rs).                                                                           |

Read-only borrowing does not consume the original value, and a borrow cannot outlive its owner. Cleanup covers returns, errors, cancellation and partial initialization. Generated C helpers are internal implementations, not a stable ABI for arbitrary concurrent third-party calls.

Current source supports synchronous `time.unix()` and `time.unix(time.instant())`, returning Unix seconds as an integer without consuming the Time value. Millisecond conversion truncates toward zero, matching the interpreter: `-1001` milliseconds becomes `-1` second. An ordinary dynamic object cannot substitute for a native Time.

Synchronous time APIs also support `from_unix`, `from_millis`, `duration`, `add/sub/diff/compare`, `is_leap`, `days_in_month`, and integer/Duration `sleep`. Time and Duration have distinct tags. Conversion and arithmetic preserve their inputs; Duration is copyable and differences may be negative. `duration`, `days_in_month`, and `sleep` retain Result errors. Failures from bare-value constructors or arithmetic remain runtime errors and retire native frame owners before propagation. `from_unix` checks multiplication overflow; `from_millis` additionally validates the calendar range.

Current source also supports synchronous native C `date`, `datetime`, `format`, `parse`, `parts`, and `weekday`. The interpreter and native ABI share the pinned Chrono calendar engine. Building uses a static `native-calendar/v1/<target>/` pack matched to the CLI, target and source; no interpreter, Runner or auxiliary process executes these operations. The linked program does not require that pack, an SDK, rustc or Ku sources at runtime.

Existing return conventions are preserved: `date()` returns Date; parameterized `date`, `datetime` and `parse` return Results. Single-argument `format(Time)`, `parts(Time)` and `weekday` return bare values, while format/zone overloads return Results. Bare-value failures remain runtime errors; Result failures retain the `time` error domain. Inputs remain readable and unchanged. Returned strings, Date and parts objects own their storage independently of inputs or temporary engine allocations. `SSS` means three millisecond digits; separators belong to the layout, so `ss.SSS` produces `00.123`.

Zones remain local time, UTC and fixed offsets; no named-zone database is added. These are synchronous capabilities and do not expand native Task time calls or types that survive `await`. Platform and toolchain requirements are listed below.

Synchronous arrays also gain native `concat`, `first` and `last`. `concat` returns an independently owned array. The static return type of `first/last` is `T | null`: empty arrays return null, and present results remain valid independently of the source array. Test `if (value != null)` before element operations. These are not Results and do not accept `?`. Inputs are neither modified nor consumed; this does not expand user borrowed-parameter support. Tasks support these three operations within their existing flat int/bool/str array subset; owned string results can survive `await`. This does not enable arbitrary nested containers or custom structs in Tasks. See [array APIs](syntax.md#125-array).

## Capabilities added in current source

The following capabilities passed normal C11 O2 functional regressions and real CLI/SDK validation on Linux x86_64, macOS Apple Silicon and Windows x86_64. They are included in current source, not in the original public 0.0.20 packages.

| Capability                                                                   | Validation entry                                                                      |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| HTTP status constants, independently owned constant objects and `statusText` | [Status regressions](../tests/native_http_status_test.rs).                            |
| Unicode `trim`/`lower`/`upper`                                               | [Unicode, borrowing and interrupted cleanup](../tests/native_unicode_string_test.rs). |
| Synchronous nested-loop `break`/`continue`                                   | [Finally order, loop cleanup and deadlines](../tests/native_loop_control_test.rs).    |
| Optional Zstandard, gzip and DEFLATE APIs                                    | [Output limits, invalid input and cleanup](../tests/native_compression_test.rs).      |

Validation covers functionality, limits, invalid input, ownership and failure cleanup. It does not establish that performance targets or sustained-load acceptance passed. Supported Tasks already have separate loop-control lowering; the synchronous changes do not expand Task types or their finally subset.

## Platforms and deployment

The 0.0.20 release targets Linux x86_64 GNU, macOS Apple Silicon and Windows x86_64 MSVC. Existing Linux ARM64 build/link coverage is retained separately; it does not establish an available release package for that architecture. Every target needs its matching C toolchain, system libraries and optional dependencies. Successful cross-generation is not target execution.

Ordinary native C programs do not use `libku.rlib`. TLS packs must match the target and CLI. Database client shared libraries and their dependencies remain deployment requirements. See [CLI/SDK](cli-runtime-bundle.en.md).

Recursive value layouts, object destructuring, optional chaining, captured `for` iteration bindings and standard-library functions without native implementations are still rejected. Task parameters, results, captures and finally blocks retain their documented subset restrictions. Unsupported features or missing dependencies must produce errors, without silently switching to the Runner.

The source default does not imply that every interpreter feature or performance goal is complete. Capability, ownership, platform, installation and real-project acceptance retain their own evidence requirements, as do separate performance changes. Existing `ku run` and direct `.ku` execution behavior remains unchanged.

## Synchronous HTTP client

`http.get(url)`, `http.post(url, body)` and `http.request(config)` return `HttpResponse!` in synchronous native programs. Explicit `HttpResponse` parameters and returns support the same public `status`, `headers` and `body` fields as the interpreter. Results own their data independently of connection reuse. Cloning produces an independent response; reading `response.headers` yields an independent map, while `response.headers[key] = value` changes that response. Printing and JSON expose only these three public fields.

Non-2xx responses remain successful results. Transport, UTF-8 and response-size failures return errors. The default total request timeout is 5000 ms and body limit is 1,000,000 B; maxima are 300,000 ms and 16 MiB. An earlier existing HTTP-handler deadline caps the call, including redirects, and normal deadline cleanup runs immediately after return. Blocking DNS/network calls cannot guarantee immediate cancellation at every point. Native Tasks, including calls through ordinary wrappers, continue to reject synchronous outbound HTTP. The existing server Task response subset does not expand to arbitrary dynamic headers.

`http.client(config)` remains a configuration object, rather than a separate connection-pool handle. Shortcuts use the existing lazily initialized shared agent. Only programs using a network method link the matching `native-http/v1/<target>` static pack. Development builds can select it with an absolute `KU_NATIVE_HTTP_CLIENT_PACK` path. Missing or mismatched target/source packs fail explicitly without a Runner fallback.

See [the loopback example](../examples/native_http_client.ku). The installed-SDK consumer links calendar and HTTP archives together, then runs the executable with its source and SDK unavailable. Each target still requires its own actual installed-runtime result; cross-compilation alone is not runtime acceptance.
