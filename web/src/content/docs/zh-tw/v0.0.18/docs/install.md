---
title: 安裝與下載
description: 下載 Ku 0.0.18 實驗預發行包，瞭解平台、構建工具與安裝驗證。
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
slug: zh-tw/v0.0.18/docs/install
---

## 選擇對應平台

[下載 Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| 系統    | 架構                  | 歸檔文件                                     |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

目前提供以上三個平台的預建包；跨平台編譯不代表每個系統或架構都有發行包。

## 解壓與驗證

1. 對照 Release 的 `SHA256SUMS` 校驗歸檔；並列 `.manifest.json` 提供歸檔和文件清單信息。校驗和用於檢查完整性，不是發佈者簽名。
2. 把**完整歸檔**解壓到一個空目錄，閱讀其中的 `README-INSTALL.md`。保留 `ku`、`libku.rlib`、`deps/` 和 `native-tls/` 的相對佈局。
3. 在解壓目錄檢查版本。macOS / Linux：

```sh
./ku --version
```

Windows PowerShell：

```powershell
.\ku.exe --version
```

應顯示版本 `0.0.18`。需要從任意目錄呼叫時，把該目錄加入 `PATH`，再執行 `ku --version`。VS Code 擴展通過歸檔內的 `ku-language-0.0.18.vsix` 安裝。

可直接複製的安裝步驟：macOS 需要 curl、tar、shasum；Linux 需要 curl、tar、awk、sha256sum；Windows 使用 PowerShell 與 tar.exe。先核對 SHA256，再執行 Hello Ku。PATH 僅在 POSIX 子 shell 暫時生效，PowerShell 以 finally 還原，不修改全域設定。Windows 原始碼使用無 BOM 的 UTF-8，並處理路徑空白。

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

## 執行與構建需要什麼

| 操作                                    | 額外要求                                                       |
| --------------------------------------- | -------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 對應平台的 Ku CLI                                              |
| 預設 `ku build hello.ku`                | 匹配目標的 Rust **1.89.0** 及系統 linker；保留隨包的 Rust 依賴 |
| `ku build --backend c hello.ku`         | 可工作的目標 C 編譯器與 linker                                 |
| native 資料庫程序                       | 另外安裝對應資料庫客戶端動態庫；發行包未包含這些庫             |

預設 build 的 runner 只嵌入入口原始碼，匯入模組仍從原路徑讀取。完整本地 import graph 的原始碼無關部署需要 native C 構建，並受[後端支援範圍](/zh-tw/v0.0.18/docs/support/)約束。

預設 runner 會呼叫 rustc。請依 RELEASE.json 與隨包 rlib 明確選用 Rust 1.89.0；其他預設 Rust 可能產生 E0514。Ku 須先加入目前終端的暫時 PATH。

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## 建置對應原始碼

以下命令可建置已發布的原始碼。

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## 卸載

目前採用歸檔安裝。移除前先備份在安裝目錄建立的程式與設定，再刪除該目錄、撤銷自行加入的 `PATH` 項目並解除安裝 VS Code 擴充功能。

下一步：[執行第一個程序](/zh-tw/v0.0.18/docs/getting-started/)。安裝合同來源：[發佈歸檔說明](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md)。

`ku supervise` 的平台條件與記憶體指標見[支援範圍](/zh-tw/v0.0.18/docs/support/)。macOS 可正常執行 CLI 程式，但原生不支援此隔離模式。
