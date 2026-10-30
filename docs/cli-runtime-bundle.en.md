# Runner bundles and toolchains

[简体中文](cli-runtime-bundle.md) · [Complete CLI reference](../web/src/content/docs/en/docs/tools/cli.md)

Development API reference; unreleased.

## Why E0514 occurs

Rust `rlib` contains compiler-specific metadata; it is not a stable cross-rustc distribution interface. The [official E0514 explanation](https://doc.rust-lang.org/error_codes/E0514.html) requires compatible compiler/dependency identities. Cargo's [rust-version](https://doc.rust-lang.org/cargo/reference/rust-version.html) declares an MSRV rather than choosing a compiler. A user's Rustup files, overrides and default follow their own [selection rules](https://rust-lang.github.io/rustup/overrides.html); Ku cannot rely on them coincidentally selecting its release compiler.

## Bundle manifest and toolchain

Keep the full extracted tree, including:

- `ku` / `ku.exe`;
- `ku-runtime.json` and `ku-runtime-deps.json`;
- the exact listed `libku.rlib`, `deps` and companion libraries;
- existing native TLS packs, licenses and installation instructions.

The actual CLI location determines its installation root. Relative manifest paths identify the same-build CLI, runtime and dependencies, with sizes and SHA checks. Resolution does not choose by mtime, fall back to ancestor target trees or borrow a Ku runtime from the project directory. Missing manifests, invalid paths, missing/corrupt files and mixed bundles fail explicitly. Relocating the whole bundle is supported; copying its executable alone cannot preserve Runner builds.

Release toolchain **T=1.89.0** is configured in [`scripts/release-toolchain.json`](../scripts/release-toolchain.json), separately from the source MSRV. Changing T requires rebuilding the CLI, rlib, dependencies and affected native/TLS packs together.

Before compiling, Ku checks the full compiler release/commit/host and runtime target ABI. A matching standalone system rustc is usable; otherwise an already-installed exact Rustup toolchain T is selected. Probe and compilation use the same resolved compiler, rather than probing one compiler and invoking another through PATH.

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
ku build .\hello.ku
```

Installation requests only the bundle's required exact toolchain without changing the system default. Use the output path reported by the CLI; default output is not necessarily `hello.exe` in the current directory. An explicit output is also supported:

```powershell
ku build .\hello.ku -o ".\dist\hello.exe"
& ".\dist\hello.exe"
```

Native C build:

```powershell
ku build --backend c --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

## Updates and failures

Extract a new bundle into a separate empty directory, verify it, then switch PATH. Do not combine a new EXE with old rlib/deps. Preserve the old directory for rollback. Installations can be read-only; program-build temporary files do not belong in the installation root. On Windows, stop the relevant Ku process before switching an in-use installation to avoid a partial update.

Missing matching compilers, linkers or target standard libraries need clear pre-compilation diagnostics. Finding `rustc` alone does not establish readiness. Restore a full verified bundle for missing/corrupt dependencies rather than copying one rlib from another version. If E0514 still occurs, retain detailed rustc errors and doctor identities for investigation; do not loop through compilers or increase timeouts.

Subcommand help includes `ku help build`, `ku build --help` and `ku run --help` for public commands. Help exits without executing programs, initializing projects or building. Unknown commands/options fail. Existing `service` / `supervise` help remains available.
