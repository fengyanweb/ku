---
title: 'Versions and release notes'
description: 'Versions and release notes'
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources: ['docs/history.md', 'docs/v0.0.17.md', 'docs/v0.0.18.md']
---

## CLI fix candidate in development

An independent candidate fixes Runner compiler/dependency identity and adds `ku doctor` plus unified subcommand help. Existing shorthands are documented in the [CLI reference](/en/docs/tools/cli/). **It is not published; 0.0.19 downloads below remain the original assets.** The fix does not overwrite old tags or automatically adopt the performance candidate. See [Runner troubleshooting](/en/docs/guide/errors/#runner-build).

## 0.0.19

Ku 0.0.19 is an experimental prerelease for controlled evaluation, with matching CLI and VS Code extension versions. It collects the implemented native Task, HTTP, PostgreSQL and service-management work, keeps the previous JSON default, and uses Apache-2.0. Complete M:N, general I/O coverage and the full database platform matrix remain incomplete. Historical HTTP 504s and cancellation-related latency tails remain documented; this release does not claim completed long-term or production acceptance.

[Downloads and checksums](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/en/docs/install/) · [Full notes](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues](/en/docs/support/#known-issues)

The three-platform release gates and downloaded-package checks have passed; packages and checksums are available in the Release. Native programs should be rebuilt after upgrading; package targets remain macOS ARM64, Linux x64 and Windows x64.

## 0.0.18

Ku 0.0.18 is an experimental release. It includes limited native Tasks, timers and background I/O, HTTP/1.1 connection reuse, JSON encoding and decoding, and explicit whole-program limits through `ku supervise`.

This edition includes fixes for native C floating-point literals and large-float builds, single evaluation of call-chain receivers, Result checking for `time.date(Time)`, and VS Code version lookup and completion. Recompile affected native programs after updating. Reinstall an older `ku-language-0.0.18.vsix` even if VS Code shows the same extension version.

[Install Ku](/en/v0.0.18/docs/install/) · [Download 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Supported platforms and known issues](/en/v0.0.18/docs/support/)

## v0.0.17

Synchronous read-only borrowing, braced while in the self-hosted parser subset, and cross-platform test tooling fixes. See its [release notes](https://github.com/fengyanweb/ku/releases/tag/v0.0.17); historical APIs may differ from the current version.

## Early development records

These 0.0.1–0.0.16 records predate the new website. Links are pinned to historical files preserved in the 0.0.17 tag. They are development records, not a claim of a GitHub Release or downloadable archive for each version. Old syntax may differ from the current version.

| Version | Scope                                                                      | Original record                                                         |
| ------- | -------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| 0.0.16  | Native libraries, database clients, registry and target-specific packaging | [0.0.16](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.16.md) |
| 0.0.15  | Object destructuring, HTTP call rules and examples                         | [0.0.15](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.15.md) |
| 0.0.14  | Project templates, HTTP status helpers and anonymous handlers              | [0.0.14](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.14.md) |
| 0.0.13  | Unified build, project entries, time APIs and editor support               | [0.0.13](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.13.md) |
| 0.0.12  | HTTP responses, connection reuse, file writes and strict bool              | [0.0.12](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.12.md) |
| 0.0.11  | Result ABI, finally CFG and file-dependency cache                          | [0.0.11](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.11.md) |
| 0.0.10  | Runtime captures, Result CFG and native control flow                       | [0.0.10](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.10.md) |
| 0.0.9   | Typed temporary IR, lockfiles and native C prototype                       | [0.0.9](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.9.md)   |
| 0.0.8   | Closure capture groundwork and typed CFG IR                                | [0.0.8](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.8.md)   |
| 0.0.7   | Fallible collection access, package boundaries and IR                      | [0.0.7](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.7.md)   |
| 0.0.6   | Standard library and compiler module organization                          | [0.0.6](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.6.md)   |
| 0.0.5   | Result, try/catch/finally and ?                                            | [0.0.5](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.5.md)   |
| 0.0.4   | Closures, match and compound-value updates                                 | [0.0.4](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.4.md)   |
| 0.0.3   | Arrays, structs, enums, objects and module imports                         | [0.0.3](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.3.md)   |
| 0.0.2   | Function values, arrow functions, templates and for                        | [0.0.2](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.2.md)   |
| 0.0.1   | Initial interpreter, functions, conditions and loops                       | [0.0.1](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.1.md)   |
