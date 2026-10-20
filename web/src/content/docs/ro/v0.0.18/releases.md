---
title: Notele versiunilor
description: 'Ku v0.0.18 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/history.md
  - Cargo.toml
slug: ro/v0.0.18/releases
---

Documentația descrie domeniul fixat al prelansării experimentale v0.0.18. Publicarea efectivă și descărcările sunt înregistrate în GitHub Releases.

> Actualizările v0.0.18 de mai jos sunt momentan în engleză; restul traducerii este păstrat. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

### Process memory boundary

| Host | `ku supervise` guarantee and prerequisite |
| --- | --- |
| Linux | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment. |
| macOS | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge. |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.


## v0.0.17 · Versiune preliminară experimentală

Publicată la **2026-10-18 (America/Los_Angeles)**, adică `2026-10-19T03:51:10Z` în UTC. Versiunea are `prerelease=true` și `draft=false`. Nu a fost verificată în prezent nicio versiune stabilă.

Schimbări principale:

* Parametrii sincroni împrumutați doar pentru citire folosesc `&name: T`, cu argumente obișnuite la apel. Parametrii obișnuiți deținuți păstrează semantica de mutare.
* Submulțimea parserului autoimplementat adaugă `while` cu acolade, cu limite de imbricare și număr de instrucțiuni.
* Testele între platforme corectează verificările de deconectare stdout. Aceasta nu adaugă un API de afaceri.
* Arhivele includ dependențe Rust potrivite pentru lansatorul implicit și un pachet TLS pentru țintă. Instrumentele externe corespunzătoare rămân necesare.

Descărcările acoperă Windows x64, Linux x64 și macOS arm64. Cele șapte fișiere Release sunt trei arhive de platformă, trei manifeste și `SHA256SUMS`. Vezi [instalarea](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) și [notele complete ale versiunii](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilitate și limite

Ku rămâne experimental și nu este pregătit pentru producție. Versiunea 0.0.x nu promite compatibilitate a sursei sau a ABI C/FFI generat. Recompilează artefactele native relevante la actualizare. Parametrii async împrumutați, autoimplementarea completă și async nativ complet nu fac parte din această versiune.

Lansatorul implicit încorporează doar sursa punctului de intrare; modulele importate încă necesită locațiile originale. Clienții de baze de date sunt doar nativi. Verifică serverul real, TLS și limitele stării de sesiune ale fiecărui driver. Codul propriu Ku încă nu are acordată o licență open source.
