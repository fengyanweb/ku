# Optional native compression

`std.zstd`, `std.gzip` and `std.deflate` use the existing `bytes` type in synchronous native C programs. They do not change HTTP compression, the JSON engine or the default allocator. Programs that do not use them do not link these libraries or initialize codec contexts, buffers or threads.

| Module    | Format                                                       | Implementation                       |
| --------- | ------------------------------------------------------------ | ------------------------------------ |
| `zstd`    | Zstandard frames; encoder enables content checksums          | Zstandard 1.5.7, compression level 3 |
| `gzip`    | RFC 1952 gzip members containing DEFLATE                     | zlib 1.4, default level and strategy |
| `deflate` | One raw RFC 1951 DEFLATE stream, without a gzip/zlib wrapper | The same zlib 1.4                    |

Each module provides:

```text
compress(data, max_output_bytes)   -> bytes!
decompress(data, max_output_bytes) -> bytes!
```

Input is read-only: it is not consumed, modified or deeply copied and remains usable after the call. Successful output is independently owned `bytes`; it does not retain pointers into the input or a destroyed codec context. Normal value cleanup releases it. Failure returns a Ku Result, never a successful partial result.

```ku
import { bytes, zstd, gzip, deflate } from "std"

fn main(): null! {
    source = bytes.from_str("中文😀")?
    encoded = zstd.compress(source, 1024)?
    decoded = zstd.decompress(encoded, 1024)?
    println(decoded.to_str()?)
    println(source.to_str()?)
    return ok(null)
}
```

## Bounds and accepted formats

- The explicit output limit must be in `0..67108864` bytes. Input retains the existing 64 MiB `bytes` bound.
- Output grows as needed and never exceeds the supplied capacity limit. A declared uncompressed size never causes an unbounded allocation. Internal scratch space is 32 KiB; at the output limit one extra scratch byte distinguishes a complete footer from excess output.
- A zero limit permits a valid empty decoded frame/member; nonempty output returns `output_limit`.
- Zstandard retains version 1.5.7's default 128 MiB decoding-window limit. Output, window, context, entropy tables, input and allocator overhead are different resources. The output limit is not a whole-process memory limit.
- gzip/raw DEFLATE retain the default 32 KiB window and `memLevel=8`. Matching per-call `zalloc/zfree` callbacks charge library allocations and this module's allocation headers, with a 2 MiB bound. This covers the upstream default configuration's approximately 268 KiB requirement without reducing its window or changing strategy.
- Zstandard accepts complete concatenated frames, including standard skippable frames. gzip accepts complete concatenated members. One cumulative output limit applies; truncation and trailing garbage fail.
- Raw DEFLATE accepts one complete stream and rejects trailing bytes. It has no checksum; a modified but still-valid stream cannot always be detected.
- External dictionaries are unsupported. A library-reported dictionary requirement returns an explicit error.

Common error codes are `invalid_argument`, `output_limit`, `truncated_input`, `invalid_data` and `out_of_memory`. Additional codes include `window_limit`, `workspace_limit`, `dictionary_required` and `library_version_mismatch`. The error domain matches the module. Failed validation never becomes null or success.

## Build and execution scope

These modules currently support synchronous native C calls, with no public streaming handles. Runner/interpreter backends do not implement the corresponding binary `bytes` value and must reject these modules before building. Task calls must also be rejected until the relevant budget, cancellation and deadline adapter exists.

Programs using a codec need matching official headers and libraries for the target platform. Header and linked-library versions must agree. Optional dependency bundles retain Zstandard's BSD-3-Clause license and zlib's zlib license. An old system library or a different architecture is not a compatible substitute; installation must not replace global system libraries.

The CLI discovers optional packs beside its executable under `native-compression/v1/<target>/`. Each `zstd/` or `zlib/` directory contains `lib/`, `include/`, `LICENSE` and `manifest.json`; `<target>` is the full target triple. With the matching pack installed, use the ordinary `ku build --backend c --release main.ku` command, with no codec-specific build option.

For an explicit pack, `KU_ZSTD_LIB` and `KU_ZLIB_LIB` must name its absolute `lib/` directory. Headers default to the sibling `include/`, optionally overridden by the corresponding absolute `KU_ZSTD_INCLUDE` or `KU_ZLIB_INCLUDE`. Sizes, SHA256, target object format, ABI, CRT and versions must match the manifest. Empty/relative settings, missing files or failed checks abort rather than searching older system libraries. Private copies supply the C compiler with verified static archives and headers; the output does not depend on development-machine codec dynamic-library paths.

Static codec linking and whole-program `--static` are separate settings. On Linux, `--static` remains subject to the selected C toolchain and available static system libraries; macOS retains its existing system-linking restrictions. MSVC packs use `/MD` and reject the `/MT` combination requested by `--static`. The CLI selects packs using the actual C compiler ABI, keeping MinGW and MSVC inputs separate. Paths are passed as individual compiler arguments and may contain spaces or Unicode; no shell command or move to an ASCII-only path is required.

Encoded bytes are not promised identical across different formats, platforms or upstream versions. Upstream library benchmarks are not Ku service results.

Stock zlib directly supplies the stable `z_stream`, gzip/raw window and `zalloc/zfree` interfaces used here. zlib-ng also provides a compatibility mode and architecture optimizations, but would require separately frozen compatibility builds and CPU dispatch configuration. Without Ku end-to-end benefit evidence, this interface keeps one implementation. This does not claim zlib is faster or change `ureq`'s transitive compression backend.

References: [Zstandard 1.5.7](https://github.com/meta/zstd/releases/tag/v1.5.7), [zlib 1.4](https://www.zlib.net/), [zlib memory configuration](https://github.com/madler/zlib/blob/v1.4/zconf.h.in), [zlib-ng builds and compatibility](https://github.com/zlib-ng/zlib-ng).
