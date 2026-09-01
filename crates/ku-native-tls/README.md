# ku-native-tls

`ku-native-tls` is Ku's bounded, socket-free native TLS client state machine.
It is built as a Rust `staticlib` and exposes only the versioned
`ku_tls_v1_*` C ABI declared in `include/ku_native_tls.h`.

There is one integration loop:

1. Create a shared config with either compiled WebPKI roots or a replacement
   custom PEM bundle, then create a session for the verified server name.
2. While `wants_write` is true, drain ciphertext into a non-empty 64 KiB-or-less
   buffer and write exactly the returned byte count to the caller-owned socket.
3. Only while `wants_read` is true, read at most 64 KiB from that socket, feed
   a non-empty byte slice, and call `process` exactly once after every feed.
4. Read or queue plaintext using the bounded plaintext functions. Partial
   progress is normal and the returned byte counts are authoritative.
5. On socket EOF call `notify_eof`; success requires an authenticated peer
   `close_notify`. To close locally, queue `send_close_notify`, drain it, then
   drop the session.

`WOULD_BLOCK` means the caller must first make the progress indicated by the
`wants_read`/`wants_write` state. Empty feeds and plaintext writes, plus
zero-capacity drain/read buffers, are `INVALID_ARGUMENT`, so write-side progress
cannot succeed with zero bytes. An authenticated clean peer close is reported
as `OK` with a zero-byte plaintext read and `peer_closed` set. TLS,
handshake-limit, and truncation failures are terminal for
feed/process/plaintext operations; a queued fatal TLS alert may still be
drained before dropping the session. Rejecting an oversized single API input
does not by itself poison an otherwise healthy session.

The crate never owns a socket and contains no retry, polling, sleep, plaintext
fallback, system-root lookup, or verification-bypass path. See
`THREAT_MODEL.md` for the remaining raw-pointer and allocator boundaries.

## Static linking

The archive is not the complete final link input. Target-pack creation must run
`cargo rustc --release -p ku-native-tls -- --print native-static-libs` with the
exact target toolchain and preserve the reported library order. Do not guess or
silently omit that platform list. With Rust 1.89 on `x86_64-pc-windows-msvc`,
the reported inputs are `bcrypt.lib`, `advapi32.lib`, `kernel32.lib`,
`ntdll.lib`, `userenv.lib`, `ws2_32.lib`, `dbghelp.lib`, and
`/defaultlib:msvcrt`; the C consumer must use the matching dynamic MSVC CRT.
Linux and macOS have different lists and require native CI evidence before their
target packs can be called verified.
