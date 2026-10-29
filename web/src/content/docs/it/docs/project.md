---
title: 'Progetto e contributi'
description: 'Stato del progetto Ku, sorgenti pubblici, limiti di licenza e segnalazioni riproducibili.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

La documentazione attuale riguarda Ku 0.0.19, una versione preliminare sperimentale con la stessa versione per CLI ed estensione VS Code. Disponibilità, file e checksum dipendono dal GitHub Release effettivo.

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## Sorgenti e licenza

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku e l’estensione VS Code sono distribuiti con Apache License 2.0. Consulta [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) e [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). I componenti di terze parti mantengono i propri diritti d’autore e le proprie licenze.

## Segnala un problema

Usa gli [Issues](https://github.com/fengyanweb/ku/issues) del repository per segnalazioni riproducibili. Includi:

Feedback via email: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versione di Ku, commit Git per i sorgenti di sviluppo e sistema / architettura di destinazione.
- Comando completo, backend e sorgente `.ku` minimo.
- Diagnostica completa o Error strutturato, output atteso e output effettivo.
- Versioni delle toolchain Rust / C per problemi di compilazione e presenza di importazioni locali.

Non inviare token, password, stringhe di connessione reali o log contenenti dati aziendali sensibili. Le segnalazioni di prestazioni devono includere ambiente, carico di lavoro, metodo di misurazione e risultati grezzi. Un solo esempio riuscito non dimostra il throughput in produzione.

## Prima di contribuire

Il [contratto sintattico](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) e il [contratto semantico](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) definiscono il comportamento del linguaggio. Controlla insieme CI, implementazione e contratti. Un obiettivo del registro di lavoro non è una funzionalità implementata.

Mantieni le modifiche mirate e spiega riproduzione, causa, ambito e validazione. Esamina i contenuti del sito insieme al sorgente corrispondente. Conserva test esistenti, modifiche all'albero di lavoro ed evidenze uniche. Il manutentore decide l'accettazione dei contributi e gli accordi di licenza; questo sito non inventa un accordo di contribuzione.

Questo sito è statico. Testi, navigazione e codice non richiedono un backend aziendale. La ricerca carica un indice Pagefind nel browser; il sito non offre esecuzione remota di codice Ku.

Continua con i [primi passi](/it/docs/getting-started/), i [limiti del supporto](/it/docs/support/) e le [note di versione](/it/releases/).
