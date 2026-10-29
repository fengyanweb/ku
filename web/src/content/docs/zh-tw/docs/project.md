---
title: 專案與貢獻
description: Ku 專案狀態、公開原始碼、授權邊界與提交問題的方式。
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Ku 是程式語言與工具鏈，提供解釋器、預設可執行 runner，以及僅涵蓋受支援子集的原生 C 編譯路徑。

## 原始碼與授權

Ku 與 VS Code 擴充套件採用 **Apache-2.0** 授權條款。可查看[原始碼](https://github.com/fengyanweb/ku)、[0.0.19 發行套件](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)與[授權條款全文](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE)。

第三方元件仍遵循各自授權條款；發行套件內保留了相應授權條款與聲明。

## 提交問題

通過[倉庫 Issues](https://github.com/fengyanweb/ku/issues)提交可復現問題。報告盡量包含：

電子郵件回報: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku 版本、Git commit（使用開發原始碼時）和目標系統 / 架構。
- 完整命令、後端、最小 `.ku` 原始碼。
- 完整診斷或結構化 Error、預期與實際輸出。
- 涉及構建時的 Rust / C 工具鏈版本，以及是否存在本地 import。

請勿提交 token、密碼、真實連接字串或含敏感業務資料的日誌。性能問題應同時記錄環境、負載、測量方法和原始結果；一次範例成功不能代表生產吞吐。

## 貢獻前先讀

修改請保持聚焦，附上最小重現、影響範圍與驗證結果。原始碼、文件及範例應使用同一版本；貢獻由維護者審核後合併。

本站不提供 Ku 程式碼的線上執行服務。

繼續閱讀[快速開始](/zh-tw/docs/getting-started/)、[支援範圍](/zh-tw/docs/support/)與[發佈記錄](/zh-tw/releases/)。
