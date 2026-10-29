---
title: 'JSON-Referenz'
description: 'JSON-Kodierung und -Dekodierung in Ku, KuValue-Konvertierung, Fehlercodes und Backend-Unterstützung.'
kuVersion: '0.0.19'
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
---

`std.json` kodiert und dekodiert JSON. `json.encode` leiht die Eingabe schreibgeschützt; `json.decode` erzeugt einen unabhängig besitzenden `KuValue`. Beide liefern Result. `stringify`, `parse` und `try_parse` bleiben kompatible Namen.

## Import und Schnittstellen

```ku
import json from "std.json"
```

| Schnittstelle                | Rückgabe   | Bedeutung                                                         |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Schreibgeschützte Kodierung in neuen kompakten JSON-Text          |
| `json.parse(text:str)`       | `KuValue!` | Strenges Parsen in einen dynamischen Wert mit unabhängigem Besitz |
| `json.try_parse(text:str)`   | `KuValue!` | Kompatibilitätsalias für `parse`                                  |
| `json.encode(&value:any)`    | `str!`     | In dieser Version enthaltenes Alias für `stringify`               |
| `json.decode(text:str)`      | `KuValue!` | In dieser Version enthaltenes Alias für `parse`                   |

`!` bezeichnet Result. Verwenden Sie `?` zum Weiterreichen von Fehlern oder `try/catch` zu ihrer Behandlung. Die Kodierung verbraucht oder verändert die Eingabe nicht.

## Werte kodieren und lesen

Vollständiges Grundbeispiel; weitere Anwendungen unter [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku).

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

Objektschlüssel werden deterministisch sortiert. Erwartete Ausgabe:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Ein geparster `KuValue` kann nicht direkt an Ganzzahlarithmetik teilnehmen. Konvertieren Sie zuerst mit `.as_int()?` oder `.as_str()?`. Ein unpassender Typ gibt einen Fehler zurück. Auch fehlende Objektschlüssel liefern Fehler statt automatischer Standardwerte.

## Daten und Grenzen

Unterstützte Eingaben sind Strings, boolesche Werte, Ganzzahlen, endliche Gleitkommazahlen, null, verschachtelte Arrays, Objekte mit Stringschlüsseln und darstellbare Structs. Funktionen, Tasks, Ressourcen-Handles, Results, Enums und Zyklen können nicht kodiert werden.

| Regel                    | Aktuelle Implementierung                                                                                     |
| ------------------------ | ------------------------------------------------------------------------------------------------------------ |
| JSON-Syntax              | Streng; lehnt Kommentare, nachgestellte Kommas, zusätzlichen nachgestellten Inhalt und ungültiges Unicode ab |
| Eingabe / Ausgabe        | Jeweils auf 1.000.000 Bytes begrenzt                                                                         |
| Tiefenbudget             | Wurzeltiefe ist 0; lehnt `depth > 32` ab; 33 Ebenen leerer Container werden akzeptiert, 34 abgelehnt         |
| Ganzzahlen               | Vorzeichenbehaftete 64-Bit-Werte; Überlauf ist ein Fehler, ohne automatische Float-Konvertierung             |
| Gleitkomma               | Endliches binary64; ursprüngliche Schreibweise, Float-Kennzeichnung und negatives Null werden nicht erhalten |
| Unicode                  | Chinesisch, Emojis, Escape-Sequenzen, eingebettetes NUL                                                      |
| Doppelte Objektschlüssel | Beim Parsen bleibt der letzte Wert erhalten                                                                  |
| Schlüsselreihenfolge     | Die Kodierung sortiert nach UTF-8-Bytereihenfolge                                                            |

`1.0` kann als `1` kodiert werden. Bei ganzzahlig aussehendem Float außerhalb int64 ergänzt der Encoder `.0`: `1e20` wird `100000000000000000000.0` und lässt sich wieder dekodieren. Endliche binary64-Werte bewahren weder Originalschreibweise noch Float-Markierung oder negatives Nullvorzeichen. Diese Korrektur gehört zur Version.

## Fehler

Behebbare Fehler verwenden die Domäne `json`. Häufige Codes sind `parse_error`, `stringify_error` und `out_of_memory`. `byte N` in einer Parsemeldung ist ein nullbasierter UTF-8-Byteoffset. Ein minimaler Handler für ungültige Eingaben steht unter [Fehler und Fehlerbehebung](/de/docs/guide/errors/).

## Backend-Unterstützung

Interpreter, standardmäßig verpackter Runner und synchrones natives C-Backend besitzen JSON-Implementierungen. **Die eingeschränkte native Task-Teilmenge und LLVM unterstützen JSON nicht.** Imports des Standard-Runners hängen weiterhin von ihren Quellorten ab; JSON-Unterstützung bedeutet kein vollständig unabhängiges Paket.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json).
