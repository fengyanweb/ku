---
title: 'Referință JSON'
description: 'Codificare și decodificare JSON Ku, conversie KuValue, coduri de eroare și suportul backendurilor.'
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

> Actualizările v0.0.18 de mai jos sunt momentan în engleză; restul traducerii este păstrat. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## Import și interfețe

```ku
import json from "std.json"
```

| Interfață                    | Returnează | Semnificație                                                 |
| ---------------------------- | ---------- | ------------------------------------------------------------ |
| `json.stringify(&value:any)` | `str!`     | Codificare doar pentru citire într-un nou text JSON compact  |
| `json.parse(text:str)`       | `KuValue!` | Parsare strictă într-o valoare dinamică deținută independent |
| `json.try_parse(text:str)`   | `KuValue!` | Alias compatibil pentru `parse`                              |
| `json.encode(&value:any)`    | `str!`     | Alias de dezvoltare pentru `stringify`                       |
| `json.decode(text:str)`      | `KuValue!` | Alias de dezvoltare pentru `parse`                           |

`!` indică Result. Folosește `?` pentru propagare sau `try/catch` pentru gestionarea erorilor. Codificarea nu consumă și nu modifică intrarea.

## Codificarea și citirea valorilor

Acesta este un exemplu complet de bază. Ramura de dezvoltare are un exemplu de regresie mai amplu în [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18/examples/json.ku).

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

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Erori

Erorile recuperabile folosesc domeniul `json`. Codurile comune sunt `parse_error`, `stringify_error` și `out_of_memory`. `byte N` dintr-un mesaj de parsare este un decalaj în octeți UTF-8 pornind de la zero. Vezi [erori și depanare](/ro/docs/guide/errors/) pentru un gestionar minim al intrărilor nevalide.

## Suportul backendurilor

Interpretorul, lansatorul împachetat implicit și backendul C nativ sincron au implementări JSON. **Submulțimea nativă Task restrânsă și LLVM nu suportă JSON.** Importurile lansatorului implicit încă depind de locațiile surselor; suportul JSON nu implică un pachet complet independent.

Semnăturile și limitele paginii au fost verificate în surse și contracte. Verificarea execuției trebuie să numească versiunea, commitul și backendul. Vezi [contractul complet de sintaxă JSON](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md#127-json).
