---
title: Progetto e contributi
description: Stato del progetto Ku, sorgenti pubblici, limiti di licenza e
  segnalazioni riproducibili.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - README.md
  - Cargo.toml
  - editors/vscode-ku/package.json
  - docs/v0.0.18.md
  - docs/syntax.md
slug: it/v0.0.18/docs/project
---

Questa documentazione descrive l’ambito fissato della versione preliminare sperimentale v0.0.18. La pubblicazione effettiva e i download sono registrati in GitHub Releases. Il progetto si chiama **Ku programming language / Ku 编程语言**, con CLI denominata `ku`. Ku è un piccolo linguaggio di programmazione e interprete in sviluppo. La versione pubblica attuale è la preliminare sperimentale v0.0.18.

## Sorgenti e licenza

Consulta il [codice su GitHub](https://github.com/fengyanweb/ku) e la [pagina della versione](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). Il codice sorgente è visibile pubblicamente, ma al codice proprio di Ku **non è stata concessa una licenza open source**. L'estensione dell'editor è contrassegnata `UNLICENSED`. Pubblicare sorgenti o binari non concede automaticamente una licenza open source.

Gli avvisi di terze parti negli archivi delle versioni si applicano soltanto ai rispettivi componenti. Non stabiliscono permessi di uso, modifica o ridistribuzione di Ku stesso. Ogni autorizzazione necessaria deve provenire esplicitamente dal manutentore; questo sito non crea nuovi impegni di licenza.

## Segnala un problema

Usa gli [Issues](https://github.com/fengyanweb/ku/issues) del repository per segnalazioni riproducibili. Includi:

* Versione di Ku, commit Git per i sorgenti di sviluppo e sistema / architettura di destinazione.
* Comando completo, backend e sorgente `.ku` minimo.
* Diagnostica completa o Error strutturato, output atteso e output effettivo.
* Versioni delle toolchain Rust / C per problemi di compilazione e presenza di importazioni locali.

Non inviare token, password, stringhe di connessione reali o log contenenti dati aziendali sensibili. Le segnalazioni di prestazioni devono includere ambiente, carico di lavoro, metodo di misurazione e risultati grezzi. Un solo esempio riuscito non dimostra il throughput in produzione.

## Prima di contribuire

Il [contratto sintattico](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) e il [contratto semantico](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definiscono il comportamento del linguaggio. Controlla insieme CI, implementazione e contratti. Un obiettivo del registro di lavoro non è una funzionalità implementata.

Mantieni le modifiche mirate e spiega riproduzione, causa, ambito e validazione. Esamina i contenuti del sito insieme al sorgente corrispondente. Conserva test esistenti, modifiche all'albero di lavoro ed evidenze uniche. Il manutentore decide l'accettazione dei contributi e gli accordi di licenza; questo sito non inventa un accordo di contribuzione.

Questo sito è statico. Testi, navigazione e codice non richiedono un backend aziendale. La ricerca carica un indice Pagefind nel browser; il sito non offre esecuzione remota di codice Ku.

Continua con i [primi passi](/it/v0.0.18/docs/getting-started/), i [limiti del supporto](/it/v0.0.18/docs/support/) e le [note di versione](/it/v0.0.18/releases/).
