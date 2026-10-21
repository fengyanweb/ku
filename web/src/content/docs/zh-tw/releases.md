---
title: 發佈記錄
description: 'Ku v0.0.18 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
  ]
---

本文檔對應 v0.0.18 實驗預發行版的凍結範圍；實際公開狀態與下載資產以 GitHub Release 為準。

> 以下 v0.0.18 新增發布資訊暫以英文提供；既有譯文保留。 [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions.

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

## v0.0.17 · 實驗預發行版

GitHub 發佈於 **2026-10-18（America/Los_Angeles）**，UTC 時間為 `2026-10-19T03:51:10Z`。該版本 `prerelease=true`、`draft=false`；目前沒有已覈實的穩定發行版。

主要變化：

- 同步唯讀借用參數 `&name: T`，呼叫仍傳普通實參；普通 owned 參數保持 move 規則。
- Ku 自舉 Parser 子集增加帶花括號的 `while`，並保留有界深度與語句數量約束。
- 修正跨平台測試工具的 stdout 斷開驗證；這不增加新的業務 API。
- 發行歸檔攜帶預設 runner 構建所需的匹配 Rust 依賴與目標 TLS pack，仍需對應工具鏈。

下載覆蓋 Windows x64、Linux x64、macOS arm64，7 個 Release 資產包括三個平台歸檔、三個 manifest 與 `SHA256SUMS`。參見[安裝與下載](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)和[完整發行說明](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)。

### 兼容性與限制

Ku 仍為實驗專案，不可用於生產；0.0.x 不承諾原始碼及生成 C/FFI ABI 向後兼容。相關 native 產物應隨版本重編譯。async borrowed 參數、完整自舉和完整 native async 不在本次發佈能力內。

預設 runner 只嵌入入口，匯入模組仍依賴原原始碼位置。資料庫為 native-only，實庫、TLS 和 session 狀態邊界應按對應驅動說明核對。專案自身仍未授予開放原始碼授權條款。
