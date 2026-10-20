---
title: リリースノート
description: 'Ku v0.0.18 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/history.md
  - Cargo.toml
slug: ja/v0.0.18/releases
---

本書は実験的プレリリース v0.0.18 の確定した範囲を説明します。実際の公開状況とダウンロードは GitHub Releases を確認してください。

> v0.0.18 の追加情報は現在英語で掲載しています。既存の翻訳は保持しています。 [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

### Process memory boundary

| Host | `ku supervise` guarantee and prerequisite |
| --- | --- |
| Linux | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment. |
| macOS | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge. |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.


## v0.0.17 · 実験的プレリリース

公開日は **2026-10-18（America/Los_Angeles）**、UTC では `2026-10-19T03:51:10Z` です。リリースは `prerelease=true`、`draft=false` です。現時点で安定版リリースは確認されていません。

主な変更：

* 同期の読み取り専用借用パラメーターは `&name: T` を使用し、呼び出しでは通常の引数を渡します。通常の所有パラメーターはムーブ意味論を維持します。
* セルフホスト構文解析器サブセットに、波括弧付き `while` を追加しました。入れ子の深さと文の数には上限があります。
* クロスプラットフォームテストの標準出力切断チェックを修正しました。業務 API の追加ではありません。
* アーカイブには、既定のランナーに対応する Rust 依存ファイルと、ターゲット用 TLS パックを含みます。適切な外部ツールチェーンは引き続き必要です。

ダウンロード対象は Windows x64、Linux x64、macOS arm64 です。リリース成果物 7 件の内訳は、プラットフォームアーカイブ 3 件、マニフェスト 3 件、`SHA256SUMS` です。[インストール](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)と[完全なリリースノート](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)をご覧ください。

### 互換性と制約

Ku は実験段階であり、本番環境で使用できる状態ではありません。0.0.x ではソースや生成 C/FFI ABI の互換性を約束しません。更新時には関連するネイティブ成果物を再コンパイルしてください。非同期パラメーターの借用、完全なセルフホスト、完全なネイティブ async は、このリリースの範囲外です。

既定のランナーに埋め込まれるのはエントリーソースだけで、インポートしたモジュールには元の場所が必要です。データベースクライアントはネイティブ専用です。各ドライバーの実サーバー、TLS、セッション状態の制約を確認してください。Ku 自体のコードには引き続きオープンソースライセンスの許諾がありません。
