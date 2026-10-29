---
title: 'Plattform- und Backend-Unterstützung'
description: 'Unterscheiden Sie Ku-Veröffentlichungsplattformen, Interpreter, Runner, natives C, Task-Teilmenge und LLVM-Unterstützung.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Ku 0.0.19 herunterladen

Ku 0.0.19 ist eine experimentelle Vorabversion. Verfügbare Pakete: macOS ARM64 (Apple Silicon), Windows x64 und Linux x64 (GNU).

[Pakete und Installationsschritte](/de/docs/install/)

## Ausführungsarten

- **`ku run`**: Interpreter für die Sprache, allgemeines JSON und synchrone HTTP-Handler; keine Datenbankclients.
- **`ku build`**: Der Standard-Runner bettet die Einstiegsdatei ein; lokale Imports benötigen weiterhin ihre Quelldateien. Erfordert Rust **1.89.0** und einen System-Linker.
- **`ku build --backend c`**: Unterstützt synchrones JSON, HTTP einschließlich `http.json` sowie native PostgreSQL-, Redis- und MySQL-Clients. Erfordert einen Ziel-C-Compiler, Linker und passende Clientbibliotheken. Datenbankoperationen werden nicht automatisch wiederholt.
- **Task**: Der native Task-Pfad unterstützt einen typisierten Teilumfang mit Start/`await`, Timern, ausgewählten I/O-Operationen, benannten async-HTTP-Handlern und bestimmten PG-Abfragen. JSON kann dort nur skalare Objektliterale kodieren; allgemeines Dekodieren ist nicht verfügbar. Vollständiges M:N und allgemeine I/O-Abdeckung sind noch nicht fertig.
- **LLVM**: Das LLVM-Text-Backend ist weiterhin ein begrenzter Prototyp.

## Bekannte Probleme

Die Veröffentlichungsprüfungen und Paketprüfungen für alle drei Plattformen sind bestanden. Das ist keine allgemeine Produktionsfreigabe.

- **HTTP 504:** historisch aufgetreten, im neuesten kurzen Nachtest nicht reproduziert; die Ursache ist ungeklärt.
- **Latenzspitzen bei Abbruch:** im neuesten 30-Sekunden-Test waren alle 60.000 kurzen Anfragen korrekt. p99 lag bei **1.557 ms**, das Maximum bei **98.809 ms**; einzelne Anfragen lagen weiterhin über 20 ms.

Die formale 30-Minuten-Stabilitätsabnahme ist noch nicht bestanden. Kurze Messungen ersetzen sie nicht.

Vollständige Referenz: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Probleme melden: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
