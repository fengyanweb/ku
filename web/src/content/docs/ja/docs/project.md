---
title: 'プロジェクトと貢献'
description: 'Ku のプロジェクト状況、公開ソース、ライセンスの境界、再現可能な問題報告。'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

現在のドキュメントは実験的プレリリース Ku 0.0.19 を対象とし、CLI と VS Code 拡張機能のバージョンも 0.0.19 です。公開状況、ファイル、チェックサムは実際の GitHub Release を確認してください。

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## ソースとライセンス

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku と VS Code 拡張機能は Apache License 2.0 で提供されます。[LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) と [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE) を確認してください。サードパーティのコンポーネントには、それぞれの著作権表示とライセンスが適用されます。

## 問題を報告する

再現可能な報告には、リポジトリの [Issues](https://github.com/fengyanweb/ku/issues) を使用してください。次を含めます：

- Ku のバージョン、開発ソースの Git コミット、ターゲットシステム／アーキテクチャ。
- 完全なコマンド、バックエンド、最小の `.ku` ソース。
- 完全な診断または構造化 Error、期待した出力、実際の出力。
- ビルド問題では Rust / C ツールチェーンのバージョンと、ローカルインポートの有無。

トークン、パスワード、実際の接続文字列、機密の業務データを含むログは送信しないでください。性能報告には、環境、負荷、測定方法、生の結果を含めます。1 回の成功例では、本番スループットは証明できません。

## 貢献する前に

言語の挙動は[構文仕様](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md)と[意味仕様](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md)で定義されます。CI、実装、仕様を併せて確認してください。作業ログの目標は、実装済み機能ではありません。

変更は限定し、再現方法、原因、範囲、検証を説明してください。ウェブサイトの内容は、対応するソースと併せてレビューします。既存テスト、作業ツリーの変更、固有の証拠を保全してください。貢献の受け入れとライセンスの取り決めは保守担当者が決定します。このウェブサイトは貢献契約を新設しません。

このウェブサイトは静的です。本文、ナビゲーション、コードに業務バックエンドは不要です。検索はブラウザーに Pagefind インデックスを読み込みます。サイトは Ku コードのリモート実行を提供しません。

続いて、[はじめに](/ja/docs/getting-started/)、[対応範囲の境界](/ja/docs/support/)、[リリースノート](/ja/releases/)をご覧ください。
