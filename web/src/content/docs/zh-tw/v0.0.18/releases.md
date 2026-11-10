---
title: 發佈記錄
description: 'Ku 0.0.18 — 發佈記錄'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
slug: zh-tw/v0.0.18/releases
---

## v0.0.18

修正呼叫鏈接收者重複求值、原生 C 浮點常量及 `time.date(Time): Date!`，並更新 VS Code 外掛。

- JSON: `encode`／`decode` 與 `stringify`／`parse` 共用實作，包含 `1e20` 浮點往返修正。

[下載 Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [平台與後端支援](/zh-tw/v0.0.18/docs/support/)

## v0.0.17 · 實驗預發行版

主要變化：

- 同步唯讀借用參數 `&name: T`，呼叫仍傳普通實參；普通 owned 參數保持 move 規則。
- Ku 自舉 Parser 子集增加帶花括號的 `while`，並保留有界深度與語句數量約束。
- 發行歸檔攜帶預設 runner 構建所需的匹配 Rust 依賴與目標 TLS pack，仍需對應工具鏈。

下載覆蓋 Windows x64、Linux x64、macOS arm64，7 個 Release 資產包括三個平台歸檔、三個 manifest 與 `SHA256SUMS`。參見[安裝與下載](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)和[完整發行說明](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)。

### 兼容性與限制

Ku 仍為實驗專案，不可用於生產；0.0.x 不承諾原始碼及生成 C/FFI ABI 向後兼容。相關 native 產物應隨版本重編譯。async borrowed 參數、完整自舉和完整 native async 不在本次發佈能力內。

預設 runner 只嵌入入口，匯入模組仍依賴原原始碼位置。資料庫為 native-only，實庫、TLS 和 session 狀態邊界應按對應驅動說明核對。專案自身仍未授予開放原始碼授權條款。
