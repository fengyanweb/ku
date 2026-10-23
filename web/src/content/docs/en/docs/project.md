---
title: Project and contribution
description: Ku project status, public source, licensing boundaries, and reproducible issue reports.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

The project is **Ku programming language / Ku 编程语言**, with the CLI named `ku`. Ku is a small programming language and interpreter under development. Its current public release is the v0.0.18 experimental prerelease.

## Source and licensing

Ku's own code in source snapshots that include the license file, and the current editor extension, are licensed under [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE) (`Apache-2.0`); see [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE) for attribution. View the [GitHub source](https://github.com/fengyanweb/ku) and [release page](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Third-party components retain their own licenses.

The v0.0.18-r1 and earlier release archives retain their accompanying original license materials; this source licensing change does not repackage those archives. Using the Ku compiler does not require you to publish your own program's source code. When distributing programs containing Ku runtime code, comply with the applicable license and attribution requirements, including third-party notices.

## Report a problem

Use the repository's [Issues](https://github.com/fengyanweb/ku/issues) for reproducible reports. Include:

- Ku version, Git commit for development sources, and target system / architecture.
- Full command, backend, and minimal `.ku` source.
- Complete diagnostic or structured Error, expected output, and actual output.
- Rust / C toolchain versions for build issues, and whether local imports are involved.

Do not submit tokens, passwords, real connection strings, or logs containing sensitive business data. Performance reports should include environment, workload, measurement method, and raw results. One successful sample does not establish production throughput.

## Before contributing

The [syntax contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) and [semantic contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) define language behavior. Check CI, implementation, and contracts together. A work log target is not an implemented feature.

Keep changes focused and explain reproduction, cause, scope, and validation. Review website content alongside the relevant source. The maintainer determines contribution acceptance and licensing arrangements; this website does not invent a contribution agreement.

This website is static. Its prose, navigation, and code do not require a business backend. Search loads a Pagefind index in the browser; the site does not provide remote Ku code execution.

Continue with [getting started](/en/docs/getting-started/), [support boundaries](/en/docs/support/), and [release notes](/en/releases/).
