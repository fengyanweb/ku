---
title: 'Ghidul limbajului'
description: 'Învață variabilele, funcțiile, controlul execuției, proprietatea, modulele și erorile recuperabile Ku.'
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
slug: ro/v0.0.18/docs/guide/language
---

Ghidul introduce sintaxa de bază. [Referința de sintaxă](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) și [referința semantică](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md) ale depozitului rămân normative. Acestea sunt fragmente didactice; diferențele sunt descrise la [suportul backendurilor](/ro/v0.0.18/docs/support/).

## Variabile și tipuri

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

Prima atribuire declară o variabilă. Tipurile primitive sunt `int`, `float`, `bool`, `str` și `null`. Întregii au 64 de biți cu semn; numerele în virgulă mobilă folosesc binary64. Întregii nu devin condiții prin conversie implicită la adevăr.

## Funcții și controlul execuției

Acest exemplu complet de bază provine din `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Ieșire așteptată: `30`. Acest fragment de control al execuției poate fi pus în `main`:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` iterează de la `0` la `4`. `if` și `while` necesită condiții `bool`. Folosește acolade pentru mai multe instrucțiuni. Expresiile de ramificare folosesc `match`; nu există `switch`.

<a id="ownership"></a>

## Proprietate și împrumut doar pentru citire

Valorile Owned se mută implicit și sunt eliberate determinist la ieșirea din domeniul proprietar. Copiile independente cer `.clone()`. Ku nu introduce un GC global cu urmărire; structurile interne partajate, ca mediile închiderilor, pot folosi numărarea referințelor și au costuri de execuție. Împrumutul sincron doar pentru citire se declară prin `&name: T`. Un parametru Owned obișnuit poate muta valoarea chiar dacă doar o citește. Apelul rămâne `length(value)`, fără `&value`; împrumuturile async nu sunt suportate.

`int`, `float`, `bool` și `null` sunt valori Copy. Șirurile, tablourile, obiectele, structurile, enumerările, Results și valorile funcție sunt Owned. Atribuirea și argumentele obișnuite mută implicit valorile deținute. Folosește `.clone()` când ai nevoie de o valoare independentă. Tasks nu pot fi clonate.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

O funcție sincronă poate declara un parametru împrumutat doar pentru citire:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Apeleaz-o ca `length(value)`, fără `&` pe argument. Datele împrumutate nu pot fi modificate, mutate în afară sau lăsate să depășească apelul. Combinații precum parametrii async împrumutați nu sunt încă suportate. Împrumutul doar pentru citire nu garantează puritatea arbitrară a funcției.

## Structuri, obiecte și module

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Obiectele dinamice folosesc `{ name: "Ku" }`. Un index obișnuit de obiect absent produce o eroare. `value["name"]?` gestionează căutarea prin Result, returnând un `KuValue` la succes. Folosește `.as_str()?` sau `.as_int()?` pentru un tip concret. O cheie absentă nu devine în mod tăcut `null`.

Importurile bibliotecii standard folosesc căi cu puncte, precum `import json from "std.json"`. Modulele locale pot folosi `import math from "./math"`. Exporturile de nivel superior definite de utilizator încep cu o majusculă ASCII; numele cu literă mică rămân private fișierului.

## Erori recuperabile

Tipul returnat `T!` poate returna T sau Error. `?` propagă o eroare recuperabilă; `fail` creează una:

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

Error are câmpurile `domain`, `code` și `message`. Un `catch` obișnuit nu prinde `panic`, anularea Task sau depășirea timpului. Curățarea din finally la anulare împarte bugetul unui termen absolut; nu garantează finalizarea oricărei curățări a utilizatorului.

## Limitele concurenței

Codul aplicației obține un handle Task de unică folosință apelând un `async fn`, apoi folosește `await`. Nu există API-uri de aplicație numite `task.spawn`, `Task.new` sau `thread.spawn`. Interpretorul și submulțimea nativă Task au acoperire diferită. Un exemplu funcțional într-un backend nu dovedește suport complet în altul. Vezi [suportul platformelor și backendurilor](/ro/v0.0.18/docs/support/).
