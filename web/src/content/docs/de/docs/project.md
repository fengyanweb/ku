---
title: 'Projekt und Beiträge'
description: 'Projektstatus von Ku, öffentlicher Quelltext, Lizenzgrenzen und reproduzierbare Fehlermeldungen.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Die aktuelle Dokumentation betrifft Ku 0.0.19, eine experimentelle Vorabversion mit gleicher Versionsnummer für CLI und VS-Code-Erweiterung. Verfügbarkeit, Dateien und Prüfsummen richten sich nach dem tatsächlichen GitHub Release.

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## Quelltext und Lizenzierung

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku und die VS-Code-Erweiterung sind unter Apache License 2.0 lizenziert. Siehe [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) und [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). Drittkomponenten behalten ihre eigenen Urheberrechts- und Lizenzhinweise.

## Ein Problem melden

Verwenden Sie für reproduzierbare Meldungen die [Issues](https://github.com/fengyanweb/ku/issues) des Repositorys. Geben Sie Folgendes an:

Feedback per E-Mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku-Version, bei Entwicklungsquellen den Git-Commit sowie Zielsystem und Architektur.
- Vollständigen Befehl, Backend und minimalen `.ku`-Quelltext.
- Vollständige Diagnose oder strukturierten Error, erwartete und tatsächliche Ausgabe.
- Bei Build-Problemen die Versionen der Rust-/C-Toolchain und ob lokale Imports beteiligt sind.

Übermitteln Sie keine Tokens, Passwörter, echten Verbindungszeichenfolgen oder Protokolle mit sensiblen Geschäftsdaten. Leistungsberichte sollten Umgebung, Arbeitslast, Messmethode und Rohdaten enthalten. Ein einzelner erfolgreicher Durchlauf belegt keinen Produktionsdurchsatz.

## Vor einem Beitrag

Der [Syntaxvertrag](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) und der [Semantikvertrag](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) definieren das Sprachverhalten. Prüfen Sie CI, Implementierung und Verträge gemeinsam. Ein Ziel im Arbeitsprotokoll ist keine implementierte Funktion.

Halten Sie Änderungen eng begrenzt und erläutern Sie Reproduktion, Ursache, Umfang und Validierung. Prüfen Sie Website-Inhalte zusammen mit dem zugehörigen Quelltext. Bewahren Sie vorhandene Tests, Änderungen im Arbeitsverzeichnis und einzigartige Nachweise. Über Annahme von Beiträgen und Lizenzvereinbarungen entscheidet der Maintainer; diese Website erfindet keine Beitragsvereinbarung.

Diese Website ist statisch. Texte, Navigation und Code benötigen kein Geschäfts-Backend. Die Suche lädt einen Pagefind-Index im Browser; die Website führt Ku-Code nicht auf einem entfernten Server aus.

Lesen Sie weiter unter [Erste Schritte](/de/docs/getting-started/), [Unterstützungsgrenzen](/de/docs/support/) und [Versionshinweise](/de/releases/).
