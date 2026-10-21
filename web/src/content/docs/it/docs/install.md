---
title: 'Installazione e download'
description: 'Scarica la versione preliminare sperimentale Ku 0.0.18 e comprendi i requisiti di piattaforma e compilazione.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-21'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> Gli aggiornamenti v0.0.18 seguono per ora in inglese; il resto della traduzione è mantenuto. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) and its assets; documentation alone does not establish completed publication or production readiness.

## Scegli la piattaforma

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). `native_ci-*` artifacts are CI examples, not CLI installers.

| Sistema | Architettura          | Archivio                                                                                                                                                 |
| ------- | --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / arm64 | [`ku-v0.0.18-aarch64-apple-darwin.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/ku-v0.0.18-aarch64-apple-darwin.tar.gz)         |
| Windows | x64 / MSVC            | [`ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz)     |
| Linux   | x64 / GNU             | [`ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz) |

[Download checksums: SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/SHA256SUMS)

Non sono stati verificati archivi per altre piattaforme. Il supporto alla compilazione incrociata non significa che ogni sistema operativo o architettura disponga di un pacchetto pubblicato.

## Estrai e verifica

1. Verifica l'archivio con `SHA256SUMS` della versione. Un `.manifest.json` separato descrive l'integrità dell'archivio e dei file. Le somme di controllo verificano l'integrità; non sono firme dell'editore.
2. Estrai **l'intero archivio** in una directory vuota e leggi `README-INSTALL.md`. Conserva la disposizione relativa di `ku`, `libku.rlib`, `deps/` e `native-tls/`.
3. Controlla la versione da quella directory. Su macOS / Linux:

```sh
./ku --version
```

Su Windows PowerShell:

```powershell
.\ku.exe --version
```

La versione riportata dovrebbe essere `0.0.18`. Se vuoi, aggiungi questa directory a `PATH`, quindi esegui `ku --version`. Installa il file incluso `ku-language-0.0.18.vsix` per l'integrazione con VS Code.

## Interpreter and VS Code extension

Each platform archive above includes the Ku CLI (`ku run` uses the interpreter) and `ku-language-0.0.18.vsix`. The extension is distributed inside those archives, without a separate VSIX download asset. This website does not provide an in-browser interpreter.

After extraction, choose **… → Install from VSIX…** in the VS Code Extensions view, open `ku-language-0.0.18.vsix`, and reload the window. Alternatively, run this from the extracted directory:

```sh
code --install-extension ./ku-language-0.0.18.vsix
```

The extension also needs the Ku CLI. Add the extracted directory to `PATH`, or set `ku.executablePath` in VS Code to the full path of `ku` (`ku.exe` on Windows).

## Requisiti di esecuzione e compilazione

| Operazione                              | Requisiti aggiuntivi                                                                              |
| --------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI Ku per la piattaforma corrispondente                                                          |
| `ku build hello.ku` predefinito         | Rust **1.89.0** per la destinazione e il linker di sistema; conservare le dipendenze Rust incluse |
| `ku build --backend c hello.ku`         | Compilatore e linker C funzionanti per la destinazione                                            |
| Programma nativo per database           | Librerie dinamiche esterne del client database corrispondenti; non sono incluse                   |

Il runner predefinito incorpora solo il sorgente di ingresso. I moduli importati continuano a caricarsi dalle posizioni originali. Il rilascio di un grafo completo di importazioni locali indipendente dal sorgente richiede la compilazione nativa C e resta soggetto ai [limiti di supporto dei backend](/it/docs/support/).

## Prova il codice di sviluppo

Da una copia locale del repository:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Disinstalla

Per un'installazione da archivio, rimuovi la directory estratta, annulla le voci `PATH` aggiunte e disinstalla l'estensione VS Code. Prima conserva i sorgenti e la configurazione che hai creato lì. Non sono stati verificati un programma di installazione separato con un clic, un'installazione tramite gestore di pacchetti o un comando `ku uninstall`.

Passo successivo: [esegui il tuo primo programma](/it/docs/getting-started/). Fonte del contratto di installazione: [archivi delle versioni](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/it/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
