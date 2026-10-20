---
title: 'Przewodnik po języku'
description: 'Poznaj zmienne, funkcje, przepływ sterowania, własność, moduły i obsługiwalne błędy Ku.'
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

Dokumentacja opisuje zamrożony zakres eksperymentalnego wydania wstępnego v0.0.18. Faktyczna publikacja i pliki do pobrania są zapisane w GitHub Releases. Przewodnik wprowadza podstawową składnię. Nadrzędne pozostają [kontrakt składni](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) i [kontrakt semantyki](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) repozytorium. To fragmenty dydaktyczne; różnice opisuje [obsługa zapleczy](/pl/docs/support/).

## Zmienne i typy

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

Pierwsze przypisanie deklaruje zmienną. Typy proste to `int`, `float`, `bool`, `str` i `null`. Liczby całkowite są 64-bitowe ze znakiem; zmiennoprzecinkowe używają binary64. Liczby całkowite nie stają się warunkami przez niejawną konwersję na prawdziwość.

## Funkcje i przepływ sterowania

Ten kompletny podstawowy przykład pochodzi z `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Oczekiwany wynik: `30`. Ten fragment przepływu sterowania można umieścić wewnątrz `main`:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` iteruje od `0` do `4`. `if` i `while` wymagają warunków typu `bool`. Dla wielu instrukcji użyj klamer. Wyrażenia rozgałęziające używają `match`; nie ma `switch`.

## Własność i pożyczanie tylko do odczytu

`int`, `float`, `bool` i `null` są wartościami Copy. Łańcuchy, tablice, obiekty, struktury, wyliczenia, Results i wartości funkcji są Owned. Przypisanie i zwykłe argumenty domyślnie przenoszą wartości z własnością. Gdy potrzebujesz niezależnej wartości, użyj `.clone()`. Tasks nie można klonować.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Funkcja synchroniczna może zadeklarować pożyczony parametr tylko do odczytu:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Wywołuj ją jako `length(value)`, bez `&` przy argumencie. Pożyczonych danych nie można modyfikować, przenosić z nich wartości ani pozwalać im wyjść poza wywołanie. Połączenia takie jak pożyczone parametry async pozostają nieobsługiwane. Pożyczanie tylko do odczytu nie gwarantuje dowolnej czystości funkcji.

## Struktury, obiekty i moduły

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Obiekty dynamiczne używają `{ name: "Ku" }`. Zwykły dostęp do brakującego indeksu obiektu kończy się błędem. `value["name"]?` obsługuje wyszukiwanie przez Result, zwracając `KuValue` przy powodzeniu. Dla konkretnego typu użyj `.as_str()?` lub `.as_int()?`. Brakujący klucz nie staje się po cichu `null`.

Importy biblioteki standardowej używają ścieżek z kropkami, np. `import json from "std.json"`. Moduły lokalne mogą używać `import math from "./math"`. Eksporty najwyższego poziomu definiowane przez użytkownika zaczynają się wielką literą ASCII; nazwy małymi literami pozostają prywatne w pliku.

## Obsługiwalne błędy

Typ zwracany `T!` może zwrócić T lub Error. `?` propaguje obsługiwalny błąd; `fail` go tworzy:

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

Error ma pola `domain`, `code` i `message`. Zwykłe `catch` nie przechwytuje `panic`, anulowania Task ani przekroczenia czasu. Sprzątanie podczas anulowania w finally współdzieli budżet bezwzględnego terminu; nie gwarantuje zakończenia dowolnego sprzątania użytkownika.

## Granice współbieżności

Kod aplikacji uzyskuje jednorazowy uchwyt Task przez wywołanie `async fn`, a następnie używa `await`. Nie ma aplikacyjnych API o nazwach `task.spawn`, `Task.new` ani `thread.spawn`. Interpreter i natywny podzbiór Task mają różny zakres obsługi. Przykład działający w jednym zapleczu nie dowodzi pełnej obsługi w innym. Zobacz [obsługę platform i zapleczy](/pl/docs/support/).
