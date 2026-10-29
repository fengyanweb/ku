---
title: Project and contribution
description: Ku project status, public source, licensing boundaries, and reproducible issue reports.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
    'LICENSE',
    'NOTICE',
  ]
---

The project is **Ku programming language / Ku 编程语言**, with the CLI named `ku`. Ku is a small programming language and interpreter under development. Use the release page to identify published versions and their platform assets.

## Source and licensing

View the [GitHub source](https://github.com/fengyanweb/ku) and [releases](https://github.com/fengyanweb/ku/releases). Ku is licensed under [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE). Read the repository [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE) and the license included with your particular release or source checkout.

Third-party components retain their own copyrights and license terms. Their notices and license materials remain separate from Ku's license; retain the materials required for the components you redistribute.

## Report a problem

Use the repository's [Issues](https://github.com/fengyanweb/ku/issues) for reproducible reports. Include:

Email feedback: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku version, Git commit for development sources, and target system / architecture.
- Full command, backend, and minimal `.ku` source.
- Complete diagnostic or structured Error, expected output, and actual output.
- Rust / C toolchain versions for build issues, and whether local imports are involved.

Do not submit tokens, passwords, real connection strings, or logs containing sensitive business data. Performance reports should include environment, workload, measurement method, and raw results. One successful sample does not establish production throughput.

## Before contributing

The [syntax contract](https://github.com/fengyanweb/ku/blob/main/docs/syntax.md) and [semantic contract](https://github.com/fengyanweb/ku/blob/main/docs/semantics.md) define language behavior. Check CI, implementation, and contracts together. A work log target is not an implemented feature.

Keep changes focused and explain reproduction, cause, scope, and validation. Review website content alongside the relevant source. The maintainer determines contribution acceptance; this website does not invent a separate contribution agreement.

This website is static. Its prose, navigation, and code do not require a business backend. Search loads a Pagefind index in the browser; the site does not provide remote Ku code execution.

Continue with [getting started](/en/docs/getting-started/), [support boundaries](/en/docs/support/), and [release notes](/en/releases/).
