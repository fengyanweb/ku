---
title: 'Projekt und Beiträge'
description: 'Projektstatus von Ku, öffentlicher Quelltext, Lizenzgrenzen und reproduzierbare Fehlermeldungen.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
    'docs/v0.0.18-r1.md',
  ]
slug: de/v0.0.18/docs/project
---

Ku ist eine Programmiersprache mit Toolchain: Interpreter, ausführbarer Standard-Runner und native C-Kompilierung einer unterstützten Teilmenge.

## Quelltext und Lizenzierung

Sehen Sie sich den [Quelltext auf GitHub](https://github.com/fengyanweb/ku) und die [Veröffentlichungsseite](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) an. Der Quelltext ist öffentlich einsehbar, aber für Kus eigenen Code wurde **keine Open-Source-Lizenz erteilt**. Die Editor-Erweiterung ist als `UNLICENSED` gekennzeichnet. Die Veröffentlichung von Quelltext oder Binärdateien gewährt nicht automatisch eine Open-Source-Lizenz.

Drittanbieterhinweise in Veröffentlichungsarchiven gelten nur für die jeweiligen Komponenten. Sie gewähren keine Rechte zur Nutzung, Änderung oder Weitergabe von Ku selbst. Jede erforderliche Genehmigung muss ausdrücklich vom Maintainer kommen; diese Website begründet keine neue Lizenzzusage.

## Ein Problem melden

Verwenden Sie für reproduzierbare Meldungen die [Issues](https://github.com/fengyanweb/ku/issues) des Repositorys. Geben Sie Folgendes an:

- Ku-Version, bei Entwicklungsquellen den Git-Commit sowie Zielsystem und Architektur.
- Vollständigen Befehl, Backend und minimalen `.ku`-Quelltext.
- Vollständige Diagnose oder strukturierten Error, erwartete und tatsächliche Ausgabe.
- Bei Build-Problemen die Versionen der Rust-/C-Toolchain und ob lokale Imports beteiligt sind.

Übermitteln Sie keine Tokens, Passwörter, echten Verbindungszeichenfolgen oder Protokolle mit sensiblen Geschäftsdaten. Leistungsberichte sollten Umgebung, Arbeitslast, Messmethode und Rohdaten enthalten. Ein einzelner erfolgreicher Durchlauf belegt keinen Produktionsdurchsatz.

## Vor einem Beitrag

Beschreiben Sie bei Beiträgen Reproduktion, Änderungsumfang und relevante Testergebnisse. Quellcode und Dokumentation müssen denselben Stand betreffen. Annahme von Beiträgen und Rechte zur Nutzung, Änderung und Weitergabe von Ku sind mit dem Maintainer zu klären.

Die Website bietet keine Online-Ausführung von Ku-Code.

Lesen Sie weiter unter [Erste Schritte](/de/v0.0.18/docs/getting-started/), [Unterstützungsgrenzen](/de/v0.0.18/docs/support/) und [Versionshinweise](/de/v0.0.18/releases/).
