---
title: Project and contribution
description: Ku project status, public source, licensing boundaries, and reproducible issue reports.
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

The project is **Ku programming language / Ku 编程语言**, with the CLI named `ku`. Ku is a programming language and toolchain with explicit ownership and Result-based error handling. Run through the interpreter, build an executable runner, or compile supported programs through the native C backend. The current version is Ku 0.0.19.

<span id="license"></span>

## Source and licensing

Ku and its VS Code extension are released under **Apache-2.0**. See the [source](https://github.com/fengyanweb/ku), [0.0.19 release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) and [license text](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Third-party components retain their own licenses. Release packages include the corresponding license texts and notices.

## Report a problem

Use the repository's [Issues](https://github.com/fengyanweb/ku/issues) for reproducible reports. Include:

Email feedback: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku version, Git commit for development sources, and target system / architecture.
- Full command, backend, and minimal `.ku` source.
- Complete diagnostic or structured Error, expected output, and actual output.
- Rust / C toolchain versions for build issues, and whether local imports are involved.
- Archive filename/checksum and actual CLI path. For a new fix candidate, include `ku doctor --json`; the original public 0.0.19 may lack doctor. See [Runner troubleshooting](/en/docs/guide/errors/#runner-build).

Do not submit tokens, passwords, real connection strings, or logs containing sensitive business data. Performance reports should include environment, workload, measurement method, and raw results. One successful sample does not establish production throughput.

## Before contributing

The [syntax reference](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) and [language semantics](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) define language behavior. Distinguish published behavior from development branch changes when choosing a contribution.

Keep changes focused and explain reproduction, cause, scope, and validation. Code, documentation and examples should target the same version. Maintainers review proposed changes before merging.

The website does not currently provide an online Ku interpreter.

Continue with [getting started](/en/docs/getting-started/), [support boundaries](/en/docs/support/), and [release notes](/en/releases/).
