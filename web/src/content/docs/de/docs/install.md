---
title: 'Installation und Downloads'
description: 'Laden Sie die experimentelle Vorabversion Ku 0.0.19 herunter und verstehen Sie Plattform- und Build-Anforderungen.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-21'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> Die Ergänzungen zu v0.0.19 folgen derzeit auf Englisch; die übrige Übersetzung bleibt erhalten. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.19 experimental prerelease**. Use the actual [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) and its assets; documentation alone does not establish completed publication or production readiness.

## Plattform auswählen

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19). `native_ci-*` artifacts are CI examples, not CLI installers.

| System  | Architektur           | Archiv                                                                                                                                                |
| ------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / arm64 | [`ku-v0.0.19-aarch64-apple-darwin.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)         |
| Windows | x64 / MSVC            | [`ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz)     |
| Linux   | x64 / GNU             | [`ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) |

[Download checksums: SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS)

Es wurden keine Archive für weitere Plattformen verifiziert. Unterstützung für Cross-Kompilierung bedeutet nicht, dass für jedes Betriebssystem oder jede Architektur ein veröffentlichtes Paket vorhanden ist.

## Entpacken und prüfen

1. Prüfen Sie das Archiv anhand der `SHA256SUMS` der Veröffentlichung. Eine separate `.manifest.json` beschreibt die Integrität des Archivs und der Dateien. Prüfsummen prüfen die Integrität und sind keine Signaturen des Herausgebers.
2. Entpacken Sie das **gesamte Archiv** in ein leeres Verzeichnis und lesen Sie `README-INSTALL.md`. Behalten Sie die relative Anordnung von `ku`, `libku.rlib`, `deps/` und `native-tls/` bei.
3. Prüfen Sie die Version aus diesem Verzeichnis. Unter macOS / Linux:

```sh
./ku --version
```

Unter Windows PowerShell:

```powershell
.\ku.exe --version
```

Die gemeldete Version sollte `0.0.19` sein. Optional können Sie dieses Verzeichnis zum `PATH` hinzufügen und danach `ku --version` ausführen. Installieren Sie für die VS-Code-Integration die mitgelieferte Datei `ku-language-0.0.19.vsix`.

## Interpreter and VS Code extension

Each platform archive above includes the Ku CLI (`ku run` uses the interpreter) and `ku-language-0.0.19.vsix`. The extension is distributed inside those archives, without a separate VSIX download asset. This website does not provide an in-browser interpreter.

After extraction, choose **… → Install from VSIX…** in the VS Code Extensions view, open `ku-language-0.0.19.vsix`, and reload the window. Alternatively, run this from the extracted directory:

```sh
code --install-extension ./ku-language-0.0.19.vsix
```

The extension also needs the Ku CLI. Add the extracted directory to `PATH`, or set `ku.executablePath` in VS Code to the full path of `ku` (`ku.exe` on Windows).

## Laufzeit- und Build-Anforderungen

| Vorgang                                 | Zusätzliche Anforderungen                                                                                  |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku-CLI für die passende Plattform                                                                          |
| Standardmäßiges `ku build hello.ku`     | Rust **1.89.0** für das Zielsystem und dessen System-Linker; mitgelieferte Rust-Abhängigkeiten beibehalten |
| `ku build --backend c hello.ku`         | Funktionsfähiger C-Compiler und Linker für das Zielsystem                                                  |
| Natives Datenbankprogramm               | Passende externe dynamische Datenbankclientbibliotheken; sie sind nicht enthalten                          |

Der Standard-Runner bettet nur den Einstiegsquelltext ein. Importierte Module werden weiterhin von ihren ursprünglichen Orten geladen. Eine vom Quelltext unabhängige Bereitstellung des vollständigen lokalen Importgraphen erfordert native C-Kompilierung und unterliegt weiterhin den [Grenzen der Backend-Unterstützung](/de/docs/support/).

## Entwicklungsquelltext ausprobieren

Aus einem lokalen Checkout des Repositorys:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.19`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Deinstallation

Bei einer Archivinstallation entfernen Sie das entpackte Verzeichnis, machen hinzugefügte `PATH`-Einträge rückgängig und deinstallieren die VS-Code-Erweiterung. Sichern Sie vorher dort erstellte Quelldateien und Konfigurationen. Ein separates Ein-Klick-Installationsprogramm, eine Installation über Paketmanager oder ein Befehl `ku uninstall` wurde nicht verifiziert.

Als Nächstes: [Ihr erstes Programm ausführen](/de/docs/getting-started/). Quelle des Installationsvertrags: [Veröffentlichungsarchive](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/de/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
