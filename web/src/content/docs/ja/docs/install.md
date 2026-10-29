---
title: 'インストールとダウンロード'
description: 'Ku 0.0.19 実験的プレリリースのダウンロードと、プラットフォームおよびビルドの要件。'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

お使いのコンピューター向けの **Ku 0.0.19** をダウンロードしてください。この実験的プレリリースの各パッケージには、コマンドラインツール、インタープリター、VS Code 拡張機能が含まれています。

## 1. Ku をダウンロード

| システム | アーキテクチャ        | ダウンロード                                                                                                                          | サイズ  |
| -------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| macOS    | Apple Silicon / ARM64 | [ダウンロード macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows  | x64                   | [ダウンロード Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux    | x64 / GNU             | [ダウンロード Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

## 2. 展開して実行

**パッケージ全体**を空のフォルダーに展開します。たとえば、ホームフォルダー内の `ku` を使用できます。展開したファイルはすべて一緒に置き、実行ファイルだけを移動しないでください。そのフォルダーでターミナルを開きます。

macOS / Linux：

```sh
./ku --version
```

Windows PowerShell：

```powershell
.\ku.exe --version
```

`ku 0.0.19` と表示されれば準備完了です。`.ku` ファイルの実行とチェックには、Rust のインストールは必要ありません。

<details>
<summary>任意：どのフォルダーからでも ku を使う</summary>

`~/ku` に展開した場合、シェルの起動ファイル（`~/.zshrc` や `~/.bashrc` など）に次の行を追加します：

```sh
export PATH="$HOME/ku:$PATH"
```

Windows では「アカウントの環境変数を編集」を開き、ユーザーの `Path` に展開先フォルダーの完全なパス（例：`%USERPROFILE%\ku`）を追加します。既存の項目は残してください。

新しいターミナルを開き、`ku --version` を実行します。

</details>

## 3. VS Code 拡張機能をインストール

VS Code の拡張機能ビューで **… → VSIX からのインストール…** を選び、展開先の `ku-language-0.0.19.vsix` を開いてウィンドウを再読み込みします。

Ku を `PATH` に追加しない場合は、`ku.executablePath` に `ku`（Windows では `ku.exe`）の完全なパスを設定します。

<details>
<summary>ダウンロードを検証</summary>

パッケージの SHA-256 を [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS) と比較できます。[リリースページ](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)には、各パッケージのファイルとチェックサムを記録した `.manifest.json` もあります。詳細はパッケージ内の `README-INSTALL.md` を参照してください。

</details>

<details>
<summary>自分の Ku プログラムをコンパイルする</summary>

`ku run` と `ku check` には対応プラットフォームの CLI だけが必要です。既定の `ku build` には Rust **1.89.0** とシステムリンカーが、`ku build --backend c` には対象の C コンパイラーとリンカーが別途必要です。ネイティブのデータベースプログラムには、パッケージに含まれない対応クライアントライブラリも必要です。

</details>

<details>
<summary>アンインストール</summary>

自分で作成したソースや設定を保存してから、展開先フォルダー、追加した `PATH` の項目、VS Code 拡張機能を削除します。

</details>

[次のステップ：最初のプログラムを実行](/ja/docs/getting-started/)
