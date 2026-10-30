# CLI and Runner bundle fix candidate

[简体中文](cli-runtime-bundle.md) · [Complete CLI reference](../web/src/content/docs/en/docs/tools/cli.md)

This describes `codex/v0.0.19-cli-bundle-fix`, an independent fix based on the published 0.0.19 product code. **It is not published and does not replace the original `v0.0.19` tag or assets.** New `doctor` and unified subcommand help apply to the candidate; the original package may reject these entries. The delivery record identifies actual bundles and checks. This document is not an acceptance receipt.

## Existing commands and artifacts

```sh
ku -v
ku create hello
cd hello
ku check
ku run
```

`ku ./hello.ku` equals `ku run ./hello.ku`, accepting exactly one source path. Omitting a project entry requires valid `ku.mod`; bare `ku` does not run a project, and `ku run .` is not a project-directory shorthand. Top-level `ku -v` prints the version; `ku build -v FILE` enables verbose logging. The CLI reference lists all shorthands, profiles, backend aliases and option positions.

| Invocation | Meaning |
| --- | --- |
| `ku build hello.ku` | Default Debug / Runner: a Rust wrapper packages interpreter execution |
| `ku build --backend c --release hello.ku -o hello` | Native C compilation and linking; use `hello.exe` on Windows |
| `ku build --emit-c hello.ku` | Runner by default with additional C output; does not solve mismatched Rust |
| `ku build --backend llvm hello.ku` | Text LLVM IR, without a complete executable-linking path |
| `ku package pack` | Source/dependency distribution, not a program executable |

Historical `ku build --native FILE` emits C only; `-o` in that compatibility form requests C compilation. It is not an ordinary backend alias accepted anywhere. A successful `ku check` does not establish execution or compilation on every backend.

## Why E0514 occurs

The original Runner build collects `libku.rlib` and `deps/libku-*.rlib` near the CLI, chooses by modification time, and invokes PATH `rustc`. It can therefore combine system Rust 1.96 with a Ku library compiled by 1.89. Ancestor target-directory fallbacks can conceal incomplete archives on a development machine.

Rust `rlib` contains compiler-specific metadata; it is not a stable cross-rustc distribution interface. The [official E0514 explanation](https://doc.rust-lang.org/error_codes/E0514.html) requires compatible compiler/dependency identities. Cargo's [rust-version](https://doc.rust-lang.org/cargo/reference/rust-version.html) declares an MSRV rather than choosing a compiler. A user's Rustup files, overrides and default follow their own [selection rules](https://rust-lang.github.io/rustup/overrides.html); Ku cannot rely on them coincidentally selecting its release compiler.

A path in an error alone neither identifies the user's asset build nor proves a hardcoded developer directory. Actual `ku.exe` location, version and SHA evidence are still needed.

## Candidate bundle and toolchain policy

Keep the full extracted tree, including:

- `ku` / `ku.exe`;
- `ku-runtime.json` and `ku-runtime-deps.json`;
- the exact listed `libku.rlib`, `deps` and companion libraries;
- existing native TLS packs, licenses and installation instructions.

The actual CLI location determines its installation root. Relative manifest paths identify the same-build CLI, runtime and dependencies, with sizes and SHA checks. The candidate does not choose by mtime, fall back to ancestor target trees or borrow a Ku runtime from the project directory. Missing manifests, invalid paths, missing/corrupt files and mixed bundles fail explicitly. Relocating the whole bundle is supported; copying its executable alone cannot preserve Runner builds.

[`scripts/release-toolchain.json`](../scripts/release-toolchain.json) is the central release-toolchain configuration. This hotfix retains verified **T=1.89.0** and its native/TLS link contracts; source MSRV and release T are distinct. A future T change requires rebuilding CLI, rlib, dependencies and affected packs together, followed by consumption tests. Users are not asked to permanently downgrade their global Rust default.

Before compiling, the candidate checks the full compiler release/commit/host and runtime target ABI. A matching standalone system rustc is usable; otherwise an already-installed exact Rustup toolchain T is selected. Probe and compilation use the same resolved compiler, rather than probing one compiler and invoking another through PATH.

Explicit `KU_RUNNER_RUSTC` must name an **absolute compiler path**, taking priority over automatic selection. A mismatch fails instead of being silently ignored. The candidate does not modify global defaults, directory overrides or project `rust-toolchain.toml`, delete other Rust versions, or clear user Cargo caches. Read-only probes disable Rustup auto-installation; only explicit installation requests download tools.

## Windows diagnosis and the fix candidate

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

With a new candidate bundle:

```powershell
ku doctor
ku doctor --json
ku doctor --backend runner
ku doctor --backend c
```

These operations are read-only by default. Basic installation and optional backends are separate: absent C tools can be WARN rather than making Ku unusable. Explicit Runner/C checks fail when that backend's requirements are unmet. JSON mode writes structured diagnostics only to stdout; ordinary help is not a diagnostic JSON stream.

If the matching toolchain is absent and Rustup is available, explicitly install it and repeat the original command:

```powershell
ku doctor --install-runner-toolchain
ku build .\hello.ku
```

Installation requests only the bundle's required exact toolchain without changing the system default. Failure does not mark Runner ready. Use the output path reported by the CLI; default output is not necessarily `hello.exe` in the current directory. An explicit output is also supported:

```powershell
ku build .\hello.ku -o ".\dist\hello.exe"
& ".\dist\hello.exe"
```

Validate C separately rather than using it as evidence for a default Runner fix:

```powershell
ku build --backend c --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

## Updates and failures

Extract a new bundle into a separate empty directory, verify it, then switch PATH. Do not combine a new EXE with old rlib/deps. Preserve the old directory for rollback. Installations can be read-only; program-build temporary files do not belong in the installation root. On Windows, stop the relevant Ku process before switching an in-use installation to avoid a partial update.

Missing matching compilers, linkers or target standard libraries need clear pre-compilation diagnostics. Finding `rustc` alone does not establish readiness. Restore a full verified bundle for missing/corrupt dependencies rather than copying one rlib from another version. If E0514 still occurs, retain detailed rustc errors and doctor identities for investigation; do not loop through compilers or increase timeouts.

Unified candidate help includes `ku help build`, `ku build --help` and `ku run --help` for public commands. Help exits without executing programs, initializing projects or building. Unknown commands/options fail. Existing `service` / `supervise` help remains available.

## Separate from performance acceptance

This usability fix starts from released product code; it does not automatically adopt the entire performance development branch. Latest performance evidence retains its `cbd158d` candidate and original inputs: 80 valid fixed windows, 1,209,000 correct responses, and only 1 of 16 CPU/memory targets met. JSON, strict-PG and steady memory.current gaps, capacity upper bounds, and CPU legacy-blocking / Task contract differences remain.

Bundle consumption, doctor and help checks do not establish the Rust-comparison goal. CLI/documentation changes do not justify rerunning all valid 80-window/V/C2 evidence. Website source changes and packaging do not imply deployment; documentation changes do not retrofit old public downloads.
