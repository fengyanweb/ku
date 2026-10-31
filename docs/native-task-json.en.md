# JSON local values in native Tasks

[简体中文](native-task-json.md)

The native C backend can hold values returned by `json.decode()` in local variables of an `async fn`. JSON owners survive `await` and timer waits, and are released during return, error, cancellation and deadline cleanup. Synchronous functions keep the existing JSON engine and API.

The [complete example](../examples/task_json.ku) decodes Chinese text, emoji, nested objects and arrays, waits on a real timer, transfers a subtree and encodes compact JSON:

```sh
ku build --backend c --profile release examples/task_json.ku -o task-json
./task-json
```

On Windows, use the output name `task-json.exe` and run `.\task-json.exe`.

The same operations work in a named `async fn handler(req: HttpRequest): HttpResponse!` registered with `app.post("/json", handler)`. Results from `json.decode`, string-key indexing, `take`, `as_int`, `as_str` and `json.encode` retain the existing `?` and `try/catch` rules. There is no implicit unwrapping.

## Ownership and accounting

- `json.encode(value)` borrows the value; it remains usable afterward. `value["key"]` returns an independently owned value.
- `value.take("key")` consumes the original root and transfers the selected subtree. `as_int()` and `as_str()` consume their conversion input. Do not reuse a consumed variable.
- Object construction moves dynamic JSON fields under the existing ownership rules. Failed construction also cleans up consumed fields. Functions, Tasks and resource handles are not JSON fields.
- Operations prepay their Task byte reservation before allocating and refund only after physical release. This ledger covers Ku-requested heap bytes, not process RSS, allocator internals or all third-party memory.

Parsing and encoding use the existing strict JSON engine, a depth limit of 32 and input/output limits of 1,000,000 bytes each. Insufficient budget returns an explicit error. An individual operation is size-bounded but does not suspend internally; this is not instruction-level preemption.

## Supported scope

Local dynamic values can hold, transfer and re-encode JSON objects, arrays, strings, numbers, booleans and null. A dynamic object literal constructed inside a Task supports at most 64 fields; decoded objects retain the existing parser rules. Arrays may come from decoding; this does not add integer indexing of dynamic JSON or heterogeneous array literals to native Tasks. Object operations include string-key indexing, `take`, `as_int` and `as_str`. No `.get()` method was added to KuValue.

Task parameters, captures and public Task outcomes were not extended to arbitrary JSON. Native Task floating-point locals and other operations remain subject to the existing type subset. Unsupported programs are rejected at compile time. Consult the separate interpreter, synchronous C and LLVM documentation for their own capabilities.

The permanent `native_task_json_test` regression reads the example directly. Other regressions cover allocation failures, prepaid budgets, cancellation, deadlines, suspended owners and actual HTTP execution.
