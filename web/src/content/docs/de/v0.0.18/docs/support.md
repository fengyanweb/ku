---
title: 'Plattform- und Backend-Unterstützung'
description: 'Unterscheiden Sie Ku-Veröffentlichungsplattformen, Interpreter, Runner, natives C, Task-Teilmenge und LLVM-Unterstützung.'
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
slug: de/v0.0.18/docs/support
---

## Veröffentlichte CLI-Plattformen

Die experimentelle Vorabversion v0.0.18 deckt Windows x64 (MSVC), Linux x64 (GNU) und macOS arm64 (Apple Silicon) ab. Jede Plattform hat ihr eigenes Archiv. Es gibt keine einzelne ausführbare Datei für alle drei Systeme. Weitere Plattformdateien wurden nicht verifiziert; siehe [Installation](/de/v0.0.18/docs/install/).

## Ausführungspfade

| Fähigkeit                                                    | Interpreter `ku run`                | Standard-Runner `ku build`                  | Synchrones natives C                                       | Native Tasks (Teilmenge)                                  | LLVM-Textprototyp                |
| ------------------------------------------------------------ | ----------------------------------- | ------------------------------------------- | ---------------------------------------------------------- | --------------------------------------------------------- | -------------------------------- |
| Sprachgrundlagen, Funktionen, Kontrollfluss                  | Implementiert                       | Interpreterpfad                             | Teilmenge implementiert                                    | Eingeschränkte typisierte Teilmenge                       | Begrenzter Prototyp              |
| JSON                                                         | Unterstützt                         | Unterstützt                                 | Unterstützt                                                | Nicht unterstützt                                         | Nicht unterstützt                |
| Vollständiger lokaler Importgraph ohne Quelltextabhängigkeit | Lädt Quelltext                      | Nicht unterstützt; nur Einstieg eingebettet | Bei der Generierung erweitert                              | Innerhalb der unterstützten Teilmenge erweitert           | Keine Bereitstellungszusage      |
| async / await                                                | Mit Ressourcengrenzen implementiert | Interpreterpfad                             | Spezielle Teilmenge oder Ablehnung                         | Teilweise, kein vollständiges async                       | Keine vollständige Unterstützung |
| PostgreSQL-/Redis-/MySQL-Clients                             | Kein Datenbankzugriff               | Kein interpretierter Datenbankzugriff       | Nur nativ                                                  | Kein generisches Task-Warten                              | Nicht unterstützt                |
| TCP-/TLS-Client                                              | Kein entsprechender nativer Pfad    | Kein entsprechender nativer Pfad            | Generierte C-Implementierung; TLS benötigt passendes Paket | Keine Zusage für vollständiges generisches Netzwerkwarten | Nicht unterstützt                |

Standard-Builds benötigen die passende Rust-Toolchain und den Linker. Natives C benötigt eine C-Toolchain für das Zielsystem. Ein erfolgreiches `ku check` beweist für keinen der beiden Builds die Funktionsfähigkeit. Ein LLVM-Befehl bedeutet kein vollständiges Veröffentlichungs-Backend.

## Die native Task-Teilmenge

Die Version enthält die begrenzte native C-Quelle `async fn main(): null!`: aufgeführte primitive Typen, Strings, flache Arrays, einstufige Results, Task-Start/Move/Await, ausgewählten Kontrollfluss und begrenzte Hintergrundoperationen für Dateien und Verzeichnisse. Lokale TCP/TLS-Operationen nutzen einen festen Blocking-Pool. Dies ist weder allgemeine Netzwerk-Readiness noch vollständiges M:N-Scheduling; maßgeblich ist die Nebenläufigkeitsreferenz dieser Version.

Generisches Task-Netzwerkwarten, vollständige M:N-Ausführung, beliebige dynamische Heap-Kombinationen, alle async-Capture-/Funktionskombinationen und vollständige Langzeittests sind noch unvollendet. Die genau akzeptierte und abgelehnte Teilmenge steht im [Nebenläufigkeitsvertrag](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Datenbank- und Netzwerkgrenzen

Datenbank-APIs verwenden `module.client(config)?`, gefolgt von Empfängermethoden, mit einem begrenzten Pool im Client. Echte Servervalidierung erfolgt separat für jeden Treiber, jedes System und jeden Commit. Eine simulierte ABI oder erfolgreiches Linken ersetzt keine echte Abfrage.

Redis verwendet unverschlüsseltes TCP. Bei MySQL fehlt eine konsistente, im Fehlerfall schließende Zertifikatsprüfung über Oracle/MariaDB hinweg. Direkte Verbindungen sollten daher Loopback, kontrollierte private Netze oder einen verifizierten TLS-Tunnel verwenden. Entferntes PostgreSQL erfordert explizites `sslmode=verify-full` und vertrauenswürdige Stammzertifikate. Natives TLS in `std.net` bedeutet nicht, dass HTTP, Redis oder der Interpreter denselben Pfad verwenden.

## Self-Hosting und Kompatibilität

Kus selbst gehosteter Lexer und Parser besitzen begrenzte Teilmengen und differenzielle Prüfungen. Ein vollständiger selbst gehosteter Parser, Checker, IR und Backend sind noch nicht fertig; Rust liefert weiterhin die Bootstrap-Toolchain. Version 0.0.x verspricht weder Quelltextkompatibilität noch Kompatibilität der generierten C-/FFI-ABI.

## Umfang und Grenzen von v0.0.18

- JSON: `encode`/`decode` teilen `stringify`/`parse`, einschließlich der `1e20`-Roundtrip-Korrektur.
- Native Tasks bleiben die aufgeführte begrenzte Teilmenge; Shutdown-Zulassung und Bereinigung bedeuten kein vollständiges M:N oder allgemeines Netzwerksuspendieren.
- HTTP bietet begrenzte Socket-Readiness, Queues, Antworten und Keep-alive sowie optionale begrenzte Diagnostik; dies ist keine vollständige Lastabnahme.
- `ku supervise` kontrolliert ganze Programm-Worker, Zulassung, Queue-Bytes, Ausgabe und Bereinigung; kein Worker pro Anfrage oder Task und keine automatische Anwendung auf `ku run`.

**Bekanntes HTTP-Problem:** Zeitweise können HTTP-504-Fehler auftreten. Die Ursache ist noch nicht geklärt.

### Prozessspeichergrenzen

| Prozessspeichergrenzen | `ku supervise`                                                                                                                                                                                                                                                     |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Linux                  | Abgerechneter cgroup-v2-Kernelspeicher; benötigt beschreibbare, delegierte Domain-Root mit `memory` und `pids` sowie `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max`, `cgroup.kill`. Fehlende Rechte oder Kontrollen verhindern den Programmstart. |
| Windows                | Verschachtelte Job Objects begrenzen Worker und Gruppe nach zugesagtem virtuellem Speicher, nicht RSS. Initialer Commit wird vor Freigabe des angehaltenen Anwendungsthreads geprüft; Host-Richtlinien können die Job-Zuweisung verweigern.                        |
| macOS                  | Keine native Unterstützung dieses Isolationsmodus; Ablehnung vor Programmstart. Bei Bedarf passend konfigurierten Linux-Host oder VM nutzen; keine automatische VM-Weiterleitung.                                                                                  |

Die Worker-Ressourcendomäne erfasst Ku- und Drittbibliotheksallokationen. Linux-cgroup-Abrechnung und Windows-Commit sind unterschiedliche Maße. `memory.max` kann kurzzeitige Überschreitungen erlauben; eine absolute Obergrenze für jeden physischen Speicher zu jedem Zeitpunkt wird nicht garantiert.

Der Koordinator hat kein hartes OS-Speicherlimit; Reserven und Zulassungsbuchhaltung sind kein präzises RSS-Limit. Der gesamte Heap einzelner Tasks wird nicht exakt gemessen; Pools, Caches und Bibliotheksallokationen lassen sich nicht genau einzelnen Tasks zuordnen. Externe Datenbanken, Host/VM, Compiler und normales `ku run` unterliegen nicht automatisch dem Worker-Limit.

Beendet das OS einen Worker, meldet der Koordinator Fehler und gibt Reserven erst nach bestätigter Bereinigung frei. Manche Allokationsfehler werden vorher gemeldet; Erschöpfung in Drittbibliotheken muss kein gewöhnliches Result ergeben. **Datenbankoperationen werden nie automatisch wiederholt**; Worker-Ende beweist keinen Rollback, Nebenwirkungen können unbekannt bleiben.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
