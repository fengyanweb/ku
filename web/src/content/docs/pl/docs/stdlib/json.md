---
title: 'Dokumentacja JSON'
description: 'Kodowanie i dekodowanie JSON w Ku, konwersja KuValue, kody błędów i obsługa zapleczy.'
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

`std.json` koduje i dekoduje JSON. `json.encode` pożycza wejście tylko do odczytu; `json.decode` tworzy niezależnie posiadający dane `KuValue`. Oba zwracają Result. `stringify`, `parse` i `try_parse` pozostają zgodne.

## Import i interfejsy

```ku
import json from "std.json"
```

| Interfejs                    | Zwraca     | Znaczenie                                                         |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Kodowanie tylko do odczytu do nowego zwartego tekstu JSON         |
| `json.parse(text:str)`       | `KuValue!` | Ścisłe parsowanie do dynamicznej wartości o niezależnej własności |
| `json.try_parse(text:str)`   | `KuValue!` | Zgodny alias `parse`                                              |
| `json.encode(&value:any)`    | `str!`     | Alias `stringify` zawarty w tym wydaniu                           |
| `json.decode(text:str)`      | `KuValue!` | Alias `parse` zawarty w tym wydaniu                               |

`!` oznacza Result. Użyj `?` do propagowania błędów lub `try/catch` do ich obsługi. Kodowanie nie zużywa ani nie zmienia wejścia.

## Kodowanie i odczyt wartości

Pełny przykład podstawowy; więcej zastosowań w [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku).

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

Klucze obiektu są sortowane deterministycznie. Oczekiwany wynik:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Sparsowany `KuValue` nie może bezpośrednio uczestniczyć w arytmetyce całkowitej. Najpierw użyj `.as_int()?` lub `.as_str()?`. Niezgodny typ zwraca błąd. Brakujące klucze obiektu również zwracają błędy zamiast automatycznych wartości domyślnych.

## Dane i granice

Obsługiwane wejście obejmuje łańcuchy, wartości logiczne, liczby całkowite, skończone liczby zmiennoprzecinkowe, null, zagnieżdżone tablice, obiekty z kluczami tekstowymi i reprezentowalne struktury. Nie można kodować funkcji, Tasks, uchwytów zasobów, Results, wyliczeń ani cykli.

| Reguła                    | Bieżąca implementacja                                                                                  |
| ------------------------- | ------------------------------------------------------------------------------------------------------ |
| Składnia JSON             | Ścisła; odrzuca komentarze, końcowe przecinki, dodatkowy tekst po wartości i niepoprawny Unicode       |
| Wejście / wyjście         | Każde ograniczone do 1 000 000 bajtów                                                                  |
| Budżet głębokości         | Głębokość korzenia wynosi 0; odrzuca `depth > 32`; przyjmuje 33 poziomy pustych kontenerów, odrzuca 34 |
| Liczby całkowite          | 64-bitowe ze znakiem; przepełnienie jest błędem, bez automatycznej konwersji na float                  |
| Zmienny przecinek         | Skończone binary64; oryginalny zapis, znaczniki float i ujemne zero nie są zachowywane                 |
| Unicode                   | Tekst chiński, emoji, sekwencje ucieczki, osadzony NUL                                                 |
| Powtórzone klucze obiektu | Parsowanie zachowuje ostatnią wartość                                                                  |
| Kolejność kluczy          | Kodowanie sortuje według kolejności bajtów UTF-8                                                       |

`1.0` może być kodowane jako `1`. Gdy całkowity zapis float przekracza int64, koder dodaje `.0`: `1e20` staje się `100000000000000000000.0` i można je ponownie zdekodować. Skończone binary64 nie zachowują zapisu źródłowego, znacznika float ani znaku ujemnego zera. Poprawka jest w tym wydaniu.

## Błędy

Obsługiwalne błędy używają domeny `json`. Typowe kody to `parse_error`, `stringify_error` i `out_of_memory`. `byte N` w komunikacie parsowania jest przesunięciem bajtowym UTF-8 od zera. Minimalną obsługę niepoprawnego wejścia pokazują [błędy i rozwiązywanie problemów](/pl/docs/guide/errors/).

## Obsługa zapleczy

Interpreter, domyślny pakowany program uruchamiający i synchroniczne natywne zaplecze C mają implementacje JSON. **Ograniczony natywny podzbiór Task i LLVM nie obsługują JSON.** Importy domyślnego programu uruchamiającego nadal zależą od położenia źródeł; obsługa JSON nie oznacza w pełni niezależnego pakietu.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json).
