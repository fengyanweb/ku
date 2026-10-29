---
title: 平台與後端支援
description: 區分 Ku 已發佈平台、解釋器、預設 runner、native C、Task 子集與 LLVM 的能力。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## 下載 Ku 0.0.19

Ku 0.0.19 是實驗預發行版，已提供 macOS ARM64（Apple Silicon）、Windows x64 與 Linux x64（GNU）平台套件。

[套件與安裝步驟](/zh-tw/docs/install/)

## 執行方式

- **`ku run`**: 直譯器支援語言功能、通用 JSON 與同步 HTTP handler；未提供資料庫用戶端。
- **`ku build`**: 預設 runner 嵌入入口檔案，本地 import 仍需要原始碼檔案。需要 Rust **1.89.0** 和系統連結器。
- **`ku build --backend c`**: 支援同步 JSON、包含 `http.json` 的 HTTP，以及 native PostgreSQL、Redis、MySQL 用戶端。需要目標 C 編譯器、連結器及相符的用戶端函式庫。資料庫操作不會自動重試。
- **Task**: native Task 支援帶型別的子集，包括啟動/`await`、計時器、部分 I/O、具名 async HTTP handler 與特定 PG 查詢。此路徑的 JSON 僅能編碼含純量值的物件字面值，不提供通用解碼。完整 M:N 與通用 I/O 支援尚未完成。
- **LLVM**: LLVM 文字後端仍是有限原型。

## 已知問題

三平台發行檢查與套件驗證已通過，但不代表所有正式環境用途均已驗收。

- **HTTP 504：** 歷史上曾發生，最新短時間複查未重現，原因尚未確認。
- **取消期間的延遲尖峰：** 最新 30 秒複查中，60,000 個短請求全部正確；p99 為 **1.557 ms**，最大值為 **98.809 ms**，仍有少量請求超過 20 ms。

正式 30 分鐘穩定性驗收尚未通過，短時間測量不能替代此項驗收。

完整參考: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

回報問題: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
