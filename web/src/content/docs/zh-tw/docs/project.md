---
title: 專案與貢獻
description: Ku 專案狀態、公開原始碼、授權邊界與提交問題的方式。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

目前文件對應 Ku 0.0.19 實驗預發行版，CLI 與 VS Code 擴充功能版本一致。實際公開狀態、下載檔案與校驗值以對應 GitHub Release 為準。

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## 原始碼與授權

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku 與 VS Code 擴充功能採用 Apache License 2.0，詳見 [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) 與 [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE)。第三方元件保留各自的著作權與授權條款。

## 提交問題

通過[倉庫 Issues](https://github.com/fengyanweb/ku/issues)提交可復現問題。報告盡量包含：

- Ku 版本、Git commit（使用開發原始碼時）和目標系統 / 架構。
- 完整命令、後端、最小 `.ku` 原始碼。
- 完整診斷或結構化 Error、預期與實際輸出。
- 涉及構建時的 Rust / C 工具鏈版本，以及是否存在本地 import。

請勿提交 token、密碼、真實連接字串或含敏感業務資料的日誌。性能問題應同時記錄環境、負載、測量方法和原始結果；一次範例成功不能代表生產吞吐。

## 貢獻前先讀

[語法合同](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md)與[語義合同](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md)規定語言行為。CI、實作和規範需一起核對，不能把工作日誌目標當成已實作功能。

修改應盡量小，說明復現、原因、影響和驗證範圍。網站內容與對應原始碼一起審查，已有測試原始碼、工作區改動和獨有證據需要保留。接受貢獻的具體流程與授權安排仍由維護方決定，本站不虛構貢獻協議。

目前官網是靜態站點，正文、導航和代碼無需業務後端。站內搜索在瀏覽器加載 Pagefind 索引；不提供線上 Ku 代碼執行服務。

繼續閱讀[快速開始](/zh-tw/docs/getting-started/)、[支援範圍](/zh-tw/docs/support/)與[發佈記錄](/zh-tw/releases/)。
