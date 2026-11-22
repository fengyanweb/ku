# Changelog

## 0.0.21 — current language and daily-editor delivery (2026-11-22)

- Scope-aware local/member completion, function signatures, struct/object returns, Task/Result unwrapping, nested destructuring and optional fields; conservative unknown types.
- Real locked-package imports, readable source/binding definitions and explicit CLI-managed dependency installation.
- Audited time/Duration, array, HTTP, PG and compression signatures; structured async highlighting and runnable snippets.
- CLI picker, actual path/version display, interactive Run, project-aware cwd and build output selection; preserve existing IDs/settings.
- Static editing in Restricted Mode, bounded/redacted tool logs and owned-process cancellation.
- Retain the prior diagnostic-queue repair and add isolated installed-VSIX/real CLI validation; update English/Chinese docs and add plugin-only three-platform CI definition.
- Extension version is independent of the tested Ku 0.0.20 daily SDK; do not equate same-version release bundles with daily SDK capabilities.

# 0.0.20

- 修复 CLI 查找、检查进程和文档失效时的诊断队列处理。
- 改善格式化、导入补全与编辑器行为，保留工作区信任要求。
- 与 Ku 0.0.20 配套发布；重新构建 VSIX。

# Changelog

## 0.0.19 — diagnostic queue follow-up (2026-11-01)

- Preserve clean queued checks, their original waiters and manual reveal intent when another document changes or saves.
- Requeue cancelled clean active checks after the old process exits, without reviving dirty, closed or superseded requests. Existing pending work keeps priority; the two-job limit remains unchanged.
- Verify the A/B active + C manual pending + unrelated D edit sequence, repeated edits, clean saves with automatic checks off, close/dirty cancellation, configuration reset and latest-request replacement.

## 0.0.19 — local extension hardening (2026-11-01)

- Run, IR, build and C-source emission use VS Code process tasks with intact paths and save-failure handling; loose files use a direct terminal executable.
- Resource-scoped CLI selection supports explicit/relative/home/workspace paths, validates Ku version output, and reports invalid explicit configuration.
- Saved-file diagnostic scheduling cancels stale work, bounds concurrency, clears closed/dirty documents and preserves contributions from multiple roots.
- Local import completion preserves directory prefixes and matching quotes; import ranges and named aliases are corrected.
- Formatting protects strings/comments and multiline literal contents, preserves line endings, and respects indentation options.
- Document actual command semantics, CLI installation/toolchain requirements, heuristic limitations and Restricted Mode behavior.
- Package compiled output with no runtime dependencies; reuse existing tests and add explicit lint/typecheck/build/package scripts.

This is an independently committed integration candidate based on `f85f0added0ff62953d961e47c65474b1a9871a8`. No CLI protocol, language core, SDK, public release asset or daily installation is changed by this delivery.
