---
title: 'インストールとダウンロード'
description: 'Ku 0.0.18 実験的プレリリースのダウンロードと、プラットフォームおよびビルドの要件。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
    'docs/v0.0.18-r1.md',
  ]
slug: ja/v0.0.18/docs/install
---

## プラットフォームを選ぶ

[Ku 0.0.18 をダウンロード](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| システム | アーキテクチャ        | アーカイブ                                   |
| -------- | --------------------- | -------------------------------------------- |
| macOS    | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows  | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux    | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

上記 3 プラットフォーム向けにビルド済みパッケージを提供します。クロスコンパイル対応は、全システム向けパッケージの配布を意味しません。

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

コピーして使えるインストール手順です。macOS は curl・tar・shasum、Linux は curl・tar・awk・sha256sum、Windows は PowerShell と tar.exe が必要です。SHA256 検証後に Hello Ku を実行します。PATH は POSIX の子シェル内だけで変更し、PowerShell では復元するため、永続設定は変えません。Windows のソースは BOM なし UTF-8 で、空白を含むパスにも対応します。

### macOS · Apple Silicon

```sh
(
  set -eu
  base="https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1"
  archive="ku-v0.0.18-aarch64-apple-darwin.tar.gz"
  work="$(mktemp -d "${TMPDIR:-/tmp}/Ku install.XXXXXX")"
  cd "$work"
  curl --fail --location --output "$archive" "$base/$archive"
  curl --fail --location --output SHA256SUMS "$base/SHA256SUMS"
  awk -v name="$archive" '$2 == name { print }' SHA256SUMS > archive.sha256
  test "$(wc -l < archive.sha256 | tr -d ' ')" = 1
  shasum -a 256 -c archive.sha256
  mkdir "Ku 0.0.18"
  tar -xzf "$archive" -C "Ku 0.0.18"
  cd "Ku 0.0.18"
  export PATH="$PWD:$PATH"
  ku --version
  cat > hello.ku <<'KU'
fn main() {
    println("Hello Ku")
}
KU
  ku check hello.ku
  ku run hello.ku
  printf 'Ku directory: %s\n' "$PWD"
)
```

### Linux · x64 GNU

```sh
(
  set -eu
  base="https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1"
  archive="ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz"
  work="$(mktemp -d "${TMPDIR:-/tmp}/Ku install.XXXXXX")"
  cd "$work"
  curl --fail --location --output "$archive" "$base/$archive"
  curl --fail --location --output SHA256SUMS "$base/SHA256SUMS"
  awk -v name="$archive" '$2 == name { print }' SHA256SUMS > archive.sha256
  test "$(wc -l < archive.sha256 | tr -d ' ')" = 1
  sha256sum -c archive.sha256
  mkdir "Ku 0.0.18"
  tar -xzf "$archive" -C "Ku 0.0.18"
  cd "Ku 0.0.18"
  export PATH="$PWD:$PATH"
  ku --version
  cat > hello.ku <<'KU'
fn main() {
    println("Hello Ku")
}
KU
  ku check hello.ku
  ku run hello.ku
  printf 'Ku directory: %s\n' "$PWD"
)
```

### Windows · x64

```powershell
$ErrorActionPreference = 'Stop'
$base = 'https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1'
$archive = 'ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz'
$work = Join-Path ([IO.Path]::GetTempPath()) ('Ku install ' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$archivePath = Join-Path $work $archive
$sumsPath = Join-Path $work 'SHA256SUMS'
Invoke-WebRequest -Uri "$base/$archive" -OutFile $archivePath
Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath
$pattern = '^[0-9a-fA-F]{64}\s+' + [regex]::Escape($archive) + '$'
$lines = @(Get-Content -LiteralPath $sumsPath | Where-Object { $_ -match $pattern })
if ($lines.Count -ne 1) { throw 'Missing or ambiguous SHA256SUMS entry' }
$expected = ($lines[0] -split '\s+')[0]
$actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
if ($actual -ne $expected) { throw 'SHA256 mismatch; do not extract or run' }
$kuDir = Join-Path $work 'Ku 0.0.18'
New-Item -ItemType Directory -Path $kuDir | Out-Null
tar -xzf $archivePath -C $kuDir
if ($LASTEXITCODE -ne 0) { throw 'Archive extraction failed' }
$hello = @'
fn main() {
    println("Hello Ku")
}
'@
[IO.File]::WriteAllText((Join-Path $kuDir 'hello.ku'), $hello + "`n", [Text.UTF8Encoding]::new($false))
$previousPath = $env:Path
Push-Location -LiteralPath $kuDir
try {
    $env:Path = "$kuDir;$env:Path"
    ku.exe --version
    if ($LASTEXITCODE -ne 0) { throw 'Version check failed' }
    ku.exe check hello.ku
    if ($LASTEXITCODE -ne 0) { throw 'Source check failed' }
    ku.exe run hello.ku
    if ($LASTEXITCODE -ne 0) { throw 'Program failed' }
} finally {
    $env:Path = $previousPath
    Pop-Location
}
Write-Output "Ku directory: $kuDir"
```

```text
ku 0.0.18
Hello Ku
```

## 実行とビルドの要件

| 操作                                    | 追加要件                                                                                    |
| --------------------------------------- | ------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 対応するプラットフォームの Ku CLI                                                           |
| 既定の `ku build hello.ku`              | ターゲット用 Rust **1.89.0** とシステムリンカー。同梱の Rust 依存ファイルを保持してください |
| `ku build --backend c hello.ku`         | 動作するターゲット用 C コンパイラーとリンカー                                               |
| ネイティブのデータベースプログラム      | 対応する外部データベースクライアント動的ライブラリ。同梱されていません                      |

既定のランナーに埋め込まれるのはエントリーソースだけです。インポートしたモジュールは引き続き元の場所から読み込みます。ローカルのインポートグラフ全体をソースに依存せず配布するにはネイティブ C コンパイルが必要で、[バックエンドの対応範囲](/ja/v0.0.18/docs/support/)にも制約されます。

既定の runner は rustc を呼び出します。RELEASE.json と同梱 rlib に合わせて Rust 1.89.0 を明示的に選択してください。異なる既定 Rust では E0514 が発生する場合があります。Ku は一時 PATH に置いてください。

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## 対応するソースのビルド

次のコマンドで公開版のソースコードをビルドします。

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## アンインストール

インストールはアーカイブ形式です。インストール先に作成したソースと設定を保存してから、ディレクトリ、自分で追加した `PATH`、VS Code 拡張機能を削除してください。

次へ：[最初のプログラムを実行する](/ja/v0.0.18/docs/getting-started/)。インストールに関する仕様の出典：[リリースアーカイブ](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md)。

`ku supervise` のプラットフォーム条件とメモリ指標は[対応範囲](/ja/v0.0.18/docs/support/)にあります。macOS の CLI はプログラムを実行できますが、この隔離モードにはネイティブ対応しません。
