---
title: 'Versionshinweise'
description: 'Ku 0.0.18 — Versionshinweise'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
---

## 0.0.19

Die aktuelle Dokumentation betrifft Ku 0.0.19, eine experimentelle Vorabversion mit gleicher Versionsnummer für CLI und VS-Code-Erweiterung. Verfügbarkeit, Dateien und Prüfsummen richten sich nach dem tatsächlichen GitHub Release.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/de/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18

Die Revision korrigiert wiederholte Empfängerauswertung in Aufrufketten, native C-Gleitkommakonstanten und `time.date(Time): Date!` und aktualisiert die VS-Code-Erweiterung.

- JSON: `encode`/`decode` teilen `stringify`/`parse`, einschließlich der `1e20`-Roundtrip-Korrektur.

[Ku 0.0.18 herunterladen](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Plattform- und Backend-Unterstützung](/de/v0.0.18/docs/support/)

## v0.0.17 · Experimentelle Vorabversion

Wesentliche Änderungen:

- Synchrone schreibgeschützt ausgeliehene Parameter verwenden `&name: T` mit gewöhnlichen Aufrufargumenten. Normale Owned-Parameter behalten ihre Verschiebesemantik.
- Die selbst gehostete Parser-Teilmenge ergänzt `while` mit geschweiften Klammern und begrenzter Verschachtelung sowie Anweisungsanzahl.
- Die Archive enthalten passende Rust-Abhängigkeiten für den Standard-Runner und ein TLS-Paket für das Zielsystem. Geeignete externe Toolchains sind weiterhin erforderlich.

Downloads decken Windows x64, Linux x64 und macOS arm64 ab. Die sieben Veröffentlichungsdateien sind drei Plattformarchive, drei Manifeste und `SHA256SUMS`. Siehe [Installation](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) und die [vollständigen Versionshinweise](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Kompatibilität und Grenzen

Ku bleibt experimentell und ist nicht produktionsreif. Version 0.0.x verspricht weder Quelltextkompatibilität noch Kompatibilität der generierten C-/FFI-ABI. Kompilieren Sie beim Aktualisieren die zugehörigen nativen Artefakte neu. Ausgeliehene async-Parameter, vollständiges Self-Hosting und vollständiges natives async sind nicht Teil dieser Veröffentlichung.

Der Standard-Runner bettet nur den Einstiegsquelltext ein; importierte Module benötigen weiterhin ihre ursprünglichen Orte. Datenbankclients sind nur nativ verfügbar. Prüfen Sie bei jedem Treiber die Grenzen echter Server, TLS und Sitzungszustände. Für Kus eigenen Code wurde weiterhin keine Open-Source-Lizenz erteilt.
