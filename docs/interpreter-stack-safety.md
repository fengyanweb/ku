# Interpreter bounded call frames and resumable execution

Status: in progress; not a v0.0.18 release or full concurrency acceptance.

## Authorized boundary (2026-09-23)

The user authorized a phased replacement of interpreter host-stack recursion
with bounded heap call frames and resumable execution. Existing syntax, public
APIs, the 512 logical call limit and its counting convention, left-to-right
argument evaluation, move/borrow rules, error precedence, finally behavior and
absolute cleanup deadlines remain unchanged. No implicit clone, borrowed value
across await, larger thread stack, lower call limit or larger timeout is an
acceptable substitute. Native lowering, parser/compiler rewrites, database
changes and a complete scheduler rewrite are outside this increment.

The starting point is `522928e6a4ea886130d1a0b1024f79306c7c76dc` on
`codex/v0.0.18`. Five pre-existing dirty files contain R6.24 owner-cleanup work
and the R6.25 crash oracle. Byte-identical copies, SHA-256 identities and binary
Git diffs are retained locally in
`target/v018-checkpoint/bounded-frames-start-20260923/`; the original crash logs
and detached baseline are retained. Existing changes are not discarded or
automatically folded into a new execution-mechanism commit.

## Confirmed defect and additional code-path risks

The real interpreter Task worker aborts with stack overflow on finite
`Descend(1024)`, before returning `runtime/call_depth_exceeded`. Existing
candidate debug/release and original-HEAD debug records show this failure.
Abort does not prove owner cleanup or a zero-active-task state.

Two further paths must be handled by the design, without claiming they have
already produced a separately observed crash:

* `await_result` and `run_blocking` can synchronously help another Task through
  `help_one_bounded` and `execute_task_job`. A new interpreter resets its logical
  call count, but the parent's Rust stack is still live.
* Dropping a cancelled, unstarted Task's captured frame can recursively drop
  another Task owner. Making expression execution iterative alone does not
  establish bounded cancellation/destruction stack use.

## Reviewable implementation sequence

1. Preserve the old changes and add independent crash-contained regressions.
   The child must exit successfully **and** report the expected value or
   structured error; stack overflow, abnormal exit and watchdog expiry fail.
   Cover real-worker named calls, non-tail return resumption, the exact logical
   limit, over-limit and function values in debug and release. Keep the original
   failing oracle until the replacement has equivalent or stronger coverage.
2. Introduce private bounded frame storage with explicit resume and unwind
   states. Capacity and byte arithmetic, fallible reservation, rejection before
   moving inputs, partial construction and iterative teardown are independently
   tested before switching the production evaluator. A prototype is not a fix.
   Reservation precedes transfer into new storage; it does not undo argument
   moves that have already occurred during source-language evaluation.
3. Move call/argument/expression/block continuations to one iterative driver.
   Preserve the old count checks at named and function-value call admission;
   an entered frame increments once and retires once. Resume records cannot
   invoke a nested driver or retain a recursive `eval`/`exec` fallback for a
   potentially calling expression. Map callbacks, templates, projections,
   assignment targets, match guards and HTTP function values require explicit
   review, not only the ordinary call syntax.
4. Add an internal pending/resume boundary for interpreter await, blocking
   operations and owner drain. A pending parent returns to the outer driver
   rather than recursively running a child. Distinguish unstarted frames from
   suspended started frames: cancelling the latter must execute its applicable
   finally/unwind states. Keep wait-edge/cycle detection and exactly-once
   terminal/accounting rules. A pending outcome is not Task completion.
   Preserve the current 64-layer await-help and insertion-time wait-graph
   checks as distinct contracts; neither counts repeated polls. A single
   physical thread's help depth is not a global bound on the live wait graph.
   Save pure cancellation contexts, not thread-local RAII guards, across a
   suspension. Reinstall/restore those guards for each execution slice. Cover
   completion racing with wait registration, and repeated polls without
   duplicate submission, await claim, expression-entry tick or payload move.
5. Preserve a single carried absolute cleanup deadline across argument, body,
   call and wait resumptions. Cleanup retains and disposes pending payloads in
   the existing order. Test entry rejection, enqueue rejection, partially
   evaluated arguments, successful/error return, cancellation, timeout,
   finally replacement, and exactly-once owned release. Include bounded
   cancellation/destruction work rather than a recursive chain of drop calls.
6. Run focused debug/release tests, full workspace/native tests, then the new
   commit's configured three-system CI, sanitizers and packaging gates. Record
   memory and execution overhead on like-for-like inputs; do not promise a
   speed or memory improvement without measurements.

## Resource and migration constraints

The 512 call count and the internal resume-record/byte budgets are different
limits. New storage must specify what it charges, include retained capacity and
growth peaks, use checked arithmetic and fallible allocation, and return inputs
to their owning unwind path on failure. Nested interpreters on one driver
cannot each silently obtain a fresh unlimited physical-stack budget. Frame
metadata accounting is not an RSS cap or proof that every allocator OOM is a
recoverable Ku error.

Read-only borrowed values currently bind both task identity and worker-thread
identity. Do not introduce cross-worker migration while a synchronous borrow
is live, or remove that check. Continuations must not turn an existing
synchronous borrow into a borrow across await.

The native async subset and its bounded worker group already exist. This
increment concerns interpreter stack safety; completing it does not complete
native async, M:N, netpoll, event-driven HTTP, real-database or production soak
acceptance. Keep the PR draft; no main merge, tag or Release.

## Documentation drift recorded before editing

`docs/self-hosting.md` currently says native async ABI is unsupported, whereas
`docs/concurrency.md` specifies the implemented restricted subset. README's
blanket missing-closure-capture item also overlaps documented implemented
parameter cases, and TLS documents mix old and newer exact-commit evidence.
These are recorded wording conflicts, not newly reproduced runtime failures.
Resolve them against implementation and exact verification records in a
separate documentation change; do not use them to expand this runtime patch.
