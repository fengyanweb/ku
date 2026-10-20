---
title: 平台與後端支援
description: 區分 Ku 已發佈平台、解釋器、預設 runner、native C、Task 子集與 LLVM 的能力。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

本文檔對應 v0.0.18 實驗預發行版的凍結範圍；實際公開狀態與下載資產以 GitHub Release 為準。

## 已發佈的 CLI 平台

v0.0.18 實驗預發行包覆蓋 Windows x64（MSVC）、Linux x64（GNU）、macOS arm64（Apple Silicon）。每個平台使用獨立歸檔，沒有一個二進制能通用於三個系統。其他平台未覈實安裝資產，見[安裝與下載](/zh-tw/docs/install/)。

## 執行方式

| 能力                                   | 解釋器 `ku run`        | 預設 `ku build` runner | 同步 native C                     | 開發中 native Task     | LLVM 文字原型  |
| -------------------------------------- | ---------------------- | ---------------------- | --------------------------------- | ---------------------- | -------------- |
| 基礎語言、函式與控制流                 | 有實作                 | 使用解釋器路徑         | 已實作子集                        | 有限 typed 子集        | 有限原型       |
| JSON                                   | 支援                   | 支援                   | 支援                              | 不支援                 | 不支援         |
| 完整本地 import graph 的原始碼無關執行 | 從原始碼加載           | 不支援；僅嵌入入口     | 在生成期展開                      | 在已支援子集中展開     | 不作部署承諾   |
| async / await                          | 有實作與資源限制       | 使用解釋器路徑         | 轉入專用子集或拒絕                | 部分支援，非完整 async | 不支援完整能力 |
| PostgreSQL / Redis / MySQL client      | 不支援連庫             | 不提供解釋器連庫       | native-only                       | 未列入通用等待能力     | 不支援         |
| TCP / TLS client                       | 不提供對應 native 路徑 | 不提供對應 native 路徑 | generated C 實作，TLS 需匹配 pack | 非通用網絡等待完成聲明 | 不支援         |

預設 build 要求匹配的 Rust 工具鏈和 linker；native C 要求目標 C 工具鏈。`ku check` 通過不能證明兩種構建都成功。LLVM 命令存在也不等於完整可發佈後端。

## native Task 目前做到哪裡

目前開發分支有受限的 `async fn main(): null!` 原始碼路徑，覆蓋已列出的 primitive、字串、平面陣列、單層 Result、Task start/move/await、部分控制流，以及有界文件/目錄後台操作。它不是已發佈 v0.0.18 的完整 async 後端。

網絡通用 Task 等待、完整 M:N、任意動態堆組合、所有 async 捕獲/函式值組合和完整長期 soak 仍未完成。具體接受與拒絕範圍見[併發合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md)。

## 資料庫與網絡邊界

資料庫統一使用 `module.client(config)?` 和 receiver 方法，內部維護有界池。實庫驗證按驅動、系統和目前提交分別驗收，fake ABI 或鏈接成功不能替代真實查詢驗證。

Redis 仍為明文 TCP；MySQL 未提供跨 Oracle/MariaDB 一致的 fail-closed 證書驗證配置。它們只應直連 loopback、受控私網或經驗證的 TLS tunnel。PostgreSQL 遠程連接需要顯式 `sslmode=verify-full` 與可信根證書。`std.net` 的 native TLS 實作不代表 HTTP、Redis 或解釋器已經復用它。

## 自舉與版本兼容

Ku 自舉 Lexer 和 Parser 已有有界子集及差分驗證；完整 Parser、checker、IR 與 backend 的自舉尚未完成，Rust 仍承擔 bootstrap 工具鏈。0.0.x 不承諾原始碼或生成 C/FFI ABI 向後兼容。

工作日誌和歷史 CI 只證明對應範圍與提交。查看[發佈記錄](/zh-tw/releases/)區分已發行變化和開發內容；本網站未把目前工作分支的全部 CI、生產負載或資料庫驗收寫成已通過。

> 以下 v0.0.18 新增發布資訊暫以英文提供；既有譯文保留。 [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

### Process memory boundary

| Host    | `ku supervise` guarantee and prerequisite                                                                                                                                                                                                                                                                   |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment.                                                          |
| macOS   | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge.                                                                                                           |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
