---
title: 'Projekt und Beiträge'
description: 'Projektstatus von Ku, öffentlicher Quelltext, Lizenzgrenzen und reproduzierbare Fehlermeldungen.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

Diese Dokumentation beschreibt den eingefrorenen Umfang der experimentellen Vorabversion v0.0.18. Der tatsächliche Veröffentlichungsstatus und die Downloads stehen in GitHub Releases. Das Projekt heißt **Ku programming language / Ku 编程语言**, die CLI `ku`. Ku ist eine kleine Programmiersprache mit Interpreter in Entwicklung. Die aktuelle öffentliche Version ist die experimentelle Vorabversion v0.0.18.

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

Der [Syntaxvertrag](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) und der [Semantikvertrag](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definieren das Sprachverhalten. Prüfen Sie CI, Implementierung und Verträge gemeinsam. Ein Ziel im Arbeitsprotokoll ist keine implementierte Funktion.

Halten Sie Änderungen eng begrenzt und erläutern Sie Reproduktion, Ursache, Umfang und Validierung. Prüfen Sie Website-Inhalte zusammen mit dem zugehörigen Quelltext. Bewahren Sie vorhandene Tests, Änderungen im Arbeitsverzeichnis und einzigartige Nachweise. Über Annahme von Beiträgen und Lizenzvereinbarungen entscheidet der Maintainer; diese Website erfindet keine Beitragsvereinbarung.

Diese Website ist statisch. Texte, Navigation und Code benötigen kein Geschäfts-Backend. Die Suche lädt einen Pagefind-Index im Browser; die Website führt Ku-Code nicht auf einem entfernten Server aus.

Lesen Sie weiter unter [Erste Schritte](/de/docs/getting-started/), [Unterstützungsgrenzen](/de/docs/support/) und [Versionshinweise](/de/releases/).
