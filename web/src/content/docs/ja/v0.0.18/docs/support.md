---
title: プラットフォームとバックエンドの対応範囲
description: Ku の公開プラットフォーム、インタープリター、ランナー、ネイティブ C、Task サブセット、LLVM の違い。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/syntax.md
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/protocol-foundation.md
  - .github/workflows/native-three-os.yml
slug: ja/v0.0.18/docs/support
---

本書は実験的プレリリース v0.0.18 の確定した範囲を説明します。実際の公開状況とダウンロードは GitHub Releases を確認してください。

## 公開済み CLI のプラットフォーム

v0.0.18 実験的プレリリースは、Windows x64（MSVC）、Linux x64（GNU）、macOS arm64（Apple Silicon）に対応します。各プラットフォームに専用アーカイブがあり、3 システム共通の単一実行ファイルはありません。その他のプラットフォーム向け成果物は検証されていません。[インストール](/ja/v0.0.18/docs/install/)を参照してください。

## 実行経路

| 機能                                             | インタープリター `ku run`  | 既定の `ku build` ランナー         | 同期ネイティブ C                      | 開発中のネイティブ Task                | LLVM テキストプロトタイプ |
| ------------------------------------------------ | -------------------------- | ---------------------------------- | ------------------------------------- | -------------------------------------- | ------------------------- |
| 基本言語、関数、制御フロー                       | 実装済み                   | インタープリター経路               | サブセットを実装                      | 制限付きの型付きサブセット             | 限定プロトタイプ          |
| JSON                                             | 対応                       | 対応                               | 対応                                  | 未対応                                 | 未対応                    |
| ソースに依存しない完全なローカルインポートグラフ | ソースを読み込む           | 未対応。エントリーのみ埋め込む     | 生成時に展開                          | 対応サブセット内で展開                 | 配布に関する保証なし      |
| async / await                                    | リソース制限付きで実装     | インタープリター経路               | 専用サブセットまたは拒否              | 部分対応。完全な async ではない        | 完全対応なし              |
| PostgreSQL / Redis / MySQL クライアント          | データベースアクセスなし   | インタープリターによるアクセスなし | ネイティブ専用                        | 汎用 Task 待機ではない                 | 未対応                    |
| TCP / TLS クライアント                           | 対応するネイティブ経路なし | 対応するネイティブ経路なし         | 生成 C の実装。TLS は対応パックが必要 | 完全な汎用ネットワーク待機は保証しない | 未対応                    |

既定のビルドには、対応する Rust ツールチェーンとリンカーが必要です。ネイティブ C にはターゲット用 C ツールチェーンが必要です。`ku check` の成功は、どちらのビルドの成功も証明しません。LLVM コマンドの存在は、完全なリリース用バックエンドを意味しません。

## ネイティブ Task サブセット

現在の開発ブランチには、制限付きの `async fn main(): null!` ソース経路があります。明記されたプリミティブ、文字列、フラットな配列、単層 Result、Task の開始・ムーブ・await、一部の制御フロー、上限付きファイル／ディレクトリのバックグラウンド操作に対応します。公開済み v0.0.18 パッケージの完全な async バックエンドではありません。

汎用 Task ネットワーク待機、完全な M:N 実行、任意の動的ヒープの組み合わせ、すべての非同期キャプチャ／関数の組み合わせ、完全な長時間連続稼働検証は未完成です。受理および拒否する正確なサブセットは[並行処理仕様](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md)を参照してください。

## データベースとネットワークの制約

データベース API は `module.client(config)?` の後にレシーバーメソッドを呼び、クライアント内部の上限付きプールを使用します。実サーバー検証は、ドライバー、システム、コミットごとに別途必要です。模擬 ABI やリンク成功は、実際のクエリーの代わりにはなりません。

Redis は平文 TCP を使用します。MySQL には、Oracle/MariaDB を通じて一貫した fail-closed の証明書検証がありません。そのため直接接続はループバック、管理されたプライベートネットワーク、または検証済み TLS トンネルを使用してください。リモート PostgreSQL には明示的な `sslmode=verify-full` と信頼されたルート証明書が必要です。`std.net` のネイティブ TLS は、HTTP、Redis、インタープリターがその経路を再利用することを意味しません。

## セルフホストと互換性

Ku のセルフホスト字句解析器と構文解析器には、限定的なサブセットと差分検証があります。完全なセルフホスト構文解析器、チェッカー、IR、バックエンドは未完成で、ブートストラップツールチェーンは引き続き Rust が提供します。0.0.x では、ソースや生成 C/FFI ABI の互換性を約束しません。

作業ログと過去の CI 結果が示すのは、そこに記載された範囲とコミットだけです。[リリースノート](/ja/v0.0.18/releases/)で公開済み変更と開発作業を区別してください。このウェブサイトは、現在のブランチの CI、本番負荷、データベースの受け入れ検証がすべて成功したとは報告しません。

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
