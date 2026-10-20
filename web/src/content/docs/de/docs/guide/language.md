---
title: 'Sprachleitfaden'
description: 'Lernen Sie Ku-Variablen, Funktionen, Kontrollfluss, Ownership, Module und behebbare Fehler kennen.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
---

Diese Dokumentation beschreibt den eingefrorenen Umfang der experimentellen Vorabversion v0.0.18. Der tatsächliche Veröffentlichungsstatus und die Downloads stehen in GitHub Releases. Dieser Leitfaden führt die grundlegende Syntax ein. Maßgeblich bleiben der [Syntaxvertrag](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) und der [Semantikvertrag](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) des Repositorys. Die Fragmente dienen dem Lernen; Unterschiede stehen unter [Backend-Unterstützung](/de/docs/support/).

## Variablen und Typen

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

Die erste Zuweisung deklariert eine Variable. Primitive Typen sind `int`, `float`, `bool`, `str` und `null`. Ganzzahlen sind vorzeichenbehaftete 64-Bit-Werte; Gleitkommazahlen verwenden binary64. Ganzzahlen werden nicht durch eine Wahrheitswertkonvertierung zu Bedingungen.

## Funktionen und Kontrollfluss

Dieses vollständige Grundbeispiel stammt aus `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Erwartete Ausgabe: `30`. Dieses Kontrollflussfragment kann in `main` eingefügt werden:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` iteriert von `0` bis einschließlich `4`. `if` und `while` erfordern Bedingungen vom Typ `bool`. Verwenden Sie für mehrere Anweisungen geschweifte Klammern. Verzweigungsausdrücke verwenden `match`; ein `switch` gibt es nicht.

## Ownership und schreibgeschütztes Ausleihen

`int`, `float`, `bool` und `null` sind Copy-Werte. Strings, Arrays, Objekte, Structs, Enums, Results und Funktionswerte sind Owned. Zuweisungen und gewöhnliche Argumente verschieben Owned-Werte standardmäßig. Verwenden Sie `.clone()`, wenn Sie einen unabhängigen Wert benötigen. Tasks können nicht geklont werden.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Eine synchrone Funktion kann einen schreibgeschützt ausgeliehenen Parameter deklarieren:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Rufen Sie sie als `length(value)` auf, ohne `&` am Argument. Ausgeliehene Daten dürfen weder geändert noch herausverschoben werden oder den Aufruf überleben. Kombinationen wie ausgeliehene async-Parameter werden weiterhin nicht unterstützt. Schreibgeschütztes Ausleihen garantiert keine allgemeine Reinheit von Funktionen.

## Structs, Objekte und Module

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Dynamische Objekte verwenden `{ name: "Ku" }`. Ein gewöhnlicher Zugriff auf einen fehlenden Objektindex schlägt fehl. `value["name"]?` verarbeitet die Suche über Result und liefert bei Erfolg einen `KuValue`. Verwenden Sie `.as_str()?` oder `.as_int()?` für einen konkreten Typ. Ein fehlender Schlüssel wird nicht stillschweigend zu `null`.

Standardbibliotheksimporte verwenden Pfade mit Punkten, etwa `import json from "std.json"`. Lokale Module können `import math from "./math"` verwenden. Benutzerdefinierte Exporte auf oberster Ebene beginnen mit einem ASCII-Großbuchstaben; kleingeschriebene Namen bleiben dateiintern.

## Behebbare Fehler

Der Rückgabetyp `T!` kann entweder T oder Error zurückgeben. `?` reicht einen behebbaren Fehler weiter; `fail` erzeugt einen:

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

Error besitzt die Felder `domain`, `code` und `message`. Ein gewöhnliches `catch` fängt weder `panic` noch Task-Abbruch oder Timeout ab. Die Bereinigung nach einem Abbruch in finally teilt sich ein absolutes Zeitbudget; sie garantiert nicht den Abschluss beliebiger benutzerdefinierter Bereinigungen.

## Nebenläufigkeitsgrenzen

Anwendungscode erhält durch den Aufruf einer `async fn` ein einmal verwendbares Task-Handle und verwendet anschließend `await`. Anwendungs-APIs namens `task.spawn`, `Task.new` oder `thread.spawn` gibt es nicht. Interpreter und native Task-Teilmenge decken unterschiedliche Bereiche ab. Ein Beispiel, das mit einem Backend funktioniert, beweist keine vollständige Unterstützung in einem anderen. Siehe [Plattform- und Backend-Unterstützung](/de/docs/support/).
