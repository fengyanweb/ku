---
title: "0.0.17 support"
description: "0.0.17 support"
kuVersion: "0.0.17"
verifiedAt: "2026-10-21"
sources: ["docs/v0.0.17.md", "docs-site/guide/syntax.md"]
slug: en/v0.0.17/docs/support
---

0.0.17 is an experimental prerelease for Windows x64/MSVC, Linux x64/GNU and macOS arm64. Each target archive contains the CLI, Rust runner dependencies, VSIX and target TLS pack.

Interpretation needs no Rust/C compiler. Default runner builds require matching Rust 1.89.0; native C requires a target C toolchain. The default runner embeds only its entry source and still depends on original import paths. Database clients are native-only. Full native async and self-hosting are incomplete; LLVM does not support `&` borrowing.

This version does not have 0.0.18's `ku supervise` process-limit mode. Source and generated C ABI compatibility are not promised across 0.0.x releases.

[Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) · [Original version record](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.17.md)
