---
title: 'Note di versione'
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

Questa documentazione descrive l’ambito fissato della versione preliminare sperimentale v0.0.18. La pubblicazione effettiva e i download sono registrati in GitHub Releases.

> Gli aggiornamenti v0.0.18 seguono per ora in inglese; il resto della traduzione è mantenuto. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

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

## v0.0.17 · Versione preliminare sperimentale

Pubblicata il **2026-10-18 (America/Los_Angeles)**, ovvero `2026-10-19T03:51:10Z` in UTC. La versione ha `prerelease=true` e `draft=false`. Attualmente non è stata verificata alcuna versione stabile.

Modifiche principali:

- I parametri sincroni presi in prestito in sola lettura usano `&name: T`, con argomenti di chiamata normali. I normali parametri posseduti mantengono la semantica di spostamento.
- Il sottoinsieme del parser self-hosted aggiunge `while` con parentesi graffe, con limiti di annidamento e numero di istruzioni.
- I test multipiattaforma correggono i controlli di disconnessione di stdout. Questo non aggiunge un'API di business.
- Gli archivi includono dipendenze Rust corrispondenti per il runner predefinito e un pacchetto TLS di destinazione. Restano necessarie toolchain esterne appropriate.

I download coprono Windows x64, Linux x64 e macOS arm64. I sette file della versione sono tre archivi di piattaforma, tre manifest e `SHA256SUMS`. Consulta l'[installazione](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) e le [note complete della versione](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilità e limiti

Ku resta sperimentale e non è pronto per la produzione. La versione 0.0.x non promette compatibilità del sorgente o dell'ABI C/FFI generata. Ricompila gli artefatti nativi interessati durante l'aggiornamento. Parametri async presi in prestito, self-hosting completo e async nativo completo non rientrano in questa versione.

Il runner predefinito incorpora solo il sorgente di ingresso; i moduli importati richiedono ancora le posizioni originali. I client di database sono solo nativi. Controlla i limiti di server reali, TLS e stato di sessione per ogni driver. Al codice proprio di Ku non è ancora stata concessa una licenza open source.
