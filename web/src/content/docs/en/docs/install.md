---
title: Install and download
description: Download the Ku 0.0.18 experimental prerelease and understand platform and build requirements.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Choose your platform

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| System  | Architecture          | Archive                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

No other platform archives have been verified. Cross compilation support does not mean every operating system or architecture has a published package.

## Extract and verify

1. Verify the archive against the Release `SHA256SUMS`. A separate `.manifest.json` describes archive and file integrity. Checksums are integrity checks, not publisher signatures.
2. Extract the **entire archive** into an empty directory and read `README-INSTALL.md`. Preserve the relative layout of `ku`, `libku.rlib`, `deps/`, and `native-tls/`.
3. Check the version from that directory. On macOS / Linux:

```sh
./ku --version
```

On Windows PowerShell:

```powershell
.\ku.exe --version
```

The reported version should be `0.0.18`. Optionally add this directory to `PATH`, then run `ku --version`. Install the bundled `ku-language-0.0.18.vsix` for VS Code integration.

## Runtime and build requirements

| Operation                               | Additional requirements                                                                    |
| --------------------------------------- | ------------------------------------------------------------------------------------------ |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI for the matching platform                                                           |
| Default `ku build hello.ku`             | Rust **1.89.0** for the target and its system linker; retain the bundled Rust dependencies |
| `ku build --backend c hello.ku`         | A working target C compiler and linker                                                     |
| Native database program                 | Matching external database client dynamic libraries; they are not bundled                  |

The default runner embeds only the entry source. Imported modules still load from their original locations. Source independent deployment of a complete local import graph requires native C compilation and remains subject to the [backend support limits](/en/docs/support/).

## Try the development source

From a local checkout of the repository:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Uninstall

For an archive installation, remove the directory you extracted, undo any `PATH` entry you added, and uninstall the VS Code extension. Preserve any source files and configuration you created there first. No separate one click installer, package manager installation, or `ku uninstall` command has been verified.

Next: [run your first program](/en/docs/getting-started/). Source for the installation contract: [release archives](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/en/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
