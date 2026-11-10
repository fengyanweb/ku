---
title: 'Installation und Downloads'
description: 'Laden Sie Ku 0.0.20 für Ihren Computer herunter. Jedes Paket enthält das Kommandozeilenprogramm, den Interpreter und die VS-Code-Erweiterung.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Laden Sie **Ku 0.0.20** für Ihren Computer herunter. Jedes Paket enthält das Kommandozeilenprogramm, den Interpreter und die VS-Code-Erweiterung.

## 1. Ku herunterladen

| System  | Architektur           | Download                                                                                                                          |
| ------- | --------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / ARM64 | [Download macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [Download Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [Download Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## 2. Entpacken und starten

Entpacken Sie das **gesamte Paket** in einen leeren Ordner, zum Beispiel `ku` in Ihrem Benutzerverzeichnis. Lassen Sie alle Dateien zusammen; verschieben Sie nicht nur die ausführbare Datei. Öffnen Sie ein Terminal in diesem Ordner.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

Die Ausgabe sollte `ku 0.0.20` sein. Sie können jetzt `.ku`-Dateien ausführen und prüfen. Dafür müssen Sie Rust nicht installieren.

<details>
<summary>Optional: ku aus jedem Ordner aufrufen</summary>

Wenn Sie nach `~/ku` entpackt haben, ergänzen Sie diese Zeile in der Startdatei Ihrer Shell, etwa `~/.zshrc` oder `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

Öffnen Sie unter Windows „Umgebungsvariablen für dieses Konto bearbeiten“, bearbeiten Sie den Benutzerwert `Path` und ergänzen Sie den vollständigen Pfad des entpackten Ordners, etwa `%USERPROFILE%\ku`. Behalten Sie vorhandene Einträge bei.

Öffnen Sie ein neues Terminal und führen Sie `ku --version` aus.

</details>

## 3. VS-Code-Erweiterung installieren

Wählen Sie in der Erweiterungsansicht von VS Code **… → Aus VSIX installieren…**, öffnen Sie `ku-language-0.0.20.vsix` aus dem entpackten Ordner und laden Sie das Fenster neu.

Falls Ku nicht im `PATH` liegt, setzen Sie `ku.executablePath` auf den vollständigen Pfad zu `ku` beziehungsweise `ku.exe` unter Windows.

<details>
<summary>Download prüfen</summary>

Vergleichen Sie die SHA-256-Prüfsumme des Pakets mit [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS). Die [Veröffentlichungsseite](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) enthält für jedes Paket auch eine `.manifest.json` mit Dateiprüfsummen. Weitere Hinweise stehen in `README-INSTALL.md` im Paket.

</details>

<details>
<summary>Eigene Ku-Programme kompilieren</summary>

`ku run` und `ku check` benötigen kein Rust. Der Standard-Build mit `ku build` benötigt einen System-Linker und die exakte Rust-Compiler-Identität aus dem installierten SDK; v0.0.20-Pakete verwenden **Rust 1.99.1**. `ku build --backend c` benötigt C-Compiler und Linker der Zielplattform. Native Datenbankprogramme benötigen zusätzlich passende, nicht mitgelieferte Clientbibliotheken.

</details>

<details>
<summary>Deinstallieren</summary>

Sichern Sie eigene Quelldateien und Einstellungen. Entfernen Sie dann den entpackten Ordner, Ihren ergänzten `PATH`-Eintrag und die VS-Code-Erweiterung.

</details>

[Weiter: Ihr erstes Programm ausführen](/de/docs/getting-started/)
