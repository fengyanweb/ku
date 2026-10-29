---
title: 安装 Ku
description: 下载 Ku 0.0.18，按 macOS、Windows 和 Linux 步骤校验、解压并运行第一个程序。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/0.0.18.md',
    '.github/scripts/export-release.py',
  ]
slug: zh-cn/v0.0.18/docs/install
---

安装 **Ku 0.0.18**。当前仍为实验版本，暂不建议用于生产。请从[下载页面](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1)选择对应平台的安装包，校验后解压。

## 选择平台

当前提供以下三个平台的预构建包，采用归档安装。每个包有同名 `.manifest.json`，Release 的 `SHA256SUMS` 覆盖三个归档和三个 manifest。`native_ci-*` 是 CI 样例，不是安装包。

| 系统    | 架构                  | 归档文件                                     |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

其他目标的源码构建条件见[支持范围](/zh-cn/v0.0.18/docs/support/)。以下命令下载并校验对应平台的安装包。

## macOS · Apple Silicon

需要 `curl`、`tar` 和系统的 `shasum`。整段复制到终端：

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

## Linux · x64 GNU

需要 `curl`、`tar`、`awk` 和 `sha256sum`。在匹配的 Linux 系统中执行：

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

## Windows · x64

在提供 `tar.exe` 的 Windows 系统中，使用 PowerShell 执行。源码写为不带 BOM 的 UTF-8，含空格路径也用完整参数传递：

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

## 检查结果与安装目录

三个流程都先按 Release 的真实 `SHA256SUMS` 校验，再解压、检查版本并运行 Hello Ku。校验和检查文件完整性，不是发布者数字签名。版本输出应为：

```text
ku 0.0.18
```

`ku check` 成功后，`ku run hello.ku` 输出：

```text
Hello Ku
```

脚本会打印实际安装目录。POSIX 的 `PATH` 只在括号中的子 shell 生效；PowerShell 在 `finally` 恢复原 PATH。它们均不改全局或持久 PATH。之后可用打印的完整路径调用 `ku` / `ku.exe`；若希望当前终端内直接调用，再临时将该目录加入 PATH，关闭终端即失效。

归档里的文件直接位于解压目录，没有额外的顶层包目录。请保留 `ku`（Windows 为 `ku.exe`）、`libku.rlib`、`deps/`、`native-tls/` 的相对位置，并阅读 `README-INSTALL.md`。包内还有 `RELEASE.json`、文件级 `SHA256SUMS`、`THIRD_PARTY.json` 与 `ku-language-0.0.18.vsix`；归档内的校验清单与 Release 外层的同名文件用途不同。

## VS Code 扩展

在 VS Code 的扩展菜单选择“从 VSIX 安装”，打开安装目录里的 `ku-language-0.0.18.vsix`。从原 0.0.18 插件升级时，版本号相同也需要重新安装；若已启用 `code` 命令，可在该目录执行：

```sh
code --install-extension ./ku-language-0.0.18.vsix --force
```

## 运行与构建的要求

| 操作                                    | 额外要求                                                                   |
| --------------------------------------- | -------------------------------------------------------------------------- |
| `ku check hello.ku` / `ku run hello.ku` | 对应平台的完整 Ku 包；无需另装 Rust 或 C 编译器                            |
| 默认 `ku build hello.ku`                | 匹配目标的 Rust **1.89.0** 与系统 linker；保留随包 `libku.rlib` 和 `deps/` |
| `ku build --backend c hello.ku`         | 可工作的目标 C 编译器与 linker                                             |
| 原生数据库程序                          | 另装匹配的数据库客户端动态库；安装包未包含                                 |

默认 runner 会调用 `rustc`，必须与包内 `RELEASE.json` 的 `rust_toolchain` 一致。使用 Rustup 时，先安装 1.89.0 工具链，再在已把 Ku 加入临时 PATH 的终端显式选择它，避免其他默认 Rust 版本与随包 rlib 不兼容：

```sh
rustup run 1.89.0 ku build hello.ku
```

Windows PowerShell 使用：

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

默认 build 生成可执行的解释器 runner，只嵌入入口源码，导入模块仍从原位置读取。C 后端编译其支持的程序，完整本地 import graph 可在生成时展开；具体能力见[支持范围](/zh-cn/v0.0.18/docs/support/)。不要把默认 runner 解释成所有代码均已原生编译。

`ku supervise` 的平台条件与内存限制也见支持页；macOS CLI 可运行程序，但不原生提供该隔离模式。

## 从对应源码构建

需要 Git、Rust 1.89.0 和目标 linker。在新的目录中执行：

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

报告问题时请提供 `RELEASE.json` 的 commit、平台、后端和完整命令。

## 卸载

删除前保留安装目录里自行创建的程序和配置，再移除该目录并卸载 VS Code 扩展。上述安装步骤没有写入持久 PATH；如另行手动添加过，请一并撤销。当前使用归档安装，没有单独的卸载命令。

下一步：[运行第一个程序](/zh-cn/v0.0.18/docs/getting-started/)。
