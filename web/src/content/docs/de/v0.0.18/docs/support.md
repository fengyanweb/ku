---
title: Plattform- und Backend-Unterstützung
description: Unterscheiden Sie Ku-Veröffentlichungsplattformen, Interpreter,
  Runner, natives C, Task-Teilmenge und LLVM-Unterstützung.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/syntax.md
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/protocol-foundation.md
  - .github/workflows/native-three-os.yml
slug: de/v0.0.18/docs/support
---

Diese Dokumentation beschreibt den eingefrorenen Umfang der experimentellen Vorabversion v0.0.18. Der tatsächliche Veröffentlichungsstatus und die Downloads stehen in GitHub Releases.

## Veröffentlichte CLI-Plattformen

Die experimentelle Vorabversion v0.0.18 deckt Windows x64 (MSVC), Linux x64 (GNU) und macOS arm64 (Apple Silicon) ab. Jede Plattform hat ihr eigenes Archiv. Es gibt keine einzelne ausführbare Datei für alle drei Systeme. Weitere Plattformdateien wurden nicht verifiziert; siehe [Installation](/de/v0.0.18/docs/install/).

## Ausführungspfade

| Fähigkeit                                                    | Interpreter `ku run`                | Standard-Runner `ku build`                  | Synchrones natives C                                       | Native Tasks in Entwicklung                               | LLVM-Textprototyp                |
| ------------------------------------------------------------ | ----------------------------------- | ------------------------------------------- | ---------------------------------------------------------- | --------------------------------------------------------- | -------------------------------- |
| Sprachgrundlagen, Funktionen, Kontrollfluss                  | Implementiert                       | Interpreterpfad                             | Teilmenge implementiert                                    | Eingeschränkte typisierte Teilmenge                       | Begrenzter Prototyp              |
| JSON                                                         | Unterstützt                         | Unterstützt                                 | Unterstützt                                                | Nicht unterstützt                                         | Nicht unterstützt                |
| Vollständiger lokaler Importgraph ohne Quelltextabhängigkeit | Lädt Quelltext                      | Nicht unterstützt; nur Einstieg eingebettet | Bei der Generierung erweitert                              | Innerhalb der unterstützten Teilmenge erweitert           | Keine Bereitstellungszusage      |
| async / await                                                | Mit Ressourcengrenzen implementiert | Interpreterpfad                             | Spezielle Teilmenge oder Ablehnung                         | Teilweise, kein vollständiges async                       | Keine vollständige Unterstützung |
| PostgreSQL-/Redis-/MySQL-Clients                             | Kein Datenbankzugriff               | Kein interpretierter Datenbankzugriff       | Nur nativ                                                  | Kein generisches Task-Warten                              | Nicht unterstützt                |
| TCP-/TLS-Client                                              | Kein entsprechender nativer Pfad    | Kein entsprechender nativer Pfad            | Generierte C-Implementierung; TLS benötigt passendes Paket | Keine Zusage für vollständiges generisches Netzwerkwarten | Nicht unterstützt                |

Standard-Builds benötigen die passende Rust-Toolchain und den Linker. Natives C benötigt eine C-Toolchain für das Zielsystem. Ein erfolgreiches `ku check` beweist für keinen der beiden Builds die Funktionsfähigkeit. Ein LLVM-Befehl bedeutet kein vollständiges Veröffentlichungs-Backend.

## Die native Task-Teilmenge

Der aktuelle Entwicklungszweig besitzt einen eingeschränkten Quellpfad für `async fn main(): null!`. Er umfasst ausdrücklich aufgeführte primitive Typen, Strings, flache Arrays, einschichtige Results, Task-Start/Verschieben/await, einige Kontrollflusskonstrukte sowie begrenzte Datei- und Verzeichnisoperationen im Hintergrund. Er ist kein vollständiges async-Backend im veröffentlichten Paket v0.0.18.

Generisches Task-Netzwerkwarten, vollständige M:N-Ausführung, beliebige dynamische Heap-Kombinationen, alle async-Capture-/Funktionskombinationen und vollständige Langzeittests sind noch unvollendet. Die genau akzeptierte und abgelehnte Teilmenge steht im [Nebenläufigkeitsvertrag](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Datenbank- und Netzwerkgrenzen

Datenbank-APIs verwenden `module.client(config)?`, gefolgt von Empfängermethoden, mit einem begrenzten Pool im Client. Echte Servervalidierung erfolgt separat für jeden Treiber, jedes System und jeden Commit. Eine simulierte ABI oder erfolgreiches Linken ersetzt keine echte Abfrage.

Redis verwendet unverschlüsseltes TCP. Bei MySQL fehlt eine konsistente, im Fehlerfall schließende Zertifikatsprüfung über Oracle/MariaDB hinweg. Direkte Verbindungen sollten daher Loopback, kontrollierte private Netze oder einen verifizierten TLS-Tunnel verwenden. Entferntes PostgreSQL erfordert explizites `sslmode=verify-full` und vertrauenswürdige Stammzertifikate. Natives TLS in `std.net` bedeutet nicht, dass HTTP, Redis oder der Interpreter denselben Pfad verwenden.

## Self-Hosting und Kompatibilität

Kus selbst gehosteter Lexer und Parser besitzen begrenzte Teilmengen und differenzielle Prüfungen. Ein vollständiger selbst gehosteter Parser, Checker, IR und Backend sind noch nicht fertig; Rust liefert weiterhin die Bootstrap-Toolchain. Version 0.0.x verspricht weder Quelltextkompatibilität noch Kompatibilität der generierten C-/FFI-ABI.

Arbeitsprotokolle und frühere CI-Ergebnisse belegen nur ihren angegebenen Umfang und Commit. Verwenden Sie die [Versionshinweise](/de/v0.0.18/releases/), um veröffentlichte Änderungen von Entwicklungsarbeit zu unterscheiden. Diese Website meldet nicht sämtliche CI-Prüfungen des aktuellen Zweigs, Produktionslasten oder Datenbankabnahmen als bestanden.

> Die Ergänzungen zu v0.0.18 folgen derzeit auf Englisch; die übrige Übersetzung bleibt erhalten. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

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
