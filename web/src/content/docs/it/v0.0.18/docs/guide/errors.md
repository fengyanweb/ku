---
title: Errori e risoluzione dei problemi
description: Diagnostica errori di installazione, importazione, proprietà, JSON
  e capacità dei backend Ku.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/diagnostics.md
  - docs/syntax.md
  - src/error.rs
  - src/stdlib/json.rs
slug: it/v0.0.18/docs/guide/errors
---

Questa documentazione descrive l’ambito fissato della versione preliminare sperimentale v0.0.18. La pubblicazione effettiva e i download sono registrati in GitHub Releases. Le diagnostiche del compilatore possono cambiare durante lo sviluppo sperimentale. Quando segnali un problema, includi versione CLI, commit Git, backend, comando completo e sorgente minimo.

## Identifica l'eseguibile Ku

```sh
ku --version
ku -h
ku check hello.ku
```

Se `ku` non viene trovato, richiama l'eseguibile dalla directory in cui lo hai estratto. Su Windows usa `ku.exe`. Se alla compilazione predefinita mancano Rust o le librerie di collegamento, controlla i [requisiti di installazione](/it/v0.0.18/docs/install/) e conserva l'intera struttura dell'archivio invece di copiare solo la CLI.

## Leggi le diagnostiche

Il controllo normale riporta file, posizione nel sorgente, estratto di codice e indicazioni di correzione disponibili. Le integrazioni con strumenti possono usare:

```sh
ku check --json hello.ku
```

L'output è in formato **JSON Lines**, con una diagnostica per riga. I campi includono `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` e `helps`. In caso di successo non viene emesso nulla; l'assenza di output JSON non significa che il controllo non sia stato eseguito.

| Diagnostica | Causa comune                                 | Cosa fare                                                                             |
| ----------- | -------------------------------------------- | ------------------------------------------------------------------------------------- |
| `E0105`     | Uso di `let`                                 | Dichiarare la variabile tramite la prima assegnazione                                 |
| `E0302`     | Condizione `if` / `while` non bool           | Scrivere un confronto booleano esplicito                                              |
| `E0601`     | Modulo sconosciuto della libreria standard   | Controllare il percorso reale del modulo, per esempio `std.json`                      |
| `E0605`     | Manca un'importazione esplicita obbligatoria | Importare il modulo, per esempio `import fs from "std.fs"`                            |
| `E0901`     | Lettura di un valore spostato                | Clonare esplicitamente un valore indipendente o usare il prestito sincrono supportato |
| `E0913`     | Parametro async preso in prestito            | Usare un parametro con proprietà; clonare nel chiamante se serve l'originale          |
| `E0918`     | Scrittura di `&value` nel punto di chiamata  | Passare un argomento normale; la firma dichiara il prestito                           |

Questa è una sintesi delle diagnostiche registrate, non l'elenco di tutti gli errori. Il registro autorevole attuale è [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/error.rs).

## Gli errori di esecuzione seguono un contratto separato

I valori `domain/code` di Error a runtime sono separati dai codici `E` del compilatore. Un JSON non valido può restituire `json/parse_error`; una chiave di oggetto assente può restituire `object/missing_key`. Usa `?` dopo una chiamata che può fallire oppure leggine i campi in `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

Questo programma didattico provoca intenzionalmente un errore di parsing recuperabile. Il `byte N` riportato è un offset in byte UTF-8 con base zero. Non è un conteggio di caratteri in prossimità di testo cinese o emoji.

## Una compilazione nativa viene rifiutata

Superare `ku check` dimostra soltanto il successo dei controlli del frontend. C nativo e LLVM hanno limiti di capacità distinti. Consulta la [matrice di supporto](/it/v0.0.18/docs/support/) quando un costrutto viene rifiutato esplicitamente. Aumentare un timeout o riuscire con un altro backend non verifica il percorso non supportato.

`ku build --native file.ku` senza `-o` scrive soltanto C. Per compilare e collegare un eseguibile, usa `ku build --backend c file.ku` e installa la toolchain C per la destinazione.

Gli errori di database `execution_unknown` e `execution_completed_without_result` non devono provocare una ripetizione automatica. Conserva le evidenze della richiesta e dell'operazione di business e determina prima lo stato di esecuzione reale. Verificare il sito locale non richiede una connessione a un database di produzione.

Per maggiori dettagli, consulta la [guida alle diagnostiche del repository](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/diagnostics.md).
