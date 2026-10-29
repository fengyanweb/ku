---
title: 'プロジェクトと貢献'
description: 'Ku のプロジェクト状況、公開ソース、ライセンスの境界、再現可能な問題報告。'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
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

Ku はプログラミング言語とツールチェーンです。インタープリター、既定の実行可能 runner、対応する部分集合のネイティブ C コンパイルを提供します。

## ソースとライセンス

Ku と VS Code 拡張機能は **Apache-2.0** ライセンスで公開されています。[ソースコード](https://github.com/fengyanweb/ku)、[0.0.19 リリース](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)、[ライセンス全文](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE)をご覧ください。

サードパーティーのコンポーネントにはそれぞれのライセンスが適用されます。リリースパッケージには対応するライセンス全文と通知が含まれます。

## 問題を報告する

再現可能な報告には、リポジトリの [Issues](https://github.com/fengyanweb/ku/issues) を使用してください。次を含めます：

メールで報告: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku のバージョン、開発ソースの Git コミット、ターゲットシステム／アーキテクチャ。
- 完全なコマンド、バックエンド、最小の `.ku` ソース。
- 完全な診断または構造化 Error、期待した出力、実際の出力。
- ビルド問題では Rust / C ツールチェーンのバージョンと、ローカルインポートの有無。

トークン、パスワード、実際の接続文字列、機密の業務データを含むログは送信しないでください。性能報告には、環境、負荷、測定方法、生の結果を含めます。1 回の成功例では、本番スループットは証明できません。

## 貢献する前に

変更範囲を絞り、最小の再現例、影響範囲、検証結果を添えてください。コード、ドキュメント、例は同じバージョンを対象にしてください。貢献はメンテナーが確認してからマージします。

このサイトは Ku コードのオンライン実行を提供していません。

続いて、[はじめに](/ja/docs/getting-started/)、[対応範囲の境界](/ja/docs/support/)、[リリースノート](/ja/releases/)をご覧ください。
