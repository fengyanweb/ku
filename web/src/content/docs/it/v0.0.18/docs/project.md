---
title: 'Progetto e contributi'
description: 'Stato del progetto Ku, sorgenti pubblici, limiti di licenza e segnalazioni riproducibili.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
    'docs/v0.0.18-r1.md',
  ]
slug: it/v0.0.18/docs/project
---

Ku è un linguaggio e una catena di strumenti: interprete, runner eseguibile predefinito e compilazione C nativa di un sottoinsieme supportato.

## Sorgenti e licenza

Consulta il [codice su GitHub](https://github.com/fengyanweb/ku) e la [pagina della versione](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Il codice sorgente è visibile pubblicamente, ma al codice proprio di Ku **non è stata concessa una licenza open source**. L'estensione dell'editor è contrassegnata `UNLICENSED`. Pubblicare sorgenti o binari non concede automaticamente una licenza open source.

Gli avvisi di terze parti negli archivi delle versioni si applicano soltanto ai rispettivi componenti. Non stabiliscono permessi di uso, modifica o ridistribuzione di Ku stesso. Ogni autorizzazione necessaria deve provenire esplicitamente dal manutentore; questo sito non crea nuovi impegni di licenza.

## Segnala un problema

Usa gli [Issues](https://github.com/fengyanweb/ku/issues) del repository per segnalazioni riproducibili. Includi:

- Versione di Ku, commit Git per i sorgenti di sviluppo e sistema / architettura di destinazione.
- Comando completo, backend e sorgente `.ku` minimo.
- Diagnostica completa o Error strutturato, output atteso e output effettivo.
- Versioni delle toolchain Rust / C per problemi di compilazione e presenza di importazioni locali.

Non inviare token, password, stringhe di connessione reali o log contenenti dati aziendali sensibili. Le segnalazioni di prestazioni devono includere ambiente, carico di lavoro, metodo di misurazione e risultati grezzi. Un solo esempio riuscito non dimostra il throughput in produzione.

## Prima di contribuire

Per contribuire, indica riproduzione minima, ambito e test pertinenti. Sorgenti e documentazione devono riguardare la stessa versione. Concorda con il responsabile accettazione dei contributi e diritti d’uso, modifica e ridistribuzione di Ku.

Il sito non offre esecuzione online di codice Ku.

Continua con i [primi passi](/it/v0.0.18/docs/getting-started/), i [limiti del supporto](/it/v0.0.18/docs/support/) e le [note di versione](/it/v0.0.18/releases/).
