---
title: 安裝與下載
description: 下載 Ku 0.0.19 實驗預發行包，瞭解平台、構建工具與安裝驗證。
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

下載適合你電腦的 **Ku 0.0.19**。目前為實驗預發行版，平台套件已包含命令列工具、直譯器和 VS Code 擴充功能。

## 1. 下載 Ku

| 系統    | 架構                  | 下載                                                                                                                          | 大小    |
| ------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ------- |
| macOS   | Apple Silicon / ARM64 | [下載 macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows | x64                   | [下載 Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux   | x64 / GNU             | [下載 Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

## 2. 解壓縮並執行

將**整個套件**解壓縮到空資料夾，例如使用者目錄下的 `ku` 資料夾。保留解壓縮後的所有檔案，不要只移動執行檔。在該資料夾開啟終端機。

macOS / Linux：

```sh
./ku --version
```

Windows PowerShell：

```powershell
.\ku.exe --version
```

顯示 `ku 0.0.19` 即可。現在就能執行和檢查 `.ku` 檔案，這兩項操作不需要安裝 Rust。

<details>
<summary>選用：在任何目錄使用 ku</summary>

如果解壓縮至 `~/ku`，在所用 shell 的啟動檔（如 `~/.zshrc` 或 `~/.bashrc`）加入這一行：

```sh
export PATH="$HOME/ku:$PATH"
```

Windows 開啟「編輯帳戶的環境變數」，編輯使用者 `Path`，新增解壓縮資料夾的完整路徑，例如 `%USERPROFILE%\ku`。保留原有項目。

重新開啟終端機，執行 `ku --version` 確認。

</details>

## 3. 安裝 VS Code 擴充功能

在 VS Code 擴充功能面板選擇 **… → 從 VSIX 安裝…**，選取解壓縮目錄中的 `ku-language-0.0.19.vsix`，再重新載入視窗。

若未設定 `PATH`，將 `ku.executablePath` 設為 `ku`（Windows 為 `ku.exe`）的完整路徑。

<details>
<summary>核對下載檔案</summary>

將套件的 SHA-256 與 [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS) 比對。[發行頁面](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)也為各平台提供 `.manifest.json` 檔案清單與校驗值。套件中的 `README-INSTALL.md` 提供進一步說明。

</details>

<details>
<summary>編譯自己的 Ku 程式</summary>

`ku run` 和 `ku check` 只需對應平台的 CLI。預設 `ku build` 另需 Rust **1.89.0** 和系統連結器；`ku build --backend c` 另需目標 C 編譯器和連結器。native 資料庫程式還需要相符的資料庫用戶端函式庫，這些函式庫不包含在平台套件內。

</details>

<details>
<summary>解除安裝</summary>

先保留自行建立的原始碼和設定，再刪除解壓縮資料夾、自己新增的 `PATH` 項目，並在 VS Code 解除安裝擴充功能。

</details>

[下一步：執行第一個程式](/zh-tw/docs/getting-started/)
