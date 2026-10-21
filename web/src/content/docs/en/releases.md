---
title: 'Versions and release notes'
description: 'Versions and release notes'
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['docs/history.md', 'docs/v0.0.17.md', 'docs/v0.0.18.md']
---

## 0.0.18

The current experimental prerelease includes bounded Task/waiting and shutdown fixes, HTTP connection reuse and buffers, large-float JSON round trips and explicit `ku supervise` workers.

[Downloads and checksums](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) · [Fixed docs](/en/v0.0.18/) · [Known limits](/en/docs/support/)

**Known HTTP issue: historical 504s, not reproduced by this round's short tests; root cause unconfirmed; formal 30-minute acceptance has not passed.** Complete M:N, general network scheduling and the complete database platform matrix remain incomplete.

## 0.0.17

Synchronous borrowing, braced while in the self-hosted parser subset and cross-platform tooling corrections.

[Download and release notes](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) · [Fixed docs](/en/v0.0.17/)

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
