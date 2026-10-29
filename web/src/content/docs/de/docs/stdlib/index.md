---
title: 'Standardbibliothek'
description: 'Finden Sie Ku-Standardbibliotheksmodule, grundlegende Signaturen und Backend-Grenzen.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
    'docs/v0.0.18-r1.md',
  ]
---

Die [Syntaxreferenz](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) beschreibt die Standardbibliothek. Backend-Grenzen stehen in der [Unterstützungsmatrix](/de/docs/support/).

## Module importieren

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` und `net` erfordern explizite Imports. Einige ältere Module lassen sich weiterhin direkt aufrufen; neue Beispiele bevorzugen explizite Imports, um Abhängigkeiten sichtbar zu machen. `std.task` ist ein interner Namensraum für Beobachtung und Stresstests, keine Nebenläufigkeits-API für Anwendungen.

| Modul                                | Zweck                                         | Zu beachtende Grenzen                                                                                           |
| ------------------------------------ | --------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `std.string` / Stringmethoden        | Unicode-Länge, Bytelänge, Suche, Ausschnitte  | Nativem `trim/lower/upper` fehlt eine Unicode-Implementierung                                                   |
| `std.array` / Arraymethoden          | Länge, Suche, neue Arrays, map                | Fehler bei direktem Indexzugriff unterscheiden sich vom behebbaren `try_get`                                    |
| `std.object`                         | Explizite Ersatzwertsuche mit `get_or`        | Ein fehlender gewöhnlicher Index liefert nicht standardmäßig null                                               |
| [std.json](/de/docs/stdlib/json/)    | Strenge JSON-Kodierung und -Dekodierung       | In nativen Tasks und LLVM nicht unterstützt                                                                     |
| `std.fs`                             | Dateien, Existenzprüfungen, Verzeichnislisten | Relative Pfade werden vom Verzeichnis der aktuellen `.ku`-Quelldatei aus aufgelöst                              |
| `std.time`                           | Zeit, Datum, Dauer, Schlafen                  | Synchrone Wartevorgänge und Task-Wartevorgänge verwenden unterschiedliche Ausführungspfade                      |
| `std.http`                           | HTTP-Clients, Dienste, Antworten              | Backend- und Lebenszyklusunterschiede bestehen fort; keine vollständige generische Task-Netzwerkimplementierung |
| `std.config`                         | Umgebung und Konfiguration                    | Keine echten Zugangsdaten im Quelltext committen                                                                |
| `std.pg` / `std.redis` / `std.mysql` | Einheitliche Clients mit begrenzten Pools     | Nur nativ; echte Servervalidierung und TLS-Grenzen müssen separat geprüft werden                                |
| `std.bytes` / `std.net`              | Binärdaten, TCP-Client, optionales TLS        | Generierter nativer C-Pfad; TLS erfordert ein passendes Zielpaket                                               |
| `std.lexer` / `std.parser`           | Compiler-Debugginghilfen                      | Geben Debug-Zusammenfassungen zurück, keine vollständigen öffentlichen Token-/AST-Strukturen                    |

## Häufige Signaturen

Dies sind Dokumentationssignaturen. `&` bezeichnet einen unterstützten schreibgeschützten Parametermodus; `T!` bezeichnet ein behebbares Ergebnis:

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

`"中😀".len()` ergibt 2 und `.byte_len()` ergibt 7. Die Länge zählt Unicode-Skalarwerte; Ausschnitte verwenden Zeichenindizes statt UTF-8-Bytes. `array.push` und `array.concat` geben neue Arrays zurück, ohne ihre Eingaben zu verändern.

## Eine Datei lesen

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Erstellen Sie zuerst eine UTF-8-Datei `message.txt` neben dem Quelltext. Eine fehlende Datei oder ein fehlgeschlagener Lesevorgang gibt einen Fehler zurück. Dieses Beispiel hängt von dieser Eingabedatei ab. `try_read` und `try_write` bleiben kompatible Aliasse mit denselben Ergebnistypen.

`read_dir` gibt sortierte Pfade mit unabhängigem Besitz zurück, begrenzt auf 10.000 Einträge und insgesamt 1.000.000 Pfadbytes. Eingeschränkte native Task-Dateioperationen verwenden einen festen Hintergrundpool. Das belegt kein generisches Warten auf Netzwerkvorgänge über denselben Mechanismus.
