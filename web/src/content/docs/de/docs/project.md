---
title: 'Projekt und Beiträge'
description: 'Projektstatus von Ku, öffentlicher Quelltext, Lizenzgrenzen und reproduzierbare Fehlermeldungen.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Ku ist eine Programmiersprache mit Toolchain: Interpreter, ausführbarer Standard-Runner und native C-Kompilierung einer unterstützten Teilmenge.

## Quelltext und Lizenzierung

Ku und die VS-Code-Erweiterung stehen unter **Apache-2.0**. Siehe [Quelltext](https://github.com/fengyanweb/ku), [Version 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) und [Lizenztext](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Drittanbieterkomponenten behalten ihre eigenen Lizenzen. Die Veröffentlichungspakete enthalten die zugehörigen Lizenztexte und Hinweise.

## Ein Problem melden

Verwenden Sie für reproduzierbare Meldungen die [Issues](https://github.com/fengyanweb/ku/issues) des Repositorys. Geben Sie Folgendes an:

Feedback per E-Mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku-Version, bei Entwicklungsquellen den Git-Commit sowie Zielsystem und Architektur.
- Vollständigen Befehl, Backend und minimalen `.ku`-Quelltext.
- Vollständige Diagnose oder strukturierten Error, erwartete und tatsächliche Ausgabe.
- Bei Build-Problemen die Versionen der Rust-/C-Toolchain und ob lokale Imports beteiligt sind.

Übermitteln Sie keine Tokens, Passwörter, echten Verbindungszeichenfolgen oder Protokolle mit sensiblen Geschäftsdaten. Leistungsberichte sollten Umgebung, Arbeitslast, Messmethode und Rohdaten enthalten. Ein einzelner erfolgreicher Durchlauf belegt keinen Produktionsdurchsatz.

## Vor einem Beitrag

Halten Sie Änderungen gezielt und liefern Sie eine minimale Reproduktion, den Umfang und passende Prüfergebnisse. Code, Dokumentation und Beispiele sollen dieselbe Version betreffen. Maintainer prüfen Beiträge vor dem Zusammenführen.

Die Website bietet keine Online-Ausführung von Ku-Code.

Lesen Sie weiter unter [Erste Schritte](/de/docs/getting-started/), [Unterstützungsgrenzen](/de/docs/support/) und [Versionshinweise](/de/releases/).
