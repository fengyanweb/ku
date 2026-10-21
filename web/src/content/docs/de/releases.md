---
title: 'Versionshinweise'
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

Diese Dokumentation beschreibt den eingefrorenen Umfang der experimentellen Vorabversion v0.0.18. Der tatsächliche Veröffentlichungsstatus und die Downloads stehen in GitHub Releases.

> Die Ergänzungen zu v0.0.18 folgen derzeit auf Englisch; die übrige Übersetzung bleibt erhalten. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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

## v0.0.17 · Experimentelle Vorabversion

Veröffentlicht am **2026-10-18 (America/Los_Angeles)** beziehungsweise `2026-10-19T03:51:10Z` in UTC. Die Veröffentlichung hat `prerelease=true` und `draft=false`. Derzeit wurde keine stabile Veröffentlichung verifiziert.

Wesentliche Änderungen:

- Synchrone schreibgeschützt ausgeliehene Parameter verwenden `&name: T` mit gewöhnlichen Aufrufargumenten. Normale Owned-Parameter behalten ihre Verschiebesemantik.
- Die selbst gehostete Parser-Teilmenge ergänzt `while` mit geschweiften Klammern und begrenzter Verschachtelung sowie Anweisungsanzahl.
- Plattformübergreifende Tests korrigieren Prüfungen auf getrennte stdout-Verbindungen. Dies fügt keine Geschäfts-API hinzu.
- Die Archive enthalten passende Rust-Abhängigkeiten für den Standard-Runner und ein TLS-Paket für das Zielsystem. Geeignete externe Toolchains sind weiterhin erforderlich.

Downloads decken Windows x64, Linux x64 und macOS arm64 ab. Die sieben Veröffentlichungsdateien sind drei Plattformarchive, drei Manifeste und `SHA256SUMS`. Siehe [Installation](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) und die [vollständigen Versionshinweise](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Kompatibilität und Grenzen

Ku bleibt experimentell und ist nicht produktionsreif. Version 0.0.x verspricht weder Quelltextkompatibilität noch Kompatibilität der generierten C-/FFI-ABI. Kompilieren Sie beim Aktualisieren die zugehörigen nativen Artefakte neu. Ausgeliehene async-Parameter, vollständiges Self-Hosting und vollständiges natives async sind nicht Teil dieser Veröffentlichung.

Der Standard-Runner bettet nur den Einstiegsquelltext ein; importierte Module benötigen weiterhin ihre ursprünglichen Orte. Datenbankclients sind nur nativ verfügbar. Prüfen Sie bei jedem Treiber die Grenzen echter Server, TLS und Sitzungszustände. Für Kus eigenen Code wurde weiterhin keine Open-Source-Lizenz erteilt.
