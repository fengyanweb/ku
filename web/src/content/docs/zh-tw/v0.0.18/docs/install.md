---
title: 安裝與下載
description: 下載 Ku 0.0.18 實驗預發行包，瞭解平台、構建工具與安裝驗證。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - Cargo.toml
  - docs/v0.0.18.md
  - docs/history.md
  - .github/scripts/export-release.py
slug: zh-tw/v0.0.18/docs/install
---

> 以下 v0.0.18 新增發布資訊暫以英文提供；既有譯文保留。 [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## 選擇對應平台

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| 系統    | 架構                  | 歸檔文件                                     |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

沒有覈實的其他平台安裝包。跨平台編譯能力也不表示任意操作系統或架構已有發行包。

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

## 執行與構建需要什麼

| 操作                                    | 額外要求                                                       |
| --------------------------------------- | -------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 對應平台的 Ku CLI                                              |
| 預設 `ku build hello.ku`                | 匹配目標的 Rust **1.89.0** 及系統 linker；保留隨包的 Rust 依賴 |
| `ku build --backend c hello.ku`         | 可工作的目標 C 編譯器與 linker                                 |
| native 資料庫程序                       | 另外安裝對應資料庫客戶端動態庫；發行包未包含這些庫             |

預設 build 的 runner 只嵌入入口原始碼，匯入模組仍從原路徑讀取。完整本地 import graph 的原始碼無關部署需要 native C 構建，並受[後端支援範圍](/zh-tw/v0.0.18/docs/support/)約束。

## 從原始碼試用開發分支

在已取得的倉庫根目錄執行：

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## 卸載

歸檔式安裝可移除自行解壓的 Ku 目錄，撤銷自己添加的 `PATH` 項，並在 VS Code 中卸載擴展。刪除前保留該目錄里自行創建的原始碼和配置。目前未覈實獨立的一鍵安裝器、包管理器安裝入口或 `ku uninstall` 命令。

下一步：[執行第一個程序](/zh-tw/v0.0.18/docs/getting-started/)。安裝合同來源：[發佈歸檔說明](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md)。

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/zh-tw/v0.0.18/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
