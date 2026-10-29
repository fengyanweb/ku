---
title: 專案與貢獻
description: Ku 專案狀態、公開原始碼、授權邊界與提交問題的方式。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
    'docs/v0.0.18-r1.md',
  ]
slug: zh-tw/v0.0.18/docs/project
---

Ku 是程式語言與工具鏈，提供解釋器、預設可執行 runner，以及僅涵蓋受支援子集的原生 C 編譯路徑。

## 原始碼與授權

[查看 GitHub 原始碼](https://github.com/fengyanweb/ku)與[發行頁面](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1)。原始碼公開可查看，但 Ku 自身代碼**未授予開放原始碼授權條款**，編輯器擴展標記為 `UNLICENSED`。公開原始碼和二進制發佈不自動授予開放原始碼授權。

發行歸檔中的第三方授權只適用於相應第三方組件，不能據此推斷 Ku 本身的使用、修改或再分發授權。需要相關授權時應由專案維護方明確說明；本站不添加新的授權承諾。

## 提交問題

通過[倉庫 Issues](https://github.com/fengyanweb/ku/issues)提交可復現問題。報告盡量包含：

- Ku 版本、Git commit（使用開發原始碼時）和目標系統 / 架構。
- 完整命令、後端、最小 `.ku` 原始碼。
- 完整診斷或結構化 Error、預期與實際輸出。
- 涉及構建時的 Rust / C 工具鏈版本，以及是否存在本地 import。

請勿提交 token、密碼、真實連接字串或含敏感業務資料的日誌。性能問題應同時記錄環境、負載、測量方法和原始結果；一次範例成功不能代表生產吞吐。

## 貢獻前先讀

貢獻請附上最小重現、修改範圍與相關測試結果。原始碼與文件應使用同一版本；貢獻接受方式及 Ku 本身的使用、修改、再散布授權，請先與維護者確認。

本站不提供 Ku 程式碼的線上執行服務。

繼續閱讀[快速開始](/zh-tw/v0.0.18/docs/getting-started/)、[支援範圍](/zh-tw/v0.0.18/docs/support/)與[發佈記錄](/zh-tw/v0.0.18/releases/)。
