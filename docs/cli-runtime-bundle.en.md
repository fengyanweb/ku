# CLI, SDK and build toolchains

[简体中文](cli-runtime-bundle.md) · [Complete CLI reference](../web/src/content/docs/en/docs/tools/cli.md)

This document describes bundle layouts, build dependencies and upgrade requirements. Current source builds default to native C; published 0.0.20 packages retain their Runner default. Runner diagnostics and examples below select `--backend runner` explicitly; `ku run` is unchanged.

## Why E0514 occurs

Rust `rlib` contains compiler-specific metadata; it is not a stable cross-rustc distribution interface. The [official E0514 explanation](https://doc.rust-lang.org/error_codes/E0514.html) requires compatible compiler/dependency identities. Cargo's [rust-version](https://doc.rust-lang.org/cargo/reference/rust-version.html) declares an MSRV rather than choosing a compiler. A user's Rustup files, overrides and default follow their own [selection rules](https://rust-lang.github.io/rustup/overrides.html); Ku cannot rely on them coincidentally selecting its release compiler.

## Bundle manifest and toolchain

Keep the full extracted tree, including:

- `ku` / `ku.exe`;
- `ku-runtime.json` and `ku-runtime-deps.json`;
- the exact listed `libku.rlib`, `deps` and companion libraries;
- existing native TLS packs, licenses and installation instructions.

The actual CLI location determines its installation root. Relative manifest paths identify the same-build CLI, runtime and dependencies, with sizes and SHA checks. Resolution does not choose by mtime, fall back to ancestor target trees or borrow a Ku runtime from the project directory. Missing manifests, invalid paths, missing/corrupt files and mixed bundles fail explicitly. Relocating the whole bundle is supported; copying its executable alone cannot preserve Runner builds.

Release toolchain **T=1.99.1** is configured in [`scripts/release-toolchain.json`](../scripts/release-toolchain.json), separately from the source MSRV. Changing T requires rebuilding the CLI, rlib, dependencies and affected native/TLS packs together.

Before an explicit Runner build, Ku checks the full compiler release/commit/host and runtime target ABI. A matching standalone system rustc is usable; otherwise an already-installed exact Rustup toolchain T is selected. Probe and compilation use the same resolved compiler, rather than probing one compiler and invoking another through PATH. Ordinary native C builds do not read those rlibs or require T.

Explicit `KU_RUNNER_RUSTC` must name an **absolute compiler path**, taking priority over automatic selection. A mismatch fails instead of being silently ignored. The command does not modify global defaults, directory overrides or project `rust-toolchain.toml`. Read-only probes disable Rustup auto-installation; only explicit installation requests download tools.

## Windows diagnosis

Windows MSVC builds require the Visual Studio Build Tools C++ tools and Windows SDK; Rust toolchain installation does not install those components.

Collect command/toolchain identity without dumping environment variables or project content:

```powershell
Get-Command ku -All
where.exe ku
ku -v
rustc -vV
rustup show active-toolchain
rustup toolchain list
```

Missing Rust/Rustup makes those commands fail; neither is required for Ku interpretation. Paths can contain usernames, so redact personal locations before sharing.

Diagnostic commands:

```powershell
ku doctor
ku doctor --json
ku doctor --backend runner
ku doctor --backend c
```

Diagnostics are read-only by default. Unrequested optional backends report WARN when unavailable; an explicitly requested backend fails if its requirements are unmet. `--json` writes structured diagnostics only to stdout.

If the matching toolchain is absent and Rustup is available, explicitly install it and repeat the original command:

```powershell
ku doctor --install-runner-toolchain
ku build --backend runner .\hello.ku
```

Installation requests only the bundle's required exact toolchain without changing the system default. Use the output path reported by the CLI; default output is not necessarily `hello.exe` in the current directory. An explicit output is also supported:

```powershell
ku build --backend runner .\hello.ku -o ".\dist\hello.exe"
& ".\dist\hello.exe"
```

The current source's default native build:

```powershell
ku build --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

To explicitly select C in both current source builds and older installed packages:

```powershell
ku build --backend c --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

## Native C build and deployment dependencies

Current source builds with no `--backend`, or with `--backend c`, use the target C compiler and linker; `--backend native-c` is an alias. Missing compilers, dependencies and unsupported programs fail explicitly without switching to Runner. Building an ordinary C program does not read `libku.rlib` or invoke Rust to create the program; this does not change `ku run` or direct `.ku` execution. Use `ku doctor --backend c` to inspect the C toolchain. That check does not establish that database libraries or other optional native dependencies are available.

For native `net.client` TLS, retain `native-tls/v1/<target>/` beside `ku`. The CLI discovers the matching target pack and validates its manifest, header and static library. Moving the whole installation does not require resetting its path. The prebuilt TLS library is linked into the executable: deployment does not require the Ku installation or TLS pack, but target CRT and system networking dependencies remain.

Current source packaging uses `MACOSX_DEPLOYMENT_TARGET=11.0` consistently for the macOS CLI, TLS and optional compression libraries, preventing dependencies from accidentally requiring a newer system than the executable. Build scripts restore the caller's environment afterwards. A minimum link target does not establish runtime validation on that older system; platform results refer to the systems actually tested.

Packages providing the optional compression modules place their dependencies in `native-compression/v1/<target>/` for automatic discovery by the synchronous C `bytes`, `zstd`, `gzip` and `deflate` paths; see the [native compression contracts](compression.en.md). Only programs using a codec link it. Do not rename and mix libraries or packs from another version into older installations that lack these modules.

Database client shared libraries are external to Ku bundles. PostgreSQL uses the absolute directory `KU_PG_LIB`; MySQL/MariaDB uses `KU_MYSQL_LIB` and matching headers. CPU architecture, target ABI and client-library family must match the build target. Deployment still requires compatible client libraries and their dependencies; `--static` does not bypass these checks. A successful C compile, cross-build or source review is not evidence that the result ran on its target machine.

## Updates and failures

Extract a new bundle into a separate empty directory, verify it, then switch PATH. Do not combine a new EXE with old rlib/deps. Preserve the old directory for rollback. Installations can be read-only; program-build temporary files do not belong in the installation root. On Windows, stop the relevant Ku process before switching an in-use installation to avoid a partial update.

Missing matching compilers, linkers or target standard libraries need clear pre-compilation diagnostics. Finding `rustc` alone does not establish readiness. Restore a full verified bundle for missing/corrupt dependencies rather than copying one rlib from another version. If E0514 still occurs, retain detailed rustc errors and doctor identities for investigation; do not loop through compilers or increase timeouts.

Subcommand help includes `ku help build`, `ku build --help` and `ku run --help` for public commands. Help exits without executing programs, initializing projects or building. Unknown commands/options fail. Existing `service` / `supervise` help remains available.

## Native calendar library

SDKs built from current source include a matching static calendar library, ABI header and license materials under `native-calendar/v1/<target>/`. The original public 0.0.20 packages do not contain these new files.

The C backend uses this library to build `time.date/datetime/format/parse/parts/weekday`, sharing the interpreter's calendar engine without invoking an interpreter or helper process. Keep the complete SDK layout for automatic discovery; a Rust installation is not required to build these C programs. Missing packs and mismatched targets or source identities produce errors. Advanced use may set the absolute `KU_NATIVE_CALENDAR_PACK` path to a complete matching target pack.

A linked executable no longer needs the SDK, Ku source or static archive at runtime. Other module dependencies remain separate, including database shared libraries. Programs that do not call these calendar APIs do not require the pack. Time zones are local time, UTC and fixed offsets; no named time-zone database is included.
