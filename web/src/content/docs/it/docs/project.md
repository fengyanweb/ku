---
title: 'Progetto e contributi'
description: 'Stato del progetto Ku, sorgenti pubblici, limiti di licenza e segnalazioni riproducibili.'
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

Ku è un linguaggio e una catena di strumenti: interprete, runner eseguibile predefinito e compilazione C nativa di un sottoinsieme supportato.

## Sorgenti e licenza

Ku e la sua estensione per VS Code sono distribuiti con licenza **Apache-2.0**. Consulta il [codice sorgente](https://github.com/fengyanweb/ku), la [versione 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) e il [testo della licenza](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

I componenti di terze parti mantengono le proprie licenze. I pacchetti includono i testi e gli avvisi corrispondenti.

## Segnala un problema

Usa gli [Issues](https://github.com/fengyanweb/ku/issues) del repository per segnalazioni riproducibili. Includi:

Feedback via email: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versione di Ku, commit Git per i sorgenti di sviluppo e sistema / architettura di destinazione.
- Comando completo, backend e sorgente `.ku` minimo.
- Diagnostica completa o Error strutturato, output atteso e output effettivo.
- Versioni delle toolchain Rust / C per problemi di compilazione e presenza di importazioni locali.

Non inviare token, password, stringhe di connessione reali o log contenenti dati aziendali sensibili. Le segnalazioni di prestazioni devono includere ambiente, carico di lavoro, metodo di misurazione e risultati grezzi. Un solo esempio riuscito non dimostra il throughput in produzione.

## Prima di contribuire

Mantieni le modifiche mirate e includi una riproduzione minima, l’ambito e le verifiche pertinenti. Codice, documentazione ed esempi devono riferirsi alla stessa versione. I responsabili esaminano i contributi prima di integrarli.

Il sito non offre esecuzione online di codice Ku.

Continua con i [primi passi](/it/docs/getting-started/), i [limiti del supporto](/it/docs/support/) e le [note di versione](/it/releases/).
