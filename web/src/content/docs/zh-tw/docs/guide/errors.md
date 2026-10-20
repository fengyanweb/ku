---
title: 錯誤與排查
description: 定位 Ku 安裝、匯入、所有權、JSON 與後端能力錯誤。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
  ]
---

本文檔對應 v0.0.18 實驗預發行版的凍結範圍；實際公開狀態與下載資產以 GitHub Release 為準。編譯器診斷可能隨實驗版本收斂。報告問題時同時提供 CLI 版本、Git commit、後端、完整命令和最小原始碼，避免把不同版本的行為混在一起。

## 先確認執行的是哪一個 Ku

```sh
ku --version
ku -h
ku check hello.ku
```

找不到 `ku` 時，直接呼叫解壓目錄的可執行文件；Windows 使用 `ku.exe`。預設 build 缺失 Rust 或鏈接庫時，核對[安裝要求](/zh-tw/docs/install/)，保留整個歸檔佈局，不只複製 CLI 文件。

## 讀取診斷

普通檢查包含文件、行列、原始碼標記及可用的修復幫助。工具集成可使用：

```sh
ku check --json hello.ku
```

輸出格式是 **JSON Lines**，每個診斷一行，字段包含 `level`、`code`、`message`、`file`、`line`、`column`、`endLine`、`endColumn`、`notes`、`helps`。成功時靜默，不能因沒有 JSON 輸出就判斷檢查未執行。

| 診斷    | 常見原因                     | 處理方式                                                 |
| ------- | ---------------------------- | -------------------------------------------------------- |
| `E0105` | 使用了 `let`                 | 改為首次賦值聲明變數                                     |
| `E0302` | `if` / `while` 條件不是 bool | 寫出明確的布林比較                                       |
| `E0601` | 標準函式庫模組名未知         | 核對 `std.json` 這類真實模組路徑                         |
| `E0605` | 缺少必須的顯式 import        | 添加對應模組匯入，例如 `import fs from "std.fs"`         |
| `E0901` | 讀取了已經 move 的值         | 需要獨立值時顯式 clone，或在支援的同步呼叫中使用唯讀借用 |
| `E0913` | async 函式聲明 borrowed 參數 | 改為 owned 參數；需要保留原值時由呼叫者 clone            |
| `E0918` | 在呼叫處寫 `&value`          | 呼叫寫普通實參，借用模式由函式簽名聲明                   |

這些是已登記診斷的摘要，不是完整錯誤碼清單。目前唯一登記來源是 [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/error.rs)。

## 可恢復錯誤與編譯器診斷不同

執行期 `Error` 的 `domain/code` 不同於編譯器 `E` 編號。例如非法 JSON 回傳 `json/parse_error`，物件缺鍵可回傳 `object/missing_key`。在回傳 `T!` 的呼叫後使用 `?`，或通過 `try/catch` 讀取錯誤字段。

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

範例故意觸發 JSON 解析錯誤並捕獲，程序正常退出。解析錯誤的 `byte N` 為從 0 開始的 UTF-8 位元組位置，中文或 emoji 前後的位元組偏移不等於字符數。

## native 編譯被拒絕

`ku check` 成功只表示前端檢查通過。native C 和 LLVM 各自有能力邊界；遇到明確的不支援診斷，先查看[支援矩陣](/zh-tw/docs/support/)，不要修改超時或以別的後端成功替代驗證。

`ku build --native file.ku` 不帶 `-o` 時僅寫 C 文件。希望編譯和鏈接二進制時使用 `ku build --backend c file.ku`，並確認目標 C 工具鏈存在。

資料庫的 `execution_unknown` / `execution_completed_without_result` 表示不能安全自動重放；保留請求和業務證據，先核對真實執行狀態。官網本地驗證不需要連接生產資料庫。

更多規則見[倉庫診斷說明](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/diagnostics.md)。
