---
title: 'Referință JSON'
description: 'Codificare și decodificare JSON Ku, conversie KuValue, coduri de eroare și suportul backendurilor.'
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
slug: ro/v0.0.18/docs/stdlib/json
---

`std.json` codifică și decodifică JSON. `json.encode` împrumută intrarea doar pentru citire; `json.decode` creează un `KuValue` cu proprietate independentă. Ambele returnează Result. `stringify`, `parse` și `try_parse` rămân compatibile.

## Import și interfețe

```ku
import json from "std.json"
```

| Interfață                    | Returnează | Semnificație                                                 |
| ---------------------------- | ---------- | ------------------------------------------------------------ |
| `json.stringify(&value:any)` | `str!`     | Codificare doar pentru citire într-un nou text JSON compact  |
| `json.parse(text:str)`       | `KuValue!` | Parsare strictă într-o valoare dinamică deținută independent |
| `json.try_parse(text:str)`   | `KuValue!` | Alias compatibil pentru `parse`                              |
| `json.encode(&value:any)`    | `str!`     | Alias pentru `stringify` inclus în această versiune          |
| `json.decode(text:str)`      | `KuValue!` | Alias pentru `parse` inclus în această versiune              |

`!` indică Result. Folosește `?` pentru propagare sau `try/catch` pentru gestionarea erorilor. Codificarea nu consumă și nu modifică intrarea.

## Codificarea și citirea valorilor

Exemplu complet de bază; alte utilizări în [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

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

Cheile obiectelor sunt sortate determinist. Ieșirea așteptată:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Un `KuValue` parsat nu poate participa direct la aritmetică întreagă. Convertește mai întâi cu `.as_int()?` sau `.as_str()?`. Un tip nepotrivit returnează eroare. Cheile de obiect absente returnează și ele erori, nu valori implicite automate.

## Date și limite

Intrările suportate includ șiruri, valori booleene, întregi, numere finite în virgulă mobilă, null, tablouri imbricate, obiecte cu chei șir și structuri reprezentabile. Funcțiile, Tasks, handle-urile de resurse, Results, enumerările și ciclurile nu pot fi codificate.

| Regulă                   | Implementarea curentă                                                                                 |
| ------------------------ | ----------------------------------------------------------------------------------------------------- |
| Sintaxă JSON             | Strictă; respinge comentarii, virgule finale, conținut suplimentar după valoare și Unicode nevalid    |
| Intrare / ieșire         | Fiecare limitată la 1.000.000 de octeți                                                               |
| Buget de adâncime        | Rădăcina are adâncimea 0; respinge `depth > 32`; acceptă 33 niveluri de containere goale, respinge 34 |
| Întregi                  | 64 de biți cu semn; depășirea este eroare, fără conversie automată la float                           |
| Virgulă mobilă           | binary64 finit; scrierea originală, etichetele float și zero negativ nu sunt păstrate                 |
| Unicode                  | Chineză, emoji, secvențe escape, NUL încorporat                                                       |
| Chei de obiect duplicate | Parsarea păstrează ultima valoare                                                                     |
| Ordinea cheilor          | Codificarea sortează după ordinea octeților UTF-8                                                     |

`1.0` se poate codifica drept `1`. Dacă textul întreg al unui float depășește int64, codificatorul adaugă `.0`: `1e20` devine `100000000000000000000.0` și se decodează din nou. binary64 finit nu păstrează scrierea originală, eticheta float sau semnul lui zero negativ. Corecția este inclusă.

## Erori

Erorile recuperabile folosesc domeniul `json`. Codurile comune sunt `parse_error`, `stringify_error` și `out_of_memory`. `byte N` dintr-un mesaj de parsare este un decalaj în octeți UTF-8 pornind de la zero. Vezi [erori și depanare](/ro/v0.0.18/docs/guide/errors/) pentru un gestionar minim al intrărilor nevalide.

## Suportul backendurilor

Interpretorul, lansatorul împachetat implicit și backendul C nativ sincron au implementări JSON. **Submulțimea nativă Task restrânsă și LLVM nu suportă JSON.** Importurile lansatorului implicit încă depind de locațiile surselor; suportul JSON nu implică un pachet complet independent.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
