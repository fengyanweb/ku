---
title: 平台與後端支援
description: 區分 Ku 已發佈平台、解釋器、預設 runner、native C、Task 子集與 LLVM 的能力。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'docs/v0.0.18-r1.md',
  ]
slug: zh-tw/v0.0.18/docs/support
---

## 已發佈的 CLI 平台

v0.0.18 實驗預發行包覆蓋 Windows x64（MSVC）、Linux x64（GNU）、macOS arm64（Apple Silicon）。每個平台使用獨立歸檔，沒有一個二進制能通用於三個系統。其他平台未覈實安裝資產，見[安裝與下載](/zh-tw/v0.0.18/docs/install/)。

## 執行方式

| 能力                                   | 解釋器 `ku run`        | 預設 `ku build` runner | 同步 native C                     | 原生 Task（受限子集）  | LLVM 文字原型  |
| -------------------------------------- | ---------------------- | ---------------------- | --------------------------------- | ---------------------- | -------------- |
| 基礎語言、函式與控制流                 | 有實作                 | 使用解釋器路徑         | 已實作子集                        | 有限 typed 子集        | 有限原型       |
| JSON                                   | 支援                   | 支援                   | 支援                              | 不支援                 | 不支援         |
| 完整本地 import graph 的原始碼無關執行 | 從原始碼加載           | 不支援；僅嵌入入口     | 在生成期展開                      | 在已支援子集中展開     | 不作部署承諾   |
| async / await                          | 有實作與資源限制       | 使用解釋器路徑         | 轉入專用子集或拒絕                | 部分支援，非完整 async | 不支援完整能力 |
| PostgreSQL / Redis / MySQL client      | 不支援連庫             | 不提供解釋器連庫       | native-only                       | 未列入通用等待能力     | 不支援         |
| TCP / TLS client                       | 不提供對應 native 路徑 | 不提供對應 native 路徑 | generated C 實作，TLS 需匹配 pack | 非通用網絡等待完成聲明 | 不支援         |

預設 build 要求匹配的 Rust 工具鏈和 linker；native C 要求目標 C 工具鏈。`ku check` 通過不能證明兩種構建都成功。LLVM 命令存在也不等於完整可發佈後端。

## native Task 目前做到哪裡

本版包含受限 `async fn main(): null!` 原生 C 路徑，涵蓋明列的基本型別、字串、平面陣列、單層 Result、Task 啟動／移動／await、部分控制流程，以及有界檔案與目錄背景操作。局部 TCP/TLS 操作可經固定 blocking pool 執行；這不等同通用網路 readiness 或完整 M:N 調度。支援型別與可等待操作仍以本版並行文件為準。

網絡通用 Task 等待、完整 M:N、任意動態堆組合、所有 async 捕獲/函式值組合和完整長期 soak 仍未完成。具體接受與拒絕範圍見[併發合同](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md)。

## 資料庫與網絡邊界

資料庫統一使用 `module.client(config)?` 和 receiver 方法，內部維護有界池。實庫驗證按驅動、系統和目前提交分別驗收，fake ABI 或鏈接成功不能替代真實查詢驗證。

Redis 仍為明文 TCP；MySQL 未提供跨 Oracle/MariaDB 一致的 fail-closed 證書驗證配置。它們只應直連 loopback、受控私網或經驗證的 TLS tunnel。PostgreSQL 遠程連接需要顯式 `sslmode=verify-full` 與可信根證書。`std.net` 的 native TLS 實作不代表 HTTP、Redis 或解釋器已經復用它。

## 自舉與版本兼容

Ku 自舉 Lexer 和 Parser 已有有界子集及差分驗證；完整 Parser、checker、IR 與 backend 的自舉尚未完成，Rust 仍承擔 bootstrap 工具鏈。0.0.x 不承諾原始碼或生成 C/FFI ABI 向後兼容。

## v0.0.18 的支援範圍與限制

- JSON: `encode`／`decode` 與 `stringify`／`parse` 共用實作，包含 `1e20` 浮點往返修正。
- 原生 Task 保持明列的有界子集；退出准入與清理修正不等於完整 M:N 或通用網路掛起。
- HTTP 提供有界 socket readiness、佇列、回應處理與 keep-alive，以及可選的有界診斷；不代表所有負載已完成驗收。
- `ku supervise` 管理完整程式 worker、准入、佇列位元組、輸出與清理；不是每個請求或 Task 一個 worker，也不自動套用到普通 `ku run`。

**HTTP 已知問題：** 服務可能間歇出現 HTTP 504，原因尚未確認。

### 程序記憶體界線

| 程序記憶體界線 | `ku supervise`                                                                                                                                                                                                            |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux          | 依 cgroup v2 核心計費量限制；須有可寫且已委派的 domain root，啟用 `memory`、`pids`，並提供 `memory.max`、`memory.swap.max`、`memory.oom.group`、`pids.max`、`cgroup.kill`。缺少控制項或權限時，在啟動應用程式前拒絕執行。 |
| Windows        | 以巢狀 Job Object 限制個別 worker 與群組的提交虛擬記憶體，不是 RSS。解除暫停應用執行緒前先檢查初始 commit；宿主策略可能拒絕指派 Job。                                                                                     |
| macOS          | 原生不支援此隔離模式，在啟動應用程式前拒絕。需要時使用已正確設定的 Linux 主機或 VM；不會自動轉送到 VM。                                                                                                                   |

worker 資源域計費包含其 Ku 與第三方函式庫配置。Linux cgroup 計費與 Windows commit 是不同指標；`memory.max` 可能容許瞬間超限，不保證所有實體記憶體在任何時刻都絕不超限。

協調器本身沒有 OS 記憶體硬限制，控制預留與准入帳本也不是精確 RSS。單一 Task 的完整 heap 未精確計量，共享連線池、快取與函式庫配置無法逐 Task 精確歸屬。外部資料庫、宿主／VM、建置器及普通 `ku run` 不會自動受 worker 額度約束。

OS 限制終止 worker 時，協調器報告失敗，確認清理完成後才歸還預留額度。部分配置失敗可提前回報，但第三方記憶體耗盡不保證轉成普通 Result。**資料庫操作不會自動重試**；worker 退出不代表已回滾，副作用可能仍未知。

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
