---
title: 'プラットフォームとバックエンドの対応範囲'
description: 'Ku の公開プラットフォーム、インタープリター、ランナー、ネイティブ C、Task サブセット、LLVM の違い。'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Ku 0.0.19 のダウンロード

Ku 0.0.19 は実験的プレリリースです。macOS ARM64（Apple Silicon）、Windows x64、Linux x64（GNU）向けのパッケージを公開しています。

[パッケージとインストール手順](/ja/docs/install/)

## 実行方法

- **`ku run`**: 言語機能、汎用 JSON、同期 HTTP ハンドラーを扱うインタープリターです。データベースクライアントは実装されていません。
- **`ku build`**: 既定の runner はエントリーファイルを埋め込みますが、ローカル import には元のソースファイルが必要です。Rust **1.89.0** とシステムリンカーが必要です。
- **`ku build --backend c`**: 同期 JSON、`http.json` を含む HTTP、PostgreSQL・Redis・MySQL のネイティブクライアントをサポートします。対象の C コンパイラー、リンカー、対応クライアントライブラリが必要です。データベース操作は自動再試行しません。
- **Task**: ネイティブ Task は型付きのサブセットを対象に、開始/`await`、タイマー、一部の I/O、名前付き async HTTP ハンドラー、特定の PG クエリをサポートします。この経路の JSON はスカラー値のオブジェクトリテラルのエンコードに限られ、汎用デコードは利用できません。完全な M:N と汎用 I/O の対応は未完了です。
- **LLVM**: LLVM テキストバックエンドは引き続き限定的なプロトタイプです。

## 既知の問題

3 プラットフォームのリリースチェックとパッケージ検証は通過しています。すべての本番用途を検証したことを意味しません。

- **HTTP 504：** 過去に発生しましたが、最新の短時間再検証では再現していません。原因は未確定です。
- **キャンセル中の遅延スパイク：** 最新の 30 秒再検証では、短いリクエスト 60,000 件がすべて正しく処理されました。p99 は **1.557 ms**、最大値は **98.809 ms** で、20 ms を超えるリクエストが残っています。

正式な 30 分間の安定性検証はまだ通過していません。短時間の測定で代替することはできません。

詳しいリファレンス: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

問題の報告: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
