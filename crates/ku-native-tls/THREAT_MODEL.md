# Ku native TLS v1 threat model

This crate is a TLS state machine, not a socket implementation. The caller owns
all sockets, deadlines, cancellation, and scheduling. No exported function does
network I/O or waits for an external event.

Trust is deliberately narrow:

- `KU_TLS_ROOTS_WEBPKI` uses the WebPKI root set compiled into this crate.
- `KU_TLS_ROOTS_CUSTOM_PEM` completely replaces that set with the supplied PEM.
- The operating-system root store is never read.
- Certificate verification and hostname verification cannot be disabled.
- There is no plaintext fallback.

Untrusted input is bounded before it is read or retained. Custom CA input is at
most 4 MiB, at most 1024 certificates, and at most 64 KiB DER per certificate.
Each ciphertext or plaintext operation is at most 64 KiB. A handshake accepts at
most 1 MiB of ciphertext and 4096 processing iterations. Rustls's outgoing
plaintext/ciphertext buffers are limited to 64 KiB. At most one 64 KiB input
fragment can be pending for processing, and rustls applies backpressure while
received plaintext remains unread. The resumption cache is limited to 64 server
names, and callers must process each ciphertext fragment before feeding another
one.

Every exported entry point that executes fallible logic catches Rust panics and
returns a stable numeric status; the ABI-version query only returns a constant.
Allocation failure can still abort the process because Rust's global allocator
is infallible; the input and buffering limits reduce that exposure but do not
turn allocator exhaustion into a recoverable ABI error.

`KU_TLS_STATUS_PANIC` is terminal for any existing config or session involved in
that call: callers may invoke its matching drop once, but must not otherwise use
the handle again. A panic fence prevents unwinding across C; it does not promise
that partially mutated rustls state remains usable. If a drop itself returns
`PANIC`, ownership is indeterminate and the pointer must be discarded without a
second drop attempt.

Opaque pointers must come from this ABI, must not be used after their matching
drop call, and a client session must not be accessed concurrently. Input/output
pointers must be valid for their stated lengths and must not overlap live opaque
handles or writable ABI outputs. Those requirements cannot be proven from a C
pointer. A configuration may be shared by concurrent session-construction calls,
but its drop must be externally synchronized after those calls return.
Configurations may be dropped immediately after session creation because
sessions retain their own reference-counted rustls configuration.
