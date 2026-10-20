---
title: 'Riferimento JSON'
description: 'Codifica e decodifica JSON in Ku, conversione KuValue, codici di errore e supporto dei backend.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
  ]
---

> Gli aggiornamenti v0.0.18 seguono per ora in inglese; il resto della traduzione è mantenuto. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## Importazione e interfacce

```ku
import json from "std.json"
```

| Interfaccia                  | Restituisce | Significato                                                       |
| ---------------------------- | ----------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`      | Codifica in sola lettura in nuovo testo JSON compatto             |
| `json.parse(text:str)`       | `KuValue!`  | Parsing rigoroso in un valore dinamico con proprietà indipendente |
| `json.try_parse(text:str)`   | `KuValue!`  | Alias di compatibilità di `parse`                                 |
| `json.encode(&value:any)`    | `str!`      | Alias di `stringify` nel ramo di sviluppo                         |
| `json.decode(text:str)`      | `KuValue!`  | Alias di `parse` nel ramo di sviluppo                             |

`!` indica Result. Usa `?` per propagare errori oppure `try/catch` per gestirli. La codifica non consuma né modifica l'input.

## Codifica e leggi valori

Questo è un esempio di base completo. Il ramo di sviluppo contiene un esempio di regressione più ampio in [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
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

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Errori

Gli errori recuperabili usano il dominio `json`. I codici comuni sono `parse_error`, `stringify_error` e `out_of_memory`. Il `byte N` in un messaggio di parsing è un offset in byte UTF-8 con base zero. Vedi [errori e risoluzione dei problemi](/it/docs/guide/errors/) per un gestore minimo di input non valido.

## Supporto dei backend

L'interprete, il runner impacchettato predefinito e il backend C nativo sincrono hanno implementazioni JSON. **Il sottoinsieme Task nativo ristretto e LLVM non supportano JSON.** Le importazioni del runner predefinito dipendono ancora dalle posizioni dei sorgenti; il supporto JSON non implica un pacchetto totalmente indipendente.

Le firme e i limiti di questa pagina sono stati verificati rispetto a sorgenti e contratti. La verifica dell'esecuzione deve indicare versione, commit e backend. Consulta il [contratto completo della sintassi JSON](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md#127-json).
