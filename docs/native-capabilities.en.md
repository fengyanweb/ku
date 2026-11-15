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
