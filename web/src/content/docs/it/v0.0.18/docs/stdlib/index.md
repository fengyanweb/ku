---
title: 'Libreria standard'
description: 'Trova moduli della libreria standard Ku, firme essenziali e limiti dei backend.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
    'docs/v0.0.18-r1.md',
  ]
slug: it/v0.0.18/docs/stdlib
---

Il [riferimento di sintassi](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) descrive la libreria standard. Consulta la [matrice di supporto](/it/v0.0.18/docs/support/) per i limiti dei backend.

## Importa moduli

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` e `net` richiedono importazioni esplicite. Alcuni moduli storici possono ancora essere chiamati direttamente; i nuovi esempi preferiscono importazioni esplicite per chiarire le dipendenze. `std.task` è uno spazio dei nomi interno per osservazione e stress test, non un'API di concorrenza applicativa.

| Modulo                               | Scopo                                                  | Limiti da ricordare                                                                                        |
| ------------------------------------ | ------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------- |
| `std.string` / metodi delle stringhe | Lunghezza Unicode, lunghezza in byte, ricerca, sezioni | `trim/lower/upper` nativi non hanno un'implementazione Unicode                                             |
| `std.array` / metodi degli array     | Lunghezza, ricerca, nuovi array, map                   | Il fallimento di un indice diretto è diverso da `try_get`, recuperabile                                    |
| `std.object`                         | Ricerca con ripiego esplicito tramite `get_or`         | Un indice ordinario assente non restituisce null per impostazione predefinita                              |
| [std.json](/it/v0.0.18/docs/stdlib/json/)    | Codifica e decodifica JSON rigorose                    | Non supportate in Task nativi e LLVM                                                                       |
| `std.fs`                             | File, controlli di esistenza, elenchi di directory     | I percorsi relativi si risolvono dalla directory del sorgente `.ku` corrente                               |
| `std.time`                           | Ora, date, durate, attese                              | Attese sincrone e Task usano percorsi di esecuzione diversi                                                |
| `std.http`                           | Client HTTP, servizi, risposte                         | Restano differenze di backend e ciclo di vita; non è un'implementazione completa di rete generica per Task |
| `std.config`                         | Ambiente e configurazione                              | Non inserire credenziali reali nei sorgenti versionati                                                     |
| `std.pg` / `std.redis` / `std.mysql` | Client unificati con pool limitati                     | Solo nativi; validazione con server reali e limiti TLS richiedono controlli separati                       |
| `std.bytes` / `std.net`              | Dati binari, client TCP, TLS opzionale                 | Percorso C nativo generato; TLS richiede un pacchetto corrispondente alla destinazione                     |
| `std.lexer` / `std.parser`           | Strumenti di debug del compilatore                     | Restituiscono riepiloghi di debug, non strutture pubbliche Token / AST complete                            |

## Firme comuni

Queste sono firme di documentazione. `&` indica una modalità di parametro in sola lettura supportata; `T!` indica un risultato recuperabile:

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` vale 2 e `.byte_len()` vale 7. La lunghezza conta valori scalari Unicode; le sezioni usano indici di carattere invece di byte UTF-8. `array.push` e `array.concat` restituiscono nuovi array senza modificare gli input.

## Leggi un file

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Crea prima un file UTF-8 `message.txt` accanto al sorgente. Un file assente o una lettura fallita restituisce un errore. Questo esempio dipende da quel file di input. `try_read` e `try_write` restano alias compatibili con gli stessi tipi di risultato.

`read_dir` restituisce percorsi ordinati con proprietà indipendente, con un limite di 10.000 voci e 1.000.000 di byte totali dei percorsi. Le operazioni sui file del sottoinsieme Task nativo ristretto usano un pool fisso in background. Questo non dimostra attese di rete generiche tramite lo stesso meccanismo.
