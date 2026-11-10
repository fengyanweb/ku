---
title: 'Riferimento JSON'
description: 'Codifica e decodifica JSON in Ku, conversione KuValue, codici di errore e supporto dei backend.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
slug: it/v0.0.18/docs/stdlib/json
---

`std.json` codifica e decodifica JSON. `json.encode` prende l’ingresso in prestito in sola lettura; `json.decode` produce un `KuValue` con proprietà indipendente. Entrambi restituiscono Result. `stringify`, `parse` e `try_parse` rimangono compatibili.

## Importazione e interfacce

```ku
import json from "std.json"
```

| Interfaccia                  | Restituisce | Significato                                                       |
| ---------------------------- | ----------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`      | Codifica in sola lettura in nuovo testo JSON compatto             |
| `json.parse(text:str)`       | `KuValue!`  | Parsing rigoroso in un valore dinamico con proprietà indipendente |
| `json.try_parse(text:str)`   | `KuValue!`  | Alias di compatibilità di `parse`                                 |
| `json.encode(&value:any)`    | `str!`      | Alias di `stringify` incluso in questa versione                   |
| `json.decode(text:str)`      | `KuValue!`  | Alias di `parse` incluso in questa versione                       |

`!` indica Result. Usa `?` per propagare errori oppure `try/catch` per gestirli. La codifica non consuma né modifica l'input.

## Codifica e leggi valori

Esempio base completo; altri usi in [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

Le chiavi degli oggetti vengono ordinate in modo deterministico. Output atteso:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Un `KuValue` analizzato non può partecipare direttamente all'aritmetica degli interi. Converti prima con `.as_int()?` o `.as_str()?`. Un tipo non corrispondente restituisce un errore. Anche le chiavi di oggetto assenti restituiscono errori, invece di valori predefiniti automatici.

## Dati e limiti

Gli input supportati includono stringhe, booleani, interi, numeri finiti in virgola mobile, null, array annidati, oggetti con chiavi stringa e struct rappresentabili. Non è possibile codificare funzioni, Task, handle di risorse, Results, enum e cicli.

| Regola                      | Implementazione attuale                                                                                  |
| --------------------------- | -------------------------------------------------------------------------------------------------------- |
| Sintassi JSON               | Rigorosa; rifiuta commenti, virgole finali, input aggiuntivo finale e Unicode non valido                 |
| Input / output              | Ciascuno limitato a 1.000.000 di byte                                                                    |
| Budget di profondità        | La radice ha profondità 0; rifiuta `depth > 32`; accetta 33 livelli di contenitori vuoti e ne rifiuta 34 |
| Interi                      | Con segno a 64 bit; l'overflow è un errore, senza conversione automatica a virgola mobile                |
| Virgola mobile              | binary64 finito; rappresentazione originale, etichette float e zero negativo non vengono preservati      |
| Unicode                     | Cinese, emoji, escape, NUL incorporato                                                                   |
| Chiavi di oggetto duplicate | Il parsing conserva l'ultimo valore                                                                      |
| Ordine delle chiavi         | La codifica ordina secondo i byte UTF-8                                                                  |

`1.0` può diventare `1`. Se il testo intero di un float supera int64, il codificatore aggiunge `.0`: `1e20` diventa `100000000000000000000.0` e può essere decodificato di nuovo. I binary64 finiti non conservano scrittura originale, etichetta float o segno dello zero negativo. La correzione è inclusa.

## Errori

Gli errori recuperabili usano il dominio `json`. I codici comuni sono `parse_error`, `stringify_error` e `out_of_memory`. Il `byte N` in un messaggio di parsing è un offset in byte UTF-8 con base zero. Vedi [errori e risoluzione dei problemi](/it/v0.0.18/docs/guide/errors/) per un gestore minimo di input non valido.

## Supporto dei backend

L'interprete, il runner impacchettato predefinito e il backend C nativo sincrono hanno implementazioni JSON. **Il sottoinsieme Task nativo ristretto e LLVM non supportano JSON.** Le importazioni del runner predefinito dipendono ancora dalle posizioni dei sorgenti; il supporto JSON non implica un pacchetto totalmente indipendente.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
