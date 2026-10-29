---
title: Project and contribution
description: Ku project status, public source, licensing boundaries, and reproducible issue reports.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
slug: en/v0.0.18/docs/project
---

The project is **Ku programming language / Ku 编程语言**, with the CLI named `ku`. Ku is a programming language and toolchain with explicit ownership and Result-based error handling. Run through the interpreter, build an executable runner, or compile supported programs through the native C backend. The current version is Ku 0.0.18.

<span id="license"></span>

## Source and licensing

View the [GitHub source](https://github.com/fengyanweb/ku) and [release page](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Source is publicly viewable, but Ku's own code has **no open source license grant**. The editor extension is marked `UNLICENSED`. Publishing source or binaries does not automatically grant an open source license.

Third party notices in release archives apply only to their respective components. They do not establish permissions to use, modify, or redistribute Ku itself. Permissions for use, modification, and redistribution require the authorization explicitly provided by the maintainer.

## Report a problem

Use the repository's [Issues](https://github.com/fengyanweb/ku/issues) for reproducible reports. Include:

- Ku version, Git commit for development sources, and target system / architecture.
- Full command, backend, and minimal `.ku` source.
- Complete diagnostic or structured Error, expected output, and actual output.
- Rust / C toolchain versions for build issues, and whether local imports are involved.

Do not submit tokens, passwords, real connection strings, or logs containing sensitive business data. Performance reports should include environment, workload, measurement method, and raw results. One successful sample does not establish production throughput.

## Before contributing

The [syntax reference](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) and [language semantics](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md) define language behavior. Distinguish published behavior from development branch changes when choosing a contribution.

Keep changes focused and explain reproduction, cause, scope, and validation. Keep content aligned with the relevant source version and examples. The maintainer determines contribution acceptance and licensing arrangements.

The website does not currently provide an online Ku interpreter.

Continue with [getting started](/en/v0.0.18/docs/getting-started/), [support boundaries](/en/v0.0.18/docs/support/), and [release notes](/en/v0.0.18/releases/).
