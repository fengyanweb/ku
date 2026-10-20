---
title: 'インストールとダウンロード'
description: 'Ku 0.0.18 実験的プレリリースのダウンロードと、プラットフォームおよびビルドの要件。'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> v0.0.18 の追加情報は現在英語で掲載しています。既存の翻訳は保持しています。 [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## プラットフォームを選ぶ

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| システム | アーキテクチャ        | アーカイブ                                   |
| -------- | --------------------- | -------------------------------------------- |
| macOS    | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows  | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux    | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

その他のプラットフォーム向けアーカイブは検証されていません。クロスコンパイルへの対応は、すべての OS やアーキテクチャ向けパッケージの公開を意味しません。

## 展開と検証

1. リリースの `SHA256SUMS` と照合してアーカイブを検証します。別の `.manifest.json` には、アーカイブとファイルの整合性が記載されています。チェックサムは整合性の確認であり、公開者の署名ではありません。
2. **アーカイブ全体**を空のディレクトリに展開し、`README-INSTALL.md` を読みます。`ku`、`libku.rlib`、`deps/`、`native-tls/` の相対配置を維持してください。
3. そのディレクトリからバージョンを確認します。macOS / Linux の場合：

```sh
./ku --version
```

Windows PowerShell の場合：

```powershell
.\ku.exe --version
```

表示されるバージョンは `0.0.18` です。必要に応じてこのディレクトリを `PATH` に追加し、`ku --version` を実行してください。VS Code 連携には、同梱の `ku-language-0.0.18.vsix` をインストールします。

## 実行とビルドの要件

| 操作                                    | 追加要件                                                                                    |
| --------------------------------------- | ------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 対応するプラットフォームの Ku CLI                                                           |
| 既定の `ku build hello.ku`              | ターゲット用 Rust **1.89.0** とシステムリンカー。同梱の Rust 依存ファイルを保持してください |
| `ku build --backend c hello.ku`         | 動作するターゲット用 C コンパイラーとリンカー                                               |
| ネイティブのデータベースプログラム      | 対応する外部データベースクライアント動的ライブラリ。同梱されていません                      |

既定のランナーに埋め込まれるのはエントリーソースだけです。インポートしたモジュールは引き続き元の場所から読み込みます。ローカルのインポートグラフ全体をソースに依存せず配布するにはネイティブ C コンパイルが必要で、[バックエンドの対応範囲](/ja/docs/support/)にも制約されます。

## 開発ソースを試す

ローカルにチェックアウトしたリポジトリで実行します：

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## アンインストール

アーカイブからインストールした場合、展開先ディレクトリを削除し、追加した `PATH` の設定を戻し、VS Code 拡張機能をアンインストールします。その場所に作成したソースファイルや設定は、先に保管してください。専用のワンクリックインストーラー、パッケージマネージャーによるインストール、`ku uninstall` コマンドは検証されていません。

次へ：[最初のプログラムを実行する](/ja/docs/getting-started/)。インストールに関する仕様の出典：[リリースアーカイブ](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md)。

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/ja/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
