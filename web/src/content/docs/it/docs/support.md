---
title: 'Supporto di piattaforme e backend'
description: 'Distingui piattaforme pubblicate di Ku, interprete, runner, C nativo, sottoinsieme Task e supporto LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Scaricare Ku 0.0.19

Ku 0.0.19 è una versione preliminare sperimentale. Sono disponibili pacchetti per macOS ARM64 (Apple Silicon), Windows x64 e Linux x64 (GNU).

[Pacchetti e istruzioni di installazione](/it/docs/install/)

## Modalità di esecuzione

- **`ku run`**: Interprete per il linguaggio, JSON generale e gestori HTTP sincroni; senza client per database.
- **`ku build`**: Il runner predefinito incorpora il file iniziale; gli import locali richiedono ancora i relativi sorgenti. Sono necessari Rust **1.89.0** e un linker di sistema.
- **`ku build --backend c`**: Supporta JSON sincrono, HTTP con `http.json` e client nativi PostgreSQL, Redis e MySQL. Richiede un compilatore C, un linker e librerie client adatti alla destinazione. Le operazioni sui database non vengono ritentate automaticamente.
- **Task**: Il percorso Task nativo supporta un sottoinsieme tipizzato con avvio/`await`, timer, alcune operazioni di I/O, gestori HTTP async con nome e determinate query PG. In questo percorso JSON può solo codificare oggetti letterali con valori scalari; la decodifica generale non è disponibile. Il M:N completo e la copertura generale dell’I/O non sono ancora terminati.
- **LLVM**: Il backend testuale LLVM rimane un prototipo limitato.

## Problemi noti

I controlli di rilascio e dei pacchetti sono passati su tutte e tre le piattaforme. Non equivalgono a una convalida generale per la produzione.

- **HTTP 504:** si è verificato in passato, ma non è stato riprodotto nell’ultimo controllo breve; la causa non è confermata.
- **Picchi di latenza durante l’annullamento:** nell’ultimo controllo di 30 secondi, tutte le 60.000 richieste brevi erano corrette. Il p99 era **1.557 ms** e il massimo **98.809 ms**; alcune richieste superavano ancora i 20 ms.

La convalida formale di stabilità di 30 minuti non è ancora stata superata. Le misurazioni brevi non la sostituiscono.

Riferimento completo: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Segnalare un problema: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
