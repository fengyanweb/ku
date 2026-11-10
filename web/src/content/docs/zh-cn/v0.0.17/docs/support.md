---
title: "0.0.17 支持范围"
description: "0.0.17 支持范围"
kuVersion: "0.0.17"
verifiedAt: "2026-10-21"
sources: ["docs/v0.0.17.md", "docs-site/guide/syntax.md"]
slug: zh-cn/v0.0.17/docs/support
---

0.0.17 是实验预发行版，下载目标为 Windows x64/MSVC、Linux x64/GNU、macOS arm64。每个平台的归档分别包含 CLI、Rust runner 依赖、VSIX 和目标 TLS pack。

解释运行不需要 Rust/C 工具链；默认 runner 构建需要匹配 Rust 1.89.0，native C 需要相应 C 工具链。默认 runner 仅嵌入入口源码，导入模块仍依赖其原路径。数据库客户端为 native-only。完整 native async 与完整自举未完成，LLVM 不支持 `&` 借用。

该版没有 0.0.18 的 `ku supervise` 进程限制模式。0.0.x 不保证源码或生成 C ABI 向后兼容。

[该版 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) · [原始版本记录](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.17.md)
