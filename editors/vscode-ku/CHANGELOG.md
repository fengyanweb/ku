# Changelog

## 0.0.19 — local extension hardening (2026-11-01)

- Run, IR, build and C-source emission use VS Code process tasks with intact paths and save-failure handling; loose files use a direct terminal executable.
- Resource-scoped CLI selection supports explicit/relative/home/workspace paths, validates Ku version output, and reports invalid explicit configuration.
- Saved-file diagnostic scheduling cancels stale work, bounds concurrency, clears closed/dirty documents and preserves contributions from multiple roots.
- Local import completion preserves directory prefixes and matching quotes; import ranges and named aliases are corrected.
- Formatting protects strings/comments and multiline literal contents, preserves line endings, and respects indentation options.
- Document actual command semantics, CLI installation/toolchain requirements, heuristic limitations and Restricted Mode behavior.
- Package compiled output with no runtime dependencies; reuse existing tests and add explicit lint/typecheck/build/package scripts.

This is an independently committed integration candidate based on `f85f0added0ff62953d961e47c65474b1a9871a8`. No CLI protocol, language core, SDK, public release asset or daily installation is changed by this delivery.
