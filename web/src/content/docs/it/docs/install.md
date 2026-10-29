---
title: 'Installazione e download'
description: 'Scarica la versione preliminare sperimentale Ku 0.0.19 e comprendi i requisiti di piattaforma e compilazione.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Scarica **Ku 0.0.19** per il tuo computer. Ogni pacchetto di questa versione preliminare sperimentale include lo strumento a riga di comando, l’interprete e l’estensione per VS Code.

## 1. Scaricare Ku

| Sistema | Architettura          | Scarica                                                                                                                          | Dimensione |
| ------- | --------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| macOS   | Apple Silicon / ARM64 | [Scarica macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB    |
| Windows | x64                   | [Scarica Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB    |
| Linux   | x64 / GNU             | [Scarica Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB    |

## 2. Estrarre ed eseguire

Estrai il **pacchetto completo** in una cartella vuota, ad esempio `ku` nella cartella utente. Mantieni insieme tutti i file; non spostare soltanto l’eseguibile. Apri un terminale in questa cartella.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

Dovrebbe apparire `ku 0.0.19`. Puoi già eseguire e controllare i file `.ku`; queste operazioni non richiedono Rust.

<details>
<summary>Facoltativo: usare ku da qualsiasi cartella</summary>

Se hai estratto in `~/ku`, aggiungi questa riga al file di avvio della tua shell, ad esempio `~/.zshrc` o `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

In Windows, apri «Modifica le variabili di ambiente per l’account», modifica il `Path` dell’utente e aggiungi il percorso completo della cartella estratta, ad esempio `%USERPROFILE%\ku`. Conserva le voci esistenti.

Apri un nuovo terminale ed esegui `ku --version`.

</details>

## 3. Installare l’estensione per VS Code

Nella vista Estensioni di VS Code, scegli **… → Installa da VSIX…**, seleziona `ku-language-0.0.19.vsix` dalla cartella estratta e ricarica la finestra.

Se Ku non è nel `PATH`, imposta `ku.executablePath` sul percorso completo di `ku` (`ku.exe` in Windows).

<details>
<summary>Verificare il download</summary>

Confronta lo SHA-256 del pacchetto con [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS). La [pagina della versione](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) offre anche un `.manifest.json` per ogni pacchetto con le somme di controllo dei file. Ulteriori istruzioni sono in `README-INSTALL.md` nel pacchetto.

</details>

<details>
<summary>Compilare i propri programmi Ku</summary>

`ku run` e `ku check` richiedono solo la CLI della piattaforma. Il `ku build` predefinito richiede inoltre Rust **1.89.0** e un linker di sistema. `ku build --backend c` richiede un compilatore C e un linker per la destinazione. I programmi nativi per database richiedono anche le librerie client corrispondenti, non incluse nel pacchetto.

</details>

<details>
<summary>Disinstallare</summary>

Conserva i sorgenti e le configurazioni che hai creato, poi elimina la cartella estratta, la voce aggiunta al `PATH` e l’estensione per VS Code.

</details>

[Prossimo passo: eseguire il primo programma](/it/docs/getting-started/)
