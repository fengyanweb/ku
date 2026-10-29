---
title: 'Note di versione'
description: 'Ku 0.0.18 — Note di versione'
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

La documentazione attuale riguarda Ku 0.0.19, una versione preliminare sperimentale con la stessa versione per CLI ed estensione VS Code. Disponibilità, file e checksum dipendono dal GitHub Release effettivo.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/it/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18

Corregge la valutazione ripetuta del ricevente nelle catene di chiamata, le costanti float C native e `time.date(Time): Date!`, aggiornando l’estensione VS Code.

- JSON: `encode`/`decode` condividono `stringify`/`parse`, con la correzione di andata e ritorno di `1e20`.

[Scarica Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Supporto di piattaforme e backend](/it/v0.0.18/docs/support/)

## v0.0.17 · Versione preliminare sperimentale

Modifiche principali:

- I parametri sincroni presi in prestito in sola lettura usano `&name: T`, con argomenti di chiamata normali. I normali parametri posseduti mantengono la semantica di spostamento.
- Il sottoinsieme del parser self-hosted aggiunge `while` con parentesi graffe, con limiti di annidamento e numero di istruzioni.
- Gli archivi includono dipendenze Rust corrispondenti per il runner predefinito e un pacchetto TLS di destinazione. Restano necessarie toolchain esterne appropriate.

I download coprono Windows x64, Linux x64 e macOS arm64. I sette file della versione sono tre archivi di piattaforma, tre manifest e `SHA256SUMS`. Consulta l'[installazione](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) e le [note complete della versione](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilità e limiti

Ku resta sperimentale e non è pronto per la produzione. La versione 0.0.x non promette compatibilità del sorgente o dell'ABI C/FFI generata. Ricompila gli artefatti nativi interessati durante l'aggiornamento. Parametri async presi in prestito, self-hosting completo e async nativo completo non rientrano in questa versione.

Il runner predefinito incorpora solo il sorgente di ingresso; i moduli importati richiedono ancora le posizioni originali. I client di database sono solo nativi. Controlla i limiti di server reali, TLS e stato di sessione per ogni driver. Al codice proprio di Ku non è ancora stata concessa una licenza open source.
