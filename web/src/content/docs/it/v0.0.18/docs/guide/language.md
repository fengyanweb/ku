---
title: 'Guida al linguaggio'
description: 'Impara variabili, funzioni, flusso di controllo, proprietà, moduli ed errori recuperabili di Ku.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
    'docs/v0.0.18-r1.md',
  ]
slug: it/v0.0.18/docs/guide/language
---

Questa guida introduce la sintassi essenziale. Il [riferimento sintattico](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) e il [riferimento semantico](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md) del repository rimangono autorevoli. Sono frammenti didattici; per le differenze consulta il [supporto dei backend](/it/v0.0.18/docs/support/).

## Variabili e tipi

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

La prima assegnazione dichiara una variabile. I tipi primitivi sono `int`, `float`, `bool`, `str` e `null`. Gli interi hanno segno e 64 bit; i valori in virgola mobile usano binary64. Gli interi non diventano condizioni attraverso conversioni implicite a valori veri.

## Funzioni e flusso di controllo

Questo esempio di base completo proviene da `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Output atteso: `30`. Questo frammento di flusso di controllo può essere inserito in `main`:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` itera da `0` a `4` inclusi. `if` e `while` richiedono condizioni `bool`. Usa parentesi graffe per più istruzioni. Le espressioni di diramazione usano `match`; non esiste `switch`.

<a id="ownership"></a>

## Proprietà e prestito in sola lettura

I valori Owned vengono spostati per impostazione predefinita e rilasciati deterministicamente al termine dell’ambito proprietario. Le copie indipendenti richiedono `.clone()`. Ku non introduce un GC globale a tracciamento; strutture interne condivise come gli ambienti delle closure possono usare conteggio dei riferimenti e avere costi runtime. Il prestito sincrono in sola lettura è dichiarato con `&name: T`. Un parametro Owned ordinario può spostare il valore anche quando lo legge soltanto. La chiamata resta `length(value)`, senza `&value`; i prestiti async non sono supportati.

`int`, `float`, `bool` e `null` sono valori Copy. Stringhe, array, oggetti, struct, enum, Results e valori di funzione sono Owned. Le assegnazioni e gli argomenti ordinari spostano i valori posseduti per impostazione predefinita. Usa `.clone()` quando serve un valore indipendente. I Task non possono essere clonati.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Una funzione sincrona può dichiarare un parametro preso in prestito in sola lettura:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Chiamala come `length(value)`, senza `&` sull'argomento. I dati presi in prestito non possono essere modificati, spostati fuori o uscire dall'ambito della chiamata. Combinazioni come i parametri async presi in prestito restano non supportate. Il prestito in sola lettura non garantisce la purezza di qualsiasi funzione.

## Struct, oggetti e moduli

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Gli oggetti dinamici usano `{ name: "Ku" }`. Un accesso normale a un indice di oggetto assente fallisce. `value["name"]?` gestisce la ricerca tramite Result e restituisce un `KuValue` in caso di successo. Usa `.as_str()?` o `.as_int()?` per ottenere un tipo concreto. Una chiave assente non diventa silenziosamente `null`.

Le importazioni della libreria standard usano percorsi separati da punti, come `import json from "std.json"`. I moduli locali possono usare `import math from "./math"`. Le esportazioni di primo livello definite dall'utente iniziano con una lettera maiuscola ASCII; i nomi minuscoli restano privati al file.

## Errori recuperabili

Il tipo di ritorno `T!` può restituire T oppure Error. `?` propaga un errore recuperabile; `fail` ne crea uno:

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error contiene i campi `domain`, `code` e `message`. Un `catch` ordinario non intercetta `panic`, annullamenti di Task o timeout. La pulizia per annullamento in finally condivide un budget con scadenza assoluta; non garantisce il completamento di qualsiasi pulizia definita dall'utente.

## Limiti della concorrenza

Il codice applicativo ottiene un handle Task monouso chiamando una `async fn`, quindi usa `await`. Non esistono API applicative chiamate `task.spawn`, `Task.new` o `thread.spawn`. L'interprete e il sottoinsieme Task nativo hanno coperture diverse. Un esempio funzionante con un backend non dimostra il supporto completo in un altro. Consulta il [supporto di piattaforme e backend](/it/v0.0.18/docs/support/).
